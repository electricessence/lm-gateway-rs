#Requires -Version 7
<#
.SYNOPSIS
    Probe the minimum model tier required for each HA test case.
.DESCRIPTION
    Sends each test case DIRECTLY to each tier (bypassing the classifier) and
    evaluates response quality. Reveals which tier is the true minimum needed
    to produce a useful answer for each question type.

    Quality criteria per label:
      command / inquiry / conversation → needs a tool_call (HassXxx call)
      greeting / chitchat              → needs a non-trivial text response
      other                            → text response, non-empty

    Tier aliases used (model hints sent to gateway):
      instant → local:instant  (qwen3:1.7b, no-think)
      fast    → local:fast     (qwen3:1.7b, with think)
      moderate→ local:moderate (qwen2.5:7b-instruct)
      deep    → local:deep     (qwen3:14b)
.PARAMETER Base
    Gateway base URL. Defaults to $env:LM_GATEWAY_URL, then http://localhost:8080.
.PARAMETER TimeoutSec
    Per-request HTTP timeout. Default: 120.
.PARAMETER Tiers
    Ordered list of tier aliases to probe. Default: instant,fast,moderate,deep
.EXAMPLE
    .\Invoke-TierMinimum.ps1
.EXAMPLE
    .\Invoke-TierMinimum.ps1 -Tiers instant,moderate,deep
#>
[CmdletBinding()]
param(
    [string]  $Base       = ($env:LM_GATEWAY_URL ?? 'http://localhost:8080'),
    [string]  $ClientKey  = $env:LMG_CLIENT_KEY,
    [int]     $TimeoutSec = 120,
    [string[]]$Tiers      = @('instant','fast','moderate','deep')
)
$ErrorActionPreference = 'Stop'

# ---------------------------------------------------------------------------
# Shared HA context (same as integration test)
# ---------------------------------------------------------------------------

$SystemContext = @"
Available entities:
  kitchen light      (light.kitchen_light)
  bedroom light      (light.bedroom_light)
  office light       (light.office_light)
  front door lock    (lock.front_door_lock)
  garage door        (cover.garage_door)
  thermostat         (climate.thermostat)
"@

$HaTools = @(
    @{
        type     = 'function'
        function = @{
            name        = 'HassTurnOn'
            description = 'Turn on a device, area, or lock'
            parameters  = @{
                type       = 'object'
                properties = @{ name = @{ type = 'string' } }
                required   = @('name')
            }
        }
    },
    @{
        type     = 'function'
        function = @{
            name        = 'HassTurnOff'
            description = 'Turn off a device or area'
            parameters  = @{
                type       = 'object'
                properties = @{ name = @{ type = 'string' } }
                required   = @('name')
            }
        }
    },
    @{
        type     = 'function'
        function = @{
            name        = 'HassGetState'
            description = 'Get the current state of a device'
            parameters  = @{
                type       = 'object'
                properties = @{ name = @{ type = 'string' } }
                required   = @('name')
            }
        }
    }
)

# ---------------------------------------------------------------------------
# Tests: [label, needs_tool, messages, tools]
# needs_tool = true → quality gate requires a tool_call in response
# ---------------------------------------------------------------------------

$SysMsg = @{ role = 'system'; content = $SystemContext }

$Tests = @(
    @{ Id='G1'; Label='greeting';     NeedsTool=$false
       Messages=@($SysMsg, @{role='user';content='Hey, good morning!'}) },
    @{ Id='G2'; Label='greeting';     NeedsTool=$false
       Messages=@($SysMsg, @{role='user';content='Goodnight'}) },
    @{ Id='CC1'; Label='chitchat';    NeedsTool=$false
       Messages=@($SysMsg, @{role='user';content='Tell me a joke'}) },
    @{ Id='CC2'; Label='chitchat';    NeedsTool=$false
       Messages=@($SysMsg, @{role='user';content="What's the capital of Brazil?"}) },
    @{ Id='CMD1'; Label='command';    NeedsTool=$true; Tools=$HaTools
       Messages=@($SysMsg, @{role='user';content='Turn on the kitchen light'}) },
    @{ Id='CMD2'; Label='command';    NeedsTool=$true; Tools=$HaTools
       Messages=@($SysMsg, @{role='user';content='Lock the front door'}) },
    @{ Id='CMD3'; Label='command';    NeedsTool=$true; Tools=$HaTools
       Messages=@($SysMsg, @{role='user';content='Turn on the light'}) },
    @{ Id='CONV1'; Label='conversation'; NeedsTool=$true; Tools=$HaTools
       Messages=@($SysMsg,
           @{role='assistant';content="Which room? I have lights in the kitchen and bedroom."},
           @{role='user';content='The kitchen one'}) },
    @{ Id='INQ1'; Label='inquiry';    NeedsTool=$true; Tools=$HaTools
       Messages=@($SysMsg, @{role='user';content='Is the front door locked?'}) },
    @{ Id='INQ2'; Label='inquiry';    NeedsTool=$true; Tools=$HaTools
       Messages=@($SysMsg, @{role='user';content='Are all the lights off?'}) },
    @{ Id='INQ3'; Label='inquiry';    NeedsTool=$true; Tools=$HaTools
       Messages=@($SysMsg, @{role='user';content="What's the thermostat set to?"}) }
)

# ---------------------------------------------------------------------------
# HTTP helper
# ---------------------------------------------------------------------------

function Invoke-Tier([string]$Tier, [array]$Messages, [array]$Tools=@()) {
    $body = @{
        model    = "local:$Tier"
        messages = $Messages
        stream   = $false
    }
    if ($Tools.Count -gt 0) { $body.tools = $Tools }

    $headers = @{ 'Content-Type' = 'application/json' }
    if ($ClientKey) { $headers['Authorization'] = "Bearer $ClientKey" }

    $json = $body | ConvertTo-Json -Depth 10 -Compress
    $sw   = [System.Diagnostics.Stopwatch]::StartNew()
    try {
        $r = Invoke-RestMethod -Method Post -Uri "$Base/v1/chat/completions" `
                -Headers $headers -Body $json -TimeoutSec $TimeoutSec
        $sw.Stop()
        [PSCustomObject]@{ Ok=$true; Ms=$sw.ElapsedMilliseconds; Response=$r }
    } catch {
        $sw.Stop()
        [PSCustomObject]@{ Ok=$false; Ms=$sw.ElapsedMilliseconds; Response=$null; Err=$_.Exception.Message }
    }
}

function Test-Quality($response, [bool]$needsTool) {
    if (-not $response.Ok) { return $false }
    $msg = $response.Response.choices[0].message
    if ($needsTool) {
        return $null -ne $msg.tool_calls -and $msg.tool_calls.Count -gt 0
    } else {
        $c = $msg.content
        return ($null -ne $c -and $c.Trim().Length -gt 10)
    }
}

function Get-Short($response) {
    if (-not $response.Ok) { return "ERR: $($response.Err.Substring(0,[Math]::Min(50,$response.Err.Length)))" }
    $msg = $response.Response.choices[0].message
    if ($msg.tool_calls) {
        $calls = $msg.tool_calls | ForEach-Object { $_.function.name }
        return "TOOL:$($calls -join '+')"
    }
    $c = $msg.content.Trim() -replace '\r?\n',' '
    if ($c.Length -gt 50) { return $c.Substring(0,47) + '...' } else { return $c }
}

# ---------------------------------------------------------------------------
# Run probe
# ---------------------------------------------------------------------------

Write-Host ''
Write-Host '=== Minimum-Tier Probe ===' -ForegroundColor Cyan
Write-Host "  Tiers tested: $($Tiers -join ' → ')"
Write-Host "  Test cases  : $($Tests.Count)"
Write-Host ''

# Header
$tierCols = $Tiers | ForEach-Object { '{0,-10}' -f $_ }
Write-Host ('{0,-6} {1,-12} {2}  first-pass summary' -f 'ID','label', ($tierCols -join '')) -ForegroundColor Gray
Write-Host ('-' * 90)

$results = [System.Collections.ArrayList]::new()

foreach ($tc in $Tests) {
    $tools      = if ($tc.Tools) { $tc.Tools } else { @() }
    $tierPass   = @{}
    $tierMs     = @{}
    $firstPass  = $null
    $firstShort = ''

    foreach ($tier in $Tiers) {
        $r = Invoke-Tier -Tier $tier -Messages $tc.Messages -Tools $tools
        $pass = Test-Quality $r $tc.NeedsTool
        $tierPass[$tier]  = $pass
        $tierMs[$tier]    = $r.Ms
        if ($pass -and -not $firstPass) {
            $firstPass  = $tier
            $firstShort = Get-Short $r
        }
    }

    [void]$results.Add([PSCustomObject]@{
        Id        = $tc.Id
        Label     = $tc.Label
        TierPass  = $tierPass
        TierMs    = $tierMs
        FirstPass = $firstPass
        Summary   = $firstShort
    })

    $cols = $Tiers | ForEach-Object {
        $sym = if ($tierPass[$_]) { 'PASS' } else { 'MISS' }
        $col = if ($tierPass[$_]) { 'Green' } else { 'DarkGray' }
        @{ Text = ('{0,-10}' -f $sym); Color = $col }
    }

    Write-Host ('{0,-6} {1,-12} ' -f $tc.Id, $tc.Label) -NoNewline
    foreach ($c in $cols) {
        Write-Host $c.Text -NoNewline -ForegroundColor $c.Color
    }
    $passColor = if ($firstPass) { 'Green' } else { 'Red' }
    Write-Host "  min=$firstPass  $firstShort" -ForegroundColor $passColor
}

Write-Host ('-' * 90)
Write-Host ''
Write-Host 'MINIMUM TIER SUMMARY:' -ForegroundColor Cyan

foreach ($grp in ($results | Group-Object FirstPass | Sort-Object Name)) {
    $ids = ($grp.Group | ForEach-Object { $_.Id }) -join ', '
    Write-Host ("  {0,-10} ← {1}" -f $grp.Name, $ids) -ForegroundColor Green
}

$never = $results | Where-Object { -not $_.FirstPass }
if ($never) {
    $ids = ($never | ForEach-Object { $_.Id }) -join ', '
    Write-Host ("  {0,-10} ← {1} (no tier passed!)" -f 'NONE', $ids) -ForegroundColor Red
}

Write-Host ''
