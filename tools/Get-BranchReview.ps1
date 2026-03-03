#Requires -Version 7
<#
.SYNOPSIS
    Summarizes the current branch state for pre-merge review.
.DESCRIPTION
    Runs git status, log diff vs main, changed file line counts, version,
    and key config/doc freshness checks. Designed for claw-router
    feature branches to assess readiness for merge to main.
.EXAMPLE
    .\tools\Get-BranchReview.ps1
.EXAMPLE
    .\tools\Get-BranchReview.ps1 -BaseBranch main -LineCountWarn 500 -LineCountError 800
#>
[CmdletBinding()]
param(
    [string]$BaseBranch     = 'main',
    [int]$LineCountWarn     = 500,
    [int]$LineCountError    = 800,
    [switch]$NoConfig
)

$ErrorActionPreference = 'Stop'

Set-Location $PSScriptRoot\..

# ── helpers ────────────────────────────────────────────────────────────────
function Write-Header([string]$Title) {
    Write-Host "`n── $Title " -ForegroundColor Cyan -NoNewline
    Write-Host ('─' * (50 - $Title.Length)) -ForegroundColor DarkCyan
}

function Write-Flag([string]$Emoji, [string]$Text, [ConsoleColor]$Color = 'White') {
    Write-Host "  $Emoji  $Text" -ForegroundColor $Color
}

# ── branch identity ────────────────────────────────────────────────────────
Write-Header 'Branch Identity'

$branch     = git rev-parse --abbrev-ref HEAD
$headSha    = git rev-parse --short HEAD
$aheadCount = (git rev-list "$BaseBranch..HEAD" --count 2>$null) -as [int]

Write-Host "  Branch  : " -NoNewline; Write-Host $branch -ForegroundColor Yellow
Write-Host "  HEAD    : " -NoNewline; Write-Host $headSha -ForegroundColor Yellow
Write-Host "  Ahead of $BaseBranch : " -NoNewline
if ($aheadCount -gt 0) {
    Write-Host "$aheadCount commit(s)" -ForegroundColor $(if ($aheadCount -gt 5) { 'Red' } else { 'Yellow' })
} else {
    Write-Host "up to date" -ForegroundColor Green
}

# ── uncommitted changes ────────────────────────────────────────────────────
Write-Header 'Uncommitted Changes'

$status = git status --short
if ($status) {
    $status | ForEach-Object { Write-Host "  $_" -ForegroundColor Yellow }
} else {
    Write-Host "  (clean)" -ForegroundColor Green
}

# ── commits ahead of base ──────────────────────────────────────────────────
Write-Header "Commits Ahead of $BaseBranch"

$commits = git log "$BaseBranch..HEAD" --oneline
if ($commits) {
    $commits | ForEach-Object { Write-Host "  $_" -ForegroundColor White }
} else {
    Write-Host "  (none)" -ForegroundColor DarkGray
}

# ── changed files (vs base) ────────────────────────────────────────────────
Write-Header "Files Changed vs $BaseBranch"

$changedFiles = git diff "$BaseBranch...HEAD" --name-only 2>$null
if (-not $changedFiles) {
    Write-Host "  (no file diff)" -ForegroundColor DarkGray
} else {
    $changedFiles | ForEach-Object { Write-Host "  $_" }
}

# ── source file line counts ────────────────────────────────────────────────
Write-Header 'Source File Line Counts'

$rustFiles = Get-ChildItem -Path src -Recurse -Filter '*.rs' | Sort-Object FullName
foreach ($f in $rustFiles) {
    $lines = (Get-Content $f.FullName).Count
    $rel   = $f.FullName.Substring((Get-Location).Path.Length + 1).Replace('\','/')
    $color = if ($lines -ge $LineCountError) { 'Red' }
             elseif ($lines -ge $LineCountWarn) { 'Yellow' }
             else { 'Green' }
    $flag  = if ($lines -ge $LineCountError) { '🔴' }
             elseif ($lines -ge $LineCountWarn) { '🟡' }
             else { '🟢' }
    Write-Host ("  $flag  {0,-50} {1,5} lines" -f $rel, $lines) -ForegroundColor $color
}

# ── version ────────────────────────────────────────────────────────────────
Write-Header 'Version'

$version = Select-String -Path Cargo.toml -Pattern '^version\s*=' |
           Select-Object -First 1 | ForEach-Object { $_.Line }
Write-Host "  $version" -ForegroundColor White

# ── config freshness ───────────────────────────────────────────────────────
if (-not $NoConfig) {
    Write-Header 'Config Freshness Checks'

    # config.example.toml — check for stale tier labels
    if (Test-Path config.example.toml) {
        $exampleContent = Get-Content config.example.toml -Raw
        $staleLabels = @('simple', 'complex')   # should be instant/fast/moderate/deep/max
        foreach ($label in $staleLabels) {
            if ($exampleContent -match "\b$label\b") {
                Write-Flag '🔴' "config.example.toml contains stale label: '$label'" Red
            }
        }
        if ($exampleContent -match 'cloud:|expert') {
            Write-Flag '🔴' "config.example.toml references cloud/expert models (expected all-local)" Red
        }
        Write-Flag '🟢' "config.example.toml checked" Green
    } else {
        Write-Flag '🟡' "config.example.toml not found" Yellow
    }

    # README — check for stale tier labels
    if (Test-Path README.md) {
        $readmeContent = Get-Content README.md -Raw
        $staleReadme = @('`simple`', '`moderate`', '`complex`')
        $staleFound = $false
        foreach ($label in $staleReadme) {
            if ($readmeContent -match [regex]::Escape($label)) {
                Write-Flag '🟡' "README.md contains possibly-stale tier label: $label" Yellow
                $staleFound = $true
            }
        }
        if (-not $staleFound) {
            Write-Flag '🟢' "README.md tier labels look current" Green
        }
    }
}

# ── summary ────────────────────────────────────────────────────────────────
Write-Header 'Summary'
Write-Host "  Branch ready? Review flags above." -ForegroundColor DarkGray
Write-Host "  Thresholds: warn=$LineCountWarn lines  error=$LineCountError lines`n"
