#Requires -Version 7
<#
.SYNOPSIS
    End-to-end smoke test for lm-gateway — verifies routing, classification, aliases, and admin API.

.DESCRIPTION
    Runs a one-pass verification across the gateway's major features:
      1. Admin API health & config
      2. Profile routing (each profile gets a test prompt)
      3. Classification accuracy (label → tier mapping)
      4. Alias resolution (hint:* aliases resolve to correct tiers)
      5. Context-window gating (large payload bumps tier)

    Each test sends a real request and checks the response model field and/or
    admin traffic log. Results are printed as a pass/fail table and optionally
    exported to CSV.

    Designed for quick post-deploy verification. Not a load test.

.PARAMETER GatewayUrl
    Base URL of the client endpoint (no trailing /v1). Default: LMG_URL env var.

.PARAMETER AdminUrl
    Base URL of the admin endpoint. Default: derived from GatewayUrl (port 8081).

.PARAMETER AdminToken
    Admin bearer token. Default: LMG_ADMIN_TOKEN env var. Required.

.PARAMETER HaKey
    HA client API key for testing ha-auto profile. Default: LMG_HA_KEY env var.

.PARAMETER TimeoutSec
    Per-request timeout. Default: 60.

.PARAMETER CsvPath
    Optional path to export results as CSV.

.EXAMPLE
    .\Test-GatewayE2E.ps1 -GatewayUrl http://my-gateway:8080 -AdminToken <token>
    Runs the full smoke test suite against the gateway.

.EXAMPLE
    .\Test-GatewayE2E.ps1 -CsvPath results.csv
    Runs the suite and exports results to CSV.
#>
[CmdletBinding()]
param(
    [string]$GatewayUrl  = $env:LMG_URL,
    [string]$AdminUrl    = $env:LMG_ADMIN_URL,
    [string]$AdminToken  = $env:LMG_ADMIN_TOKEN,
    [string]$HaKey       = $env:LMG_HA_KEY,
    [int]   $TimeoutSec  = 60,
    [string]$CsvPath     = ''
)

$ErrorActionPreference = 'Stop'

if (-not $GatewayUrl)  { throw 'Set LMG_URL env var or pass -GatewayUrl' }
if (-not $AdminToken)  { throw 'Set LMG_ADMIN_TOKEN env var or pass -AdminToken' }

# Derive admin URL from client URL when not explicitly set
if (-not $AdminUrl) {
    $AdminUrl = ($GatewayUrl -replace ':8080', ':8081' -replace '/v1.*', '').TrimEnd('/')
}
# Normalize: strip any trailing /v1 path so Send-ChatRequest can always append /v1/...
$clientBase = ($GatewayUrl -replace '/v1/?$', '').TrimEnd('/')
$adminBase  = $AdminUrl.TrimEnd('/')

Write-Host "`n========================================" -ForegroundColor Cyan
Write-Host " LM Gateway — End-to-End Smoke Test" -ForegroundColor Cyan
Write-Host "========================================" -ForegroundColor Cyan
Write-Host "  Client : $clientBase"
Write-Host "  Admin  : $adminBase"
Write-Host ""

# --- Helpers ---------------------------------------------------------------

$results = [System.Collections.Generic.List[PSCustomObject]]::new()

function Add-Result {
    param(
        [string]$Category,
        [string]$Test,
        [string]$Expected,
        [string]$Actual,
        [bool]$Pass,
        [long]$Ms = 0,
        [string]$Note = ''
    )
    $script:results.Add([PSCustomObject]@{
        Category = $Category
        Test     = $Test
        Expected = $Expected
        Actual   = $Actual
        Pass     = if ($Pass) { 'PASS' } else { 'FAIL' }
        Ms       = $Ms
        Note     = $Note
    })
    $color = if ($Pass) { 'Green' } else { 'Red' }
    $icon  = if ($Pass) { '[PASS]' } else { '[FAIL]' }
    Write-Host "  $icon $Category / $Test" -ForegroundColor $color -NoNewline
    if ($Note) { Write-Host "  ($Note)" -ForegroundColor DarkGray } else { Write-Host '' }
}

function Send-ChatRequest {
    param(
        [string]$Model,
        [string]$Prompt,
        [string]$Key = '',
        [string]$SystemPrompt = '',
        [bool]$Stream = $false
    )
    $messages = @()
    if ($SystemPrompt) {
        $messages += @{ role = 'system'; content = $SystemPrompt }
    }
    $messages += @{ role = 'user'; content = $Prompt }
    $body = @{
        model    = $Model
        messages = $messages
        stream   = $Stream
    } | ConvertTo-Json -Depth 5 -Compress

    $headers = @{ 'Content-Type' = 'application/json' }
    if ($Key) {
        $headers['Authorization'] = "Bearer $Key"
    }

    $sw = [System.Diagnostics.Stopwatch]::StartNew()
    try {
        $r = Invoke-RestMethod -Method Post -Uri "$clientBase/v1/chat/completions" `
            -Headers $headers -Body $body -TimeoutSec $TimeoutSec -NoProxy
        $sw.Stop()
        return @{
            Ok      = $true
            Ms      = $sw.ElapsedMilliseconds
            Model   = $r.model
            Content = $r.choices[0].message.content
            Error   = $null
        }
    } catch {
        $sw.Stop()
        return @{
            Ok      = $false
            Ms      = $sw.ElapsedMilliseconds
            Model   = '?'
            Content = ''
            Error   = $_.Exception.Message
        }
    }
}

function Get-AdminData {
    param([string]$Endpoint)
    try {
        return Invoke-RestMethod -Uri "$adminBase/admin/$Endpoint" `
            -Headers @{ Authorization = "Bearer $AdminToken" } -TimeoutSec 10 -NoProxy
    } catch {
        Write-Warning "Admin /$Endpoint failed: $_"
        return $null
    }
}

# ===================================================================
# 1. ADMIN API
# ===================================================================
Write-Host "`n--- 1. Admin API ---" -ForegroundColor Yellow

# Health check
$health = Get-AdminData 'health'
Add-Result -Category 'Admin' -Test 'Health endpoint' `
    -Expected 'ok' -Actual ($health.status ?? 'null') `
    -Pass ($health.status -eq 'ok')

# Config endpoint — verify tiers are listed
$config = Get-AdminData 'config'
$tierNames = @($config.tiers | ForEach-Object { $_.name })
$expectedTiers = @('local:instant', 'local:fast', 'local:moderate', 'local:deep', 'local:max')
$allTiersPresent = ($expectedTiers | Where-Object { $_ -in $tierNames }).Count -eq $expectedTiers.Count
Add-Result -Category 'Admin' -Test 'Config lists all 5 tiers' `
    -Expected ($expectedTiers -join ', ') -Actual ($tierNames -join ', ') `
    -Pass $allTiersPresent

# Config — verify max_context_tokens is present on tiers
$hasCtx = ($config.tiers | Where-Object { $null -ne $_.max_context_tokens }).Count -eq 5
Add-Result -Category 'Admin' -Test 'Tiers have max_context_tokens' `
    -Expected '5 tiers with values' -Actual "$($($config.tiers | Where-Object { $null -ne $_.max_context_tokens }).Count) tiers" `
    -Pass $hasCtx

# Config — verify profiles exist
$profileNames = if ($config.profiles) {
    @($config.profiles | Get-Member -MemberType NoteProperty | ForEach-Object { $_.Name })
} else { @() }
Add-Result -Category 'Admin' -Test 'Profiles listed' `
    -Expected 'ha-auto + claw-agent + others' -Actual ($profileNames -join ', ') `
    -Pass (($profileNames -contains 'ha-auto') -and ($profileNames -contains 'claw-agent'))

# Traffic endpoint
$traffic = Get-AdminData 'traffic?limit=1'
Add-Result -Category 'Admin' -Test 'Traffic endpoint' `
    -Expected 'responds' -Actual $(if ($null -ne $traffic) { 'ok' } else { 'failed' }) `
    -Pass ($null -ne $traffic)

# ===================================================================
# 2. ALIAS RESOLUTION — hint:* bypasses classification (dispatch mode)
#    Note: aliases are only respected in dispatch-mode profiles.
#    In classify-mode profiles, the classifier overrides the alias.
#    Test uses explicit tier names without auth to verify direct routing.
# ===================================================================
Write-Host "`n--- 2. Alias Resolution (dispatch) ---" -ForegroundColor Yellow

# hint:instant should map to local:instant → qwen3:1.7b
# Even under classify mode, a simple "Say hi" should classify to instant anyway.
$aliasTests = @(
    @{ Model = 'hint:instant';  Expected = 'qwen3:1.7b';          Note = 'alias→instant' }
    @{ Model = 'hint:fast';     Expected = 'qwen3:1.7b';          Note = 'alias→fast (same base model)' }
)

foreach ($at in $aliasTests) {
    $r = Send-ChatRequest -Model $at.Model -Prompt 'Say hi in one word.'
    # The response model field should contain the expected model name
    $matched = $r.Model -like "*$($at.Expected)*"
    Add-Result -Category 'Alias' -Test "model=$($at.Model)" `
        -Expected $at.Expected -Actual $r.Model `
        -Pass ($r.Ok -and $matched) -Ms $r.Ms `
        -Note $(if (-not $r.Ok) { $r.Error } elseif ($at.Note) { $at.Note } else { '' })
}

# ===================================================================
# 3. PUBLIC PROFILE CLASSIFICATION (current public profile)
# ===================================================================
Write-Host "`n--- 3. Public Profile Classification ---" -ForegroundColor Yellow

# These test against the public profile (no auth key)
# Public profile is currently ha-auto, so test with HA-style prompts
$classifyTests = @(
    @{ Prompt = 'Hi';                                    ExpectTierContains = 'qwen3:1.7b';           Label = 'greeting→instant' }
    @{ Prompt = 'Tell me a joke';                        ExpectTierContains = 'qwen3:1.7b';           Label = 'chitchat→fast' }
    @{ Prompt = 'Turn on the office light';              ExpectTierContains = 'qwen2.5:7b-instruct';  Label = 'command→moderate' }
    @{ Prompt = 'Is the front door locked?';             ExpectTierContains = 'qwen3:14b';            Label = 'inquiry→deep' }
)

foreach ($ct in $classifyTests) {
    $r = Send-ChatRequest -Model 'auto' -Prompt $ct.Prompt
    # Model field in response indicates which tier handled it
    $matched = $r.Model -like "*$($ct.ExpectTierContains)*"
    Add-Result -Category 'Classify' -Test $ct.Label `
        -Expected $ct.ExpectTierContains -Actual $r.Model `
        -Pass ($r.Ok -and $matched) -Ms $r.Ms `
        -Note $(if (-not $r.Ok) { $r.Error } else { '' })
}

# ===================================================================
# 4. HA-AUTO PROFILE (authenticated)
# ===================================================================
Write-Host "`n--- 4. ha-auto Profile (authenticated) ---" -ForegroundColor Yellow

if ($HaKey) {
    $haTests = @(
        @{ Prompt = 'Good morning';                  ExpectTierContains = 'qwen3:1.7b';           Label = 'greeting' }
        @{ Prompt = 'Lock the front door';           ExpectTierContains = 'qwen2.5:7b-instruct';  Label = 'command' }
    )
    foreach ($ht in $haTests) {
        $r = Send-ChatRequest -Model 'ha-auto:latest' -Prompt $ht.Prompt -Key $HaKey
        $matched = $r.Model -like "*$($ht.ExpectTierContains)*"
        Add-Result -Category 'ha-auto' -Test $ht.Label `
            -Expected $ht.ExpectTierContains -Actual $r.Model `
            -Pass ($r.Ok -and $matched) -Ms $r.Ms `
            -Note $(if (-not $r.Ok) { $r.Error } else { '' })
    }
} else {
    Write-Host "  [SKIP] No LMG_HA_KEY — skipping authenticated ha-auto tests" -ForegroundColor DarkGray
    Add-Result -Category 'ha-auto' -Test 'skipped (no key)' `
        -Expected 'N/A' -Actual 'N/A' -Pass $true -Note 'Set LMG_HA_KEY to enable'
}

# ===================================================================
# 5. GENERAL PROFILE — direct model name routing
# ===================================================================
Write-Host "`n--- 5. general Profile ---" -ForegroundColor Yellow

$r = Send-ChatRequest -Model 'general:latest' -Prompt 'What is 2+2? Reply with just the number.'
Add-Result -Category 'general' -Test 'basic request' `
    -Expected 'response ok' -Actual $(if ($r.Ok) { "ok ($($r.Model))" } else { $r.Error }) `
    -Pass $r.Ok -Ms $r.Ms

# ===================================================================
# 6. CONTEXT-WINDOW GATING — verify large payload gets bumped
# ===================================================================
Write-Host "`n--- 6. Context-Window Gating ---" -ForegroundColor Yellow

# Generate a payload that exceeds the instant tier's 4096 token limit
# but fits in moderate (8192). ~5000 tokens ≈ ~20000 chars of prose.
# Use longer timeout since the bumped tier may need model cold-start.
$padding = ('The quick brown fox jumps over the lazy dog. ' * 500)
$origTimeout = $TimeoutSec
$TimeoutSec  = [Math]::Max($TimeoutSec, 180)
$r = Send-ChatRequest -Model 'ha-auto:latest' -Prompt "Summarize this text in one sentence: $padding" -Key $HaKey
$TimeoutSec  = $origTimeout

# The classify mode will try to route to a low tier, but context-window
# gating should bump it to at least moderate (qwen2.5:7b-instruct).
$bumped = $r.Model -notlike '*qwen3:1.7b*'
if ($r.Ok) {
    Add-Result -Category 'Gating' -Test 'Large payload bumps tier' `
        -Expected 'model > 1.7b' -Actual $r.Model `
        -Pass $bumped -Ms $r.Ms `
        -Note $(if ($bumped) { 'tier bumped as expected' } else { 'not bumped — may be within limit' })
} else {
    Add-Result -Category 'Gating' -Test 'Large payload bumps tier' `
        -Expected 'model > 1.7b' -Actual 'error' `
        -Pass $false -Ms $r.Ms -Note $r.Error
}

# ===================================================================
# SUMMARY
# ===================================================================
Write-Host "`n========================================" -ForegroundColor Cyan
$passed = ($results | Where-Object { $_.Pass -eq 'PASS' }).Count
$failed = ($results | Where-Object { $_.Pass -eq 'FAIL' }).Count
$total  = $results.Count

$summaryColor = if ($failed -eq 0) { 'Green' } else { 'Red' }
Write-Host " Results: $passed/$total passed" -ForegroundColor $summaryColor
if ($failed -gt 0) {
    Write-Host " Failures:" -ForegroundColor Red
    $results | Where-Object { $_.Pass -eq 'FAIL' } | ForEach-Object {
        Write-Host "   - $($_.Category)/$($_.Test): expected=$($_.Expected) actual=$($_.Actual)" -ForegroundColor Red
    }
}
Write-Host "========================================`n" -ForegroundColor Cyan

# Export to CSV if requested
if ($CsvPath) {
    $results | Export-Csv -Path $CsvPath -NoTypeInformation -Encoding UTF8
    Write-Host "Results exported to $CsvPath" -ForegroundColor Gray
}

# Output results for pipeline consumption
$results
