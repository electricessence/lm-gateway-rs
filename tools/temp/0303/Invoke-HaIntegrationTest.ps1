#Requires -Version 7
<#
.SYNOPSIS
    End-to-end integration test for the ha-auto classifier pipeline.
.DESCRIPTION
    Sends realistic Home Assistant voice commands through the ha-auto profile,
    verifying:
      1. Routing tier   — did the classifier send it to the right model tier?
      2. Response sense — does the response content / tool call look correct?

    Each test case carries an expected tier and an optional keyword that should
    appear in the response. The script does NOT enforce response content — it
    displays it for human review. Only tier routing is marked pass/fail.

    The entity list and HA tool definitions are embedded so responses reflect
    a real HA session (model will produce tool_calls for commands).
.PARAMETER Base
    Gateway base URL. Default: http://10.10.80.20:8080
.PARAMETER ClientKey
    Optional bearer token. Leave blank if gateway has no client auth.
.PARAMETER TimeoutSec
    Per-request HTTP timeout in seconds. Default: 90.
.EXAMPLE
    .\Invoke-HaIntegrationTest.ps1
.EXAMPLE
    .\Invoke-HaIntegrationTest.ps1 | Tee-Object integration-results.txt
#>
[CmdletBinding()]
param(
    [string]$Base      = 'http://10.10.80.20:8080',
    [string]$ClientKey = $env:LMG_CLIENT_KEY,
    [int]   $TimeoutSec = 90
)
$ErrorActionPreference = 'Stop'

# ---------------------------------------------------------------------------
# Context injected into every request (entity list + HA persona)
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
            description = 'Turn on a device, area, or lock (locks use HassTurnOn to lock)'
            parameters  = @{
                type       = 'object'
                properties = @{ name = @{ type = 'string'; description = 'Entity friendly name or area' } }
                required   = @('name')
            }
        }
    },
    @{
        type     = 'function'
        function = @{
            name        = 'HassTurnOff'
            description = 'Turn off a device or area. Use HassTurnOff on a lock to unlock it.'
            parameters  = @{
                type       = 'object'
                properties = @{ name = @{ type = 'string'; description = 'Entity friendly name or area' } }
                required   = @('name')
            }
        }
    },
    @{
        type     = 'function'
        function = @{
            name        = 'HassGetState'
            description = 'Get the current state of a device or area'
            parameters  = @{
                type       = 'object'
                properties = @{ name = @{ type = 'string'; description = 'Entity friendly name or area' } }
                required   = @('name')
            }
        }
    }
)

# ---------------------------------------------------------------------------
# HTTP helper
# ---------------------------------------------------------------------------

function Invoke-Chat {
    param(
        [string]$Model,
        [array] $Messages,
        [array] $Tools = @()
    )
    $body = @{
        model    = $Model
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
        [PSCustomObject]@{ Ok = $true; Ms = $sw.ElapsedMilliseconds; Model = $r.model; Response = $r }
    }
    catch {
        $sw.Stop()
        [PSCustomObject]@{ Ok = $false; Ms = $sw.ElapsedMilliseconds; Model = ''; Response = $null; Err = $_.Exception.Message }
    }
}

function Get-Tier([string]$m) {
    if     ($m -match 'qwen3:1\.7b') { 'instant/fast' }
    elseif ($m -match 'qwen2\.5')    { 'moderate'     }
    else                             { 'deep/max'      }
}

function Get-ResponseSummary($response) {
    $msg = $response.choices[0].message
    if ($msg.tool_calls) {
        $calls = $msg.tool_calls | ForEach-Object {
            "$($_.function.name)($($_.function.arguments))"
        }
        "TOOL: $($calls -join ' | ')"
    } elseif ($msg.content) {
        $c = $msg.content.Trim() -replace '\r?\n', ' '
        if ($c.Length -gt 160) { $c.Substring(0, 157) + '...' } else { $c }
    } else {
        '(empty)'
    }
}

# ---------------------------------------------------------------------------
# Test case definition
# Three-element: [label, expected_tier, messages]
# messages = @( @{role;content}, ... ) — last must be role=user
# ---------------------------------------------------------------------------

$SysMsg = @{ role = 'system'; content = $SystemContext }

$Tests = @(
    # ── greeting → instant/fast ────────────────────────────────────────────
    @{ Label='greeting'; Tier='instant/fast'; Note='social hello'
       Messages = @($SysMsg, @{role='user';content='Hey, good morning!'}) },

    @{ Label='greeting'; Tier='instant/fast'; Note='time-of-day farewell'
       Messages = @($SysMsg, @{role='user';content='Goodnight'}) },

    # ── chitchat → instant/fast ─────────────────────────────────────────────
    @{ Label='chitchat'; Tier='instant/fast'; Note='request for joke'
       Messages = @($SysMsg, @{role='user';content='Tell me a joke'}) },

    @{ Label='chitchat'; Tier='instant/fast'; Note='general knowledge'
       Messages = @($SysMsg, @{role='user';content="What's the capital of Brazil?"}) },

    # ── command → moderate ──────────────────────────────────────────────────
    @{ Label='command'; Tier='moderate'; Note='named device: kitchen light'
       Messages = @($SysMsg, @{role='user';content='Turn on the kitchen light'})
       Tools = $HaTools },

    @{ Label='command'; Tier='moderate'; Note='named device: front door lock'
       Messages = @($SysMsg, @{role='user';content='Lock the front door'})
       Tools = $HaTools },

    @{ Label='command'; Tier='moderate'; Note='control: thermostat'
       Messages = @($SysMsg, @{role='user';content='Set the thermostat to 72'})
       Tools = $HaTools },

    @{ Label='command'; Tier='moderate'; Note='vague (formerly its own label)'
       Messages = @($SysMsg, @{role='user';content='Turn on the light'})
       Tools = $HaTools },

    @{ Label='command'; Tier='moderate'; Note='vague lock command'
       Messages = @($SysMsg, @{role='user';content='Lock up'})
       Tools = $HaTools },

    # ── conversation → moderate ─────────────────────────────────────────────
    @{ Label='conversation'; Tier='moderate'; Note='disambiguation reply'
       Messages = @(
           $SysMsg,
           @{ role='assistant'; content="Which room? I have lights in the kitchen and bedroom." },
           @{ role='user';      content='The kitchen one' }
       )
       Tools = $HaTools },

    @{ Label='conversation'; Tier='moderate'; Note='simple confirmation'
       Messages = @(
           $SysMsg,
           @{ role='assistant'; content='Would you like me to lock all the doors?' },
           @{ role='user';      content='Yes' }
       )
       Tools = $HaTools },

    # ── inquiry → deep ──────────────────────────────────────────────────────
    @{ Label='inquiry'; Tier='deep/max'; Note='lock state query'
       Messages = @($SysMsg, @{role='user';content='Is the front door locked?'})
       Tools = $HaTools },

    @{ Label='inquiry'; Tier='deep/max'; Note='multi-entity state query'
       Messages = @($SysMsg, @{role='user';content='Are all the lights off?'})
       Tools = $HaTools },

    @{ Label='inquiry'; Tier='deep/max'; Note='sensor query'
       Messages = @($SysMsg, @{role='user';content="What's the thermostat set to?"})
       Tools = $HaTools }
)

# ---------------------------------------------------------------------------
# Run tests
# ---------------------------------------------------------------------------

$pass = 0; $fail = 0; $errors = 0

$sep = '-' * 100
Write-Host ''
Write-Host '=== ha-auto end-to-end integration test ===' -ForegroundColor Cyan
Write-Host "  Gateway : $Base"
Write-Host "  Profile : ha-auto:latest"
Write-Host "  Cases   : $($Tests.Count)"
Write-Host ''
Write-Host ('{0,-5} {1,7}ms  {2,-12} {3,-12} {4,-18} {5}' -f 'RSLT','','got','expect','[label / note]','response') -ForegroundColor Gray
Write-Host $sep

foreach ($tc in $Tests) {
    $tools = if ($tc.Tools) { $tc.Tools } else { @() }
    $r     = Invoke-Chat -Model 'ha-auto:latest' -Messages $tc.Messages -Tools $tools

    if (-not $r.Ok) {
        $errors++; $fail++
        Write-Host ('ERR  [{0,6}ms]  {1,-12} {2,-12} [{3,-16}]  ERROR: {4}' -f
            $r.Ms, '?', $tc.Tier, "$($tc.Label) / $($tc.Note)", $r.Err.Substring(0,[Math]::Min(80,$r.Err.Length)))
        continue
    }

    $tier   = Get-Tier $r.Model
    $ok     = $tier -eq $tc.Tier
    $icon   = if ($ok) { 'OK   ' } else { 'MISS ' }
    $color  = if ($ok) { 'Green' } else { 'Yellow' }
    if ($ok) { $pass++ } else { $fail++ }
    $summary = Get-ResponseSummary $r.Response

    Write-Host ('{0}[{1,6}ms]  {2,-12} {3,-12} [{4,-16}]' -f $icon, $r.Ms, $tier, $tc.Tier, "$($tc.Label) / $($tc.Note)") -ForegroundColor $color -NoNewline
    Write-Host "  $summary"
}

Write-Host $sep
Write-Host ''
$total = $pass + $fail
$color = if ($fail -eq 0) { 'Green' } else { 'Yellow' }
Write-Host "Routing result  : $pass/$total correct  ($errors errors)" -ForegroundColor $color
Write-Host "(Response content above is for visual inspection — not auto-verified)" -ForegroundColor Gray
Write-Host ''
