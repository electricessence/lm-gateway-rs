#Requires -Version 7
[CmdletBinding()]
param(
    [string]$Base       = 'http://10.10.80.20:8080',
    [string]$ClientKey  = $env:LMG_CLIENT_KEY
)
$ErrorActionPreference = 'Stop'
if (-not $ClientKey) { throw 'Set LMG_CLIENT_KEY or pass -ClientKey' }

function ICT([string]$Model, [string]$Q) {
    $body = @{ model=$Model; messages=@(@{role='user';content=$Q}); stream=$false } |
            ConvertTo-Json -Depth 5 -Compress
    $sw = [System.Diagnostics.Stopwatch]::StartNew()
    try {
        $r = Invoke-RestMethod -Method Post -Uri "$Base/v1/chat/completions" `
                -Headers @{ Authorization="Bearer $ClientKey"; 'Content-Type'='application/json' } `
                -Body $body -TimeoutSec 60
        $sw.Stop()
        [PSCustomObject]@{ Ok=$true; Ms=$sw.ElapsedMilliseconds; Mdl=$r.model }
    } catch {
        $sw.Stop()
        [PSCustomObject]@{ Ok=$false; Ms=$sw.ElapsedMilliseconds; Err=$_.Exception.Message }
    }
}

function Get-Tier([string]$m) {
    if     ($m -match 'qwen3:1\.7b') { 'instant/fast' }
    elseif ($m -match 'qwen2\.5')    { 'moderate'     }
    else                             { 'deep/max'      }
}

$pass = 0; $fail = 0

function Show-Row([string]$Profile, [string]$Q, [string]$Expect, [string]$Class) {
    $r = ICT $Profile $Q
    if ($r.Ok) {
        $t  = Get-Tier $r.Mdl
        $ok = $t -eq $Expect
        if ($ok) { $script:pass++ } else { $script:fail++ }
        $icon = if ($ok) { 'OK  ' } else { 'MISS' }
        '{0} [{1,5}ms] got={2,-12} exp={3,-12} [{4,-12}] {5}' -f $icon, $r.Ms, $t, $Expect, $Class, $Q
    } else {
        $script:fail++
        'ERR  ' + $r.Err.Substring(0, [Math]::Min(100, $r.Err.Length))
    }
}

Write-Host ''
Write-Host '=== ha-auto — 6-label classifier ===' -ForegroundColor Cyan
Write-Host ('{0,-4} {1,7}   {2,-12} {3,-12} [{4,-12}] {5}' -f 'RSLT', 'ms', 'got', 'expected', 'class', 'prompt')
Write-Host ('-' * 90)

# greeting → instant (pure social, incl. time-of-day greetings)
Show-Row 'ha-auto:latest' 'Hey, good morning'             'instant/fast' 'greeting'
Show-Row 'ha-auto:latest' 'Goodnight'                     'instant/fast' 'greeting'

# chitchat → fast (1.7b + think)
Show-Row 'ha-auto:latest' 'Tell me a joke'                'instant/fast' 'chitchat'
Show-Row 'ha-auto:latest' "What's the capital of France?" 'instant/fast' 'chitchat'

# command → moderate (vague OR specific — all actions go here)
Show-Row 'ha-auto:latest' 'Turn on the light'             'moderate'     'command'
Show-Row 'ha-auto:latest' 'Lock up'                       'moderate'     'command'
Show-Row 'ha-auto:latest' 'Lock the front door'           'moderate'     'command'
Show-Row 'ha-auto:latest' 'Set the thermostat to 72'      'moderate'     'command'

# conversation → moderate
Show-Row 'ha-auto:latest' 'Yes'                           'moderate'     'conversation'
Show-Row 'ha-auto:latest' 'The bedroom one'               'moderate'     'conversation'

# inquiry → deep
Show-Row 'ha-auto:latest' 'Is the garage door open?'      'deep/max'     'inquiry'
Show-Row 'ha-auto:latest' 'Are all the doors locked?'     'deep/max'     'inquiry'
Show-Row 'ha-auto:latest' 'What lights are still on?'     'deep/max'     'inquiry'

Write-Host ('-' * 90)
Write-Host ''
$total = $pass + $fail
$color = if ($fail -eq 0) { 'Green' } else { 'Yellow' }
Write-Host "Result: $pass/$total correct" -ForegroundColor $color
