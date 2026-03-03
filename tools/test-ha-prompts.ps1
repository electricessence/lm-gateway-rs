#Requires -Version 7
<#
.SYNOPSIS
    End-to-end HA voice test — warms all 3 models, verifies they stay loaded, then runs battery.
.DESCRIPTION
    Uses `ssh cortex` (must be configured in ~/.ssh/config) to reach Proxmox,
    then `pct exec 200` to hit the lm-gateway inside LXC 200.
#>
[CmdletBinding()]
param(
    [switch]$NoWarmup
)

$ErrorActionPreference = 'Stop'

# -- Models expected in RAM after warmup --
$ExpectedModels = @('qwen3:1.7b', 'qwen2.5:7b-instruct', 'qwen3:14b-q3_K_M')

# -- Warmup: one request per distinct model (cheapest → heaviest) --
$WarmupTests = @(
    'good morning'                        # → instant  (qwen3:1.7b)
    'turn on the office light'            # → moderate (qwen2.5:7b-instruct)
    'turn off all lights in every room'   # → deep     (qwen3:14b-q3_K_M)
)

# -- Battery tests --
$Tests = @(
    'turn on the office light'
    'turn off all the lights'
    'lock the front door'
    'unlock the front door'
    'dim the bedroom light to 50%'
    'what is the living room temperature'
    'good morning'
)

# -- Minimal HA tool definitions for the request --
$ToolDefs = @(
    @{ type = "function"; function = @{ name = "HassTurnOn";  description = "Turn on a device";  parameters = @{ type = "object"; properties = @{ name = @{type="string"}; domain = @{type="string"}; area = @{type="string"} } } } }
    @{ type = "function"; function = @{ name = "HassTurnOff"; description = "Turn off a device"; parameters = @{ type = "object"; properties = @{ name = @{type="string"}; domain = @{type="string"}; area = @{type="string"} } } } }
    @{ type = "function"; function = @{ name = "HassGetState"; description = "Get the state of a device"; parameters = @{ type = "object"; properties = @{ name = @{type="string"}; domain = @{type="string"}; area = @{type="string"} } } } }
)

function Invoke-Ssh {
    <# Runs a command on the Proxmox host via ssh cortex. #>
    param([string]$Command)
    $result = ssh cortex $Command 2>&1
    if ($LASTEXITCODE -ne 0) { throw "SSH failed ($LASTEXITCODE): $result" }
    return $result
}

function Invoke-HaRequest {
    <# Sends a chat completion request to lm-gateway via SSH + base64 encoding. #>
    param([string]$Msg, [switch]$Silent)

    $body = @{
        model    = "ha-auto"
        stream   = $false
        tools    = $ToolDefs
        messages = @(@{ role = "user"; content = $Msg })
    } | ConvertTo-Json -Depth 10 -Compress

    # Base64-encode to avoid SSH/shell quoting issues
    $b64 = [Convert]::ToBase64String([System.Text.Encoding]::UTF8.GetBytes($body))
    $cmd = "pct exec 200 -- bash -c 'echo $b64 | base64 -d | curl -sf -X POST http://127.0.0.1:8080/v1/chat/completions -H Content-Type:application/json -d @-'"

    $sw = [System.Diagnostics.Stopwatch]::StartNew()
    $raw = Invoke-Ssh -Command $cmd
    $sw.Stop()
    $ms = $sw.ElapsedMilliseconds

    if ($Silent) { return }

    $result = ($raw -join "`n") | ConvertFrom-Json
    $c = $result.choices[0]
    $fr = $c.finish_reason.PadRight(10)
    $latency = "$($ms)ms".PadLeft(7)
    if ($c.message.tool_calls) {
        $tc = ($c.message.tool_calls | ForEach-Object {
            $a = $_.function.arguments
            "$($_.function.name)($a)"
        }) -join "; "
        Write-Host "[$fr] $latency  $($Msg.PadRight(45)) => $tc"
    } else {
        Write-Host "[$fr] $latency  $($Msg.PadRight(45)) => `"$($c.message.content)`""
    }
}

function Test-OllamaLoaded {
    <# Checks ollama ps and verifies all expected models are resident. #>
    Write-Host "`n--- Ollama loaded models ---" -ForegroundColor Cyan
    $ps = Invoke-Ssh -Command "pct exec 200 -- ollama ps"
    $ps | ForEach-Object { Write-Host "  $_" }

    $missing = @()
    foreach ($m in $ExpectedModels) {
        if (-not ($ps | Where-Object { $_ -match [regex]::Escape($m) })) {
            $missing += $m
        }
    }
    if ($missing.Count -gt 0) {
        Write-Host "  WARNING: missing from RAM: $($missing -join ', ')" -ForegroundColor Red
        return $false
    }
    Write-Host "  All 3 models loaded." -ForegroundColor Green
    return $true
}

# ======= Main =======

if (-not $NoWarmup) {
    Write-Host "Warming up all 3 models..." -ForegroundColor Cyan
    foreach ($msg in $WarmupTests) {
        Write-Host "  $msg" -ForegroundColor DarkGray
        Invoke-HaRequest -Msg $msg -Silent
    }
    Write-Host "Warmup done." -ForegroundColor Cyan

    # Verify all models stuck in RAM
    $ok = Test-OllamaLoaded
    if (-not $ok) {
        Write-Warning "Not all models are loaded — results may include cold-start latency."
    }
}

Write-Host "`n=== Battery Tests ===" -ForegroundColor Cyan
foreach ($msg in $Tests) {
    Invoke-HaRequest -Msg $msg
}

# Final check — prove they stayed loaded
Test-OllamaLoaded | Out-Null
