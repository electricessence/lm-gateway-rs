#Requires -Version 7

# Connection details from environment — set LMG_SSH_HOST and LMG_SSH_KEY,
# or pass as parameters.  Example:
#   $env:LMG_SSH_HOST = 'root@<host>'
#   $env:LMG_SSH_KEY  = "$env:USERPROFILE\.ssh\<keyfile>"
[CmdletBinding()]
param(
    [ValidateSet('current', 'proposed', 'both', 'debug')]
    [string] $Variant   = 'both',
    [string] $SshHost   = $env:LMG_SSH_HOST,
    [string] $SshKey    = $env:LMG_SSH_KEY,
    [string] $OllamaUrl = ($env:LMG_OLLAMA_URL ?? 'http://127.0.0.1:11434/api/chat')
)

$ErrorActionPreference = 'Stop'

if (-not $SshHost) { throw 'Set $env:LMG_SSH_HOST or pass -SshHost' }
if (-not $SshKey)  { throw 'Set $env:LMG_SSH_KEY  or pass -SshKey'  }

$SSH_KEY  = $SshKey
$SSH_HOST = $SshHost
$MODEL    = 'qwen3:1.7b'
$OLLAMA   = $OllamaUrl

$Prompts = @{
    # The current ha-auto production prompt (2-tier: instant | deep)
    current = @"
Route this Home Assistant request.

DEEP — use for any of these:
- Controls a device: light, switch, lock, thermostat, fan, cover, climate, AC, plug, media, speaker
- Action words: turn on/off, set, dim, brighten, open, close, lock, unlock, play, pause, stop, adjust
- Queries home state: is X on/off/open/locked/playing, what is the temperature/status
- Departure or arrival: leaving, I'm home, goodnight, heading out

INSTANT — only if:
- Pure greeting, chitchat, joke request, or entertainment with no home action (hi, tell me a joke)
- General knowledge question unrelated to the home (time, math, news, general facts)

Reply with exactly one word: instant or deep.
"@

    # Proposed 3-tier prompt (instant | fast | deep)
    # fast = single unambiguous device command (routed to 8b instead of 14b)
    # deep = state queries, multi-entity, tool synthesis, ambiguous
    proposed = @"
Route this Home Assistant request. Reply with one word: instant, fast, or deep.

  Turn on the kitchen lights                               -> fast
  Lock the front door                                      -> fast
  Set the thermostat to 72                                 -> fast
  Dim the office light to 40%                              -> fast
  Play music in the bedroom                                -> fast
  Turn off the TV                                          -> fast
  What is the living room temperature?                     -> deep
  What is the temperature outside?                         -> deep
  Are any lights on downstairs?                            -> deep
  Is the garage door open?                                 -> deep
  Turn off all the lights                                  -> deep
  Lock everything up                                       -> deep
  Goodnight                                                -> deep
  Good morning                                             -> deep
  I'm home                                                 -> deep
  I'm leaving                                              -> deep
  Heading out                                              -> deep
  Hi                                                       -> instant
  How are you                                              -> instant
  Tell me a joke                                           -> instant
  What's the capital of France?                            -> instant
  What's 2+2?                                              -> instant

Reply with one word only: instant, fast, or deep.
"@
}

# Each case: Q=query, C=expected for current (2-tier), P=expected for proposed (3-tier)
$Cases = @(
    # Simple device commands → current:deep, proposed:fast (8b is sufficient)
    @{ Q = 'Turn on the kitchen lights';                             C = 'deep'; P = 'fast' }
    @{ Q = 'Lock the front door';                                    C = 'deep'; P = 'fast' }
    @{ Q = 'Turn off all the lights';                                C = 'deep'; P = 'deep' }  # multi-entity → always deep
    @{ Q = 'Dim the office light to 40%';                            C = 'deep'; P = 'fast' }
    @{ Q = 'Set the thermostat to 72';                               C = 'deep'; P = 'fast' }    # State queries — could be fast or deep; fast is acceptable for simple binary queries
    @{ Q = 'What is the temperature outside?';                       C = 'deep'; P = 'deep' }
    @{ Q = 'Are any lights on downstairs?';                          C = 'deep'; P = 'deep' }
    @{ Q = 'What is the living room temperature?';                   C = 'deep'; P = 'deep' }
    @{ Q = 'Is the garage door open?';                               C = 'deep'; P = 'deep' }
    # Arrival/departure — always deep (trigger home scenes)
    @{ Q = 'Goodnight';                                              C = 'deep'; P = 'deep' }
    @{ Q = "I'm leaving";                                            C = 'deep'; P = 'deep' }
    # Non-home — instant
    @{ Q = 'Hi';                                                     C = 'instant'; P = 'instant' }
    @{ Q = 'Tell me a joke';                                         C = 'instant'; P = 'instant' }
    @{ Q = "What's the capital of France?";                          C = 'instant'; P = 'instant' }
)

function Invoke-ClassifierQuery([string]$Prompt, [string]$Query, [switch]$Debug) {
    $body = [ordered]@{
        model    = $MODEL
        messages = @(
            @{ role = 'system'; content = $Prompt }
            @{ role = 'user';   content = $Query  }
        )
        stream  = $false
        think   = $false
        options = @{ num_predict = 10; temperature = 0 }
    } | ConvertTo-Json -Depth 5 -Compress

    # Base64-encode so we don't fight SSH quoting
    $b64 = [Convert]::ToBase64String([Text.Encoding]::UTF8.GetBytes($body))
    $cmd = "pct exec 200 -- bash -c 'echo $b64 | base64 -d | curl -sf -X POST $OLLAMA -H Content-Type:application/json -d @-'"

    $raw = ssh -i $SSH_KEY $SSH_HOST $cmd
    if (-not $raw) { return $null }

    $d = $raw | ConvertFrom-Json
    if ($Debug) {
        Write-Host "  thinking: $($d.message.thinking)" -ForegroundColor DarkGray
        Write-Host "  content:  $($d.message.content)"  -ForegroundColor DarkGray
    }
    $content = ($d.message.content ?? '').Trim().ToLower()
    $label = ($content -split '\s+')[0] -replace '[.,\[\]()"'']+', ''
    return $label
}

function Invoke-Suite([string]$SuiteName, [string]$Prompt, [string]$ExpectedKey) {
    Write-Host "`n=== $SuiteName ===" -ForegroundColor Cyan
    $pass = 0
    foreach ($c in $Cases) {
        $label = Invoke-ClassifierQuery -Prompt $Prompt -Query $c.Q
        $expected = $c[$ExpectedKey]
        $ok = $label -eq $expected
        if ($ok) { $pass++; $status = 'PASS'; $color = 'Green' }
        else      { $status = "FAIL (want $expected)"; $color = 'Red' }
        Write-Host ("  {0,-20} {1,-28} {2}" -f $label, $status, $c.Q) -ForegroundColor $color
    }
    Write-Host "--- $pass/$($Cases.Count) correct ---" -ForegroundColor Cyan
}

if ($Variant -eq 'debug') {
    Write-Host "=== Debug: raw classifier output (current prompt) ===" -ForegroundColor Cyan
    Invoke-ClassifierQuery -Prompt $Prompts['current'] -Query $Cases[0].Q -Debug
    return
}

if ($Variant -eq 'both') {
    Invoke-Suite 'current (2-tier: instant|deep)'        $Prompts['current']  'C'
    Invoke-Suite 'proposed (3-tier: instant|fast|deep)'  $Prompts['proposed'] 'P'
} else {
    $expKey = if ($Variant -eq 'current') { 'C' } else { 'P' }
    Invoke-Suite $Variant $Prompts[$Variant] $expKey
}
