$ErrorActionPreference = 'Stop'
[CmdletBinding()]
param(
    [string]$GatewayUrl = $env:LMG_URL,
    [string]$HaKey      = $env:LMG_HA_KEY
)

if (-not $GatewayUrl) { throw "Set LMG_URL env var or pass -GatewayUrl" }
if (-not $HaKey)      { throw "Set LMG_HA_KEY env var or pass -HaKey" }

$base  = $GatewayUrl.TrimEnd('/')
$haKey = $HaKey

function Invoke-GatewayTest {
    param([string]$Model, [string]$Query, [string]$Key)
    $body  = @{ model = $Model; messages = @(@{ role = "user"; content = $Query }); stream = $false } | ConvertTo-Json -Depth 5 -Compress
    $start = [System.Diagnostics.Stopwatch]::StartNew()
    try {
        $r   = Invoke-RestMethod -Method Post -Uri "$base/v1/chat/completions" `
                   -Headers @{ Authorization = "Bearer $Key"; "Content-Type" = "application/json" } `
                   -Body $body -TimeoutSec 120
        $start.Stop()
        $ans = $r.choices[0].message.content.Trim()
        $ans = if ($ans.Length -gt 120) { $ans.Substring(0, 120) + "…" } else { $ans }
        [PSCustomObject]@{ Ms = $start.ElapsedMilliseconds; Model = $r.model; Query = $Query; Answer = $ans; Error = $null }
    } catch {
        $start.Stop()
        [PSCustomObject]@{ Ms = $start.ElapsedMilliseconds; Model = $Model; Query = $Query; Answer = ""; Error = $_.Exception.Message }
    }
}

Write-Host "`n=== ha-auto profile ===" -ForegroundColor Cyan
$haTests = @(
    "Lock the front door",
    "Is the garage door open?",
    "Create an automation to turn the porch light on at sunset",
    "Why does my away mode trigger twice when I leave?"
)
foreach ($q in $haTests) {
    $res = Invoke-GatewayTest -Model "ha-auto:latest" -Query $q -Key $haKey
    if ($res.Error) {
        Write-Host "  ERR [$($res.Ms)ms] $q  →  $($res.Error)" -ForegroundColor Red
    } else {
        Write-Host "  OK  [$($res.Ms)ms | $($res.Model)] $q" -ForegroundColor Yellow
        Write-Host "       $($res.Answer)" -ForegroundColor White
    }
}

Write-Host "`n=== general profile ===" -ForegroundColor Cyan
$genTests = @(
    "What does TCP/IP mean?",
    "Write a PowerShell function to parse a CSV file",
    "Design a distributed cache with eviction policies"
)
foreach ($q in $genTests) {
    $res = Invoke-GatewayTest -Model "general:latest" -Query $q -Key $haKey
    if ($res.Error) {
        Write-Host "  ERR [$($res.Ms)ms] $q  →  $($res.Error)" -ForegroundColor Red
    } else {
        Write-Host "  OK  [$($res.Ms)ms | $($res.Model)] $q" -ForegroundColor Yellow
        Write-Host "       $($res.Answer)" -ForegroundColor White
    }
}
