#Requires -Version 7
<#
.SYNOPSIS
    A/B test classifier system prompt variants against the smoke-test case set.
.DESCRIPTION
    Sends each classification probe to the classifier endpoint using two
    named prompt variants (A = current baseline, B = candidate) and reports
    which variant routes each case correctly.

    The classifier endpoint (/v1/chat/completions model=ha-auto:instant:classify)
    returns a JSON response; we extract the text and compare it against the
    expected label.

    Outputs:
      - Per-case table: prompt text, expected label, variant-A result, variant-B result
      - Score summary: A total vs B total, per-label breakdown, net delta
      - Cases where A and B disagree (the interesting ones)

.PARAMETER Base
    Gateway base URL. Defaults to $env:LM_GATEWAY_URL, then http://localhost:8080.
.PARAMETER TimeoutSec
    Per-request HTTP timeout. Default: 60.
.EXAMPLE
    .\Invoke-ClassifyAB.ps1
.EXAMPLE
    .\Invoke-ClassifyAB.ps1 -Base http://localhost:8080
.NOTES
    Add new variants to the $Variants hashtable below. Keep variant "A" as the
    current production baseline so delta is always meaningful.
#>
[CmdletBinding()]
param(
    [string]$Base       = ($env:LM_GATEWAY_URL ?? 'http://localhost:8080'),
    [string]$ClientKey  = $env:LMG_CLIENT_KEY,
    [int]   $TimeoutSec = 60
)
$ErrorActionPreference = 'Stop'

# ---------------------------------------------------------------------------
# Test cases — label + user text.  Same 13-case set as smoke test.
# ---------------------------------------------------------------------------

$Cases = @(
    @{ Id='1';  Expected='greeting';      Text="Good morning!" }
    @{ Id='2';  Expected='greeting';      Text="Hey, what's up?" }
    @{ Id='3';  Expected='chitchat';      Text="Tell me a joke" }
    @{ Id='4';  Expected='chitchat';      Text="What's the capital of Brazil?" }
    @{ Id='5';  Expected='command';       Text="Turn on the kitchen light" }
    @{ Id='6';  Expected='command';       Text="Lock the front door" }
    @{ Id='7';  Expected='command';       Text="Turn off all the lights" }
    @{ Id='8';  Expected='command';       Text="Turn on the light" }
    @{ Id='9';  Expected='command';       Text="Lock up" }
    @{ Id='10'; Expected='conversation';  Text="The kitchen one" }
    @{ Id='11'; Expected='conversation';  Text="Yes" }
    @{ Id='12'; Expected='inquiry';       Text="Is the front door locked?" }
    @{ Id='13'; Expected='inquiry';       Text="Are all the lights off?" }
)

# ---------------------------------------------------------------------------
# Variants — add candidate variants here.
# Keep the labels: the keys here are used verbatim in output column headers.
# ---------------------------------------------------------------------------

$Variants = [ordered]@{

    'A(current)' = @"
Classify the intent of the following smart home assistant message.
Reply with ONLY one of these labels:
  greeting     - a greeting or farewell (Good morning, Goodnight, etc.)
  chitchat     - casual conversation, jokes, or general knowledge questions
  command      - a request to control a device or area (even vague commands like "turn on the light")
  conversation - a fragment that is part of an ongoing conversation, not a standalone request
  inquiry      - a question about the current state of a device or entity
  other        - anything that doesn't fit the above
Reply with exactly one label, no explanation, no punctuation.
"@

    'B(brief)'   = @"
Label the user message. Reply with ONE word only:
  greeting     — greetings and farewells
  chitchat     — casual conversation or general-knowledge questions
  command      — control a device or area (including vague commands)
  conversation — fragment of an ongoing conversation
  inquiry      — question about current device / entity state
  other        — anything else
One word. No punctuation.
"@

    'C(examples)' = @"
Classify this smart home assistant message into exactly one of these labels.
Reply with only the label, no explanation.

greeting     : "Good morning", "Goodnight", "Hey what's up"
chitchat     : "Tell me a joke", "What's the capital of Brazil", "How far is the moon"
command      : "Turn on the light", "Lock the front door", "Turn off everything", "Lock up"
conversation : "The kitchen one", "Yes", "That one" (fragment needing prior context)
inquiry      : "Is the front door locked?", "Are all lights off?", "What is the thermostat set to?"
other        : anything not fitting the above
"@

    'D(ha-discriminant)' = @"
Classify this smart home assistant message into exactly one of these labels.
Reply with exactly one label, no explanation.

Key rule: if the question or statement has NO relation to smart home devices,
areas, entities, or automation — classify it as chitchat, not inquiry.

greeting     : "Good morning", "Goodnight", "Hey, what's up?", "Have a good night"
chitchat     : "Tell me a joke", "What's the capital of Brazil?", "How far is the moon?" — any topic unrelated to home automation
command      : "Turn on the light", "Lock the front door", "Turn off everything", "Lock up" — control a device or area
conversation : "The kitchen one", "Yes", "That one" — fragment only meaningful with prior context
inquiry      : "Is the front door locked?", "Are all the lights off?", "What is the thermostat set to?" — questions about a home device or entity state
other        : anything not fitting the above
"@

    'E(combo)' = @"
Classify this smart home assistant message into exactly one of these labels.
Reply with exactly one label, no explanation.

Priority rules (apply in order):
1. If it is a social greeting, farewell, or check-in → greeting (even if no HA content).
2. If the topic has NOTHING to do with smart home devices or automation → chitchat (not inquiry).

greeting     : "Good morning", "Goodnight", "Hey, what's up?", "Have a good night", "Hello"
chitchat     : "Tell me a joke", "What's the capital of Brazil?", "How far is the moon?" — topic unrelated to home automation
command      : "Turn on the light", "Lock the front door", "Turn off everything", "Lock up" — control a device or area
conversation : "The kitchen one", "Yes", "That one" — fragment only meaningful with prior context
inquiry      : "Is the front door locked?", "Are all the lights off?", "What is the thermostat set to?" — questions about a home device or entity state
other        : anything not fitting the above
"@
}

# ---------------------------------------------------------------------------
# HTTP helper: send classify probe, return raw label text
# ---------------------------------------------------------------------------

function Invoke-Classify([string]$VariantPrompt, [string]$UserText) {
    $body = @{
        model    = 'ha-auto:instant:classify'
        messages = @(
            @{ role = 'system'; content = $VariantPrompt }
            @{ role = 'user';   content = $UserText }
        )
        stream = $false
    }
    $headers = @{ 'Content-Type' = 'application/json' }
    if ($ClientKey) { $headers['Authorization'] = "Bearer $ClientKey" }

    try {
        $r = Invoke-RestMethod -Method Post -Uri "$Base/v1/chat/completions" `
                -Headers $headers -Body ($body | ConvertTo-Json -Depth 8 -Compress) `
                -TimeoutSec $TimeoutSec
        $raw = $r.choices[0].message.content.Trim().ToLower() -replace '[^a-z]',''
        return $raw
    } catch {
        return 'ERR'
    }
}

# ---------------------------------------------------------------------------
# Run all variants × all cases
# ---------------------------------------------------------------------------

$variantNames = @($Variants.Keys)
$variantCount = $variantNames.Count

Write-Host ''
Write-Host '=== Classifier A/B Probe ===' -ForegroundColor Cyan
Write-Host "  Variants : $($variantNames -join ', ')"
Write-Host "  Cases    : $($Cases.Count)"
Write-Host ''

$rows = [System.Collections.ArrayList]::new()

foreach ($tc in $Cases) {
    $variantResults = @{}
    foreach ($vk in $variantNames) {
        $label = Invoke-Classify -VariantPrompt $Variants[$vk] -UserText $tc.Text
        $variantResults[$vk] = $label
    }
    [void]$rows.Add([PSCustomObject]@{
        Id       = $tc.Id
        Expected = $tc.Expected
        Text     = $tc.Text
        Results  = $variantResults
    })
}

# ---------------------------------------------------------------------------
# Print per-case table
# ---------------------------------------------------------------------------

$hdr = '{0,-4} {1,-14}' -f 'ID','Expected'
foreach ($vk in $variantNames) { $hdr += ' {0,-13}' -f $vk }
$hdr += '  Text'
Write-Host $hdr -ForegroundColor Gray
Write-Host ('-' * 100)

foreach ($row in $rows) {
    $line = '{0,-4} {1,-14}' -f $row.Id, $row.Expected
    $anyDiff = $false
    foreach ($vk in $variantNames) {
        $got   = $row.Results[$vk]
        $match = ($got -eq $row.Expected)
        $sym   = if ($match) { 'PASS' } else { 'MISS' }
        $col   = if ($match) { 'Green' } else { 'Red' }
        if (-not $match) { $anyDiff = $true }
        $line += ' '
        Write-Host $line -NoNewline -ForegroundColor White
        $line = ''
        Write-Host ('{0,-13}' -f "$sym($got)") -NoNewline -ForegroundColor $col
    }
    $txtColor = if ($anyDiff) { 'Yellow' } else { 'DarkGray' }
    $short = if ($row.Text.Length -gt 45) { $row.Text.Substring(0,42) + '...' } else { $row.Text }
    Write-Host "  $short" -ForegroundColor $txtColor
}

Write-Host ('-' * 100)

# ---------------------------------------------------------------------------
# Score summary
# ---------------------------------------------------------------------------

Write-Host ''
Write-Host 'SCORE SUMMARY:' -ForegroundColor Cyan
$baseline = $variantNames[0]
$baseScore = ($rows | Where-Object { $_.Results[$baseline] -eq $_.Expected }).Count

foreach ($vk in $variantNames) {
    $correct = ($rows | Where-Object { $_.Results[$vk] -eq $_.Expected }).Count
    $total   = $rows.Count
    $pct     = [Math]::Round($correct / $total * 100)
    $delta   = $correct - $baseScore
    $deltaStr = if ($vk -eq $baseline) { '(baseline)' } elseif ($delta -gt 0) { "+$delta" } elseif ($delta -lt 0) { "$delta" } else { '0' }
    $col     = if ($correct -ge $baseScore) { 'Green' } else { 'Red' }
    Write-Host ("  {0,-14}: {1,2}/{2} ({3}%)  {4}" -f $vk, $correct, $total, $pct, $deltaStr) -ForegroundColor $col
}

# ---------------------------------------------------------------------------
# Show disagreements (most interesting cases)
# ---------------------------------------------------------------------------

$disagreements = $rows | Where-Object {
    $r = $_
    $scores = $variantNames | ForEach-Object { if ($r.Results[$_] -eq $r.Expected) { 1 } else { 0 } }
    ($scores | Measure-Object -Sum).Sum -lt $variantCount -and
    ($scores | Measure-Object -Sum).Sum -gt 0
}

if ($disagreements) {
    Write-Host ''
    Write-Host 'DISAGREEMENTS (variants differ — review these):' -ForegroundColor Yellow
    foreach ($row in $disagreements) {
        Write-Host ("  [{0}] {1}" -f $row.Id, $row.Text) -ForegroundColor Yellow
        foreach ($vk in $variantNames) {
            $got   = $row.Results[$vk]
            $match = ($got -eq $row.Expected)
            $sym   = if ($match) { '✓' } else { '✗' }
            $col   = if ($match) { 'Green' } else { 'Red' }
            Write-Host ("        {0,-14} → {1} {2}" -f $vk, $got, $sym) -ForegroundColor $col
        }
    }
}

$allWrong = $rows | Where-Object {
    $r = $_
    $scores = $variantNames | ForEach-Object { if ($r.Results[$_] -eq $r.Expected) { 1 } else { 0 } }
    ($scores | Measure-Object -Sum).Sum -eq 0
}

if ($allWrong) {
    Write-Host ''
    Write-Host 'ALL-VARIANTS-MISS (every variant failed — classifier gap):' -ForegroundColor Red
    foreach ($row in $allWrong) {
        Write-Host ("  [{0}] expected={1}  text=`"{2}`"" -f $row.Id, $row.Expected, $row.Text) -ForegroundColor Red
        foreach ($vk in $variantNames) {
            Write-Host ("        {0,-14} → {1}" -f $vk, $row.Results[$vk]) -ForegroundColor DarkRed
        }
    }
}

Write-Host ''
