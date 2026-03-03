#Requires -Version 7
<#
.SYNOPSIS
    Push modified lm-gateway source files to LXC and run cargo test.
.DESCRIPTION
    Gets the list of modified files from git, pushes each to the corresponding
    path inside the LXC container, then runs cargo test to validate.
    This is the standard dev loop: edit locally -> sync -> test remotely.
    Resolves defaults from tools/.env.ps1 if present.
.PARAMETER SshAlias
    SSH Host alias that resolves to the Proxmox node (see ~/.ssh/config).
    Default: value from .env.ps1, or 'cortex'.
.PARAMETER LxcId
    Proxmox container ID.
    Default: value from .env.ps1, or 200.
.PARAMETER ProjectPath
    Absolute path to the Rust project inside the LXC container.
    Default: value from .env.ps1, or '/opt/lm-gateway'.
.PARAMETER RepoRoot
    Local path to the claw-router repository root.
    Default: parent of the script's directory (i.e. the repo root when run from tools/).
.PARAMETER SkipTest
    Push files without running cargo test.
.PARAMETER TestFilter
    Optional cargo test filter (substring match).
.EXAMPLE
    .\Sync-LmGateway.ps1
.EXAMPLE
    .\Sync-LmGateway.ps1 -SkipTest
.EXAMPLE
    .\Sync-LmGateway.ps1 -TestFilter config::tests
#>
[CmdletBinding()]
param(
    [string]$SshAlias,

    [int]$LxcId = 0,

    [string]$ProjectPath,

    [string]$RepoRoot,

    [switch]$SkipTest,

    [string]$TestFilter = ''
)

$ErrorActionPreference = 'Stop'

# Load shared defaults if present.
$envFile = Join-Path $PSScriptRoot '.env.ps1'
if (Test-Path $envFile) { . $envFile }

if (-not $SshAlias)    { $SshAlias    = if ($DefaultSshAlias)     { $DefaultSshAlias     } else { 'cortex'          } }
if ($LxcId -eq 0)      { $LxcId      = if ($DefaultLxcId)         { $DefaultLxcId         } else { 200               } }
if (-not $ProjectPath) { $ProjectPath = if ($DefaultLmGatewayPath) { $DefaultLmGatewayPath } else { '/opt/lm-gateway' } }
if (-not $RepoRoot)    { $RepoRoot    = Split-Path $PSScriptRoot -Parent }

# Helper: push a single file to the LXC container.
function Push-File {
    param([string]$LocalFile, [string]$RemoteFile)

    $tmpName = "lmg-sync-$([System.IO.Path]::GetRandomFileName().Replace('.',''))-$(Split-Path $LocalFile -Leaf)"
    $tmpPath = "/tmp/$tmpName"
    $remoteDir = [System.IO.Path]::GetDirectoryName($RemoteFile).Replace('\', '/')

    scp $LocalFile "${SshAlias}:${tmpPath}" | Out-Null
    ssh $SshAlias "pct exec $LxcId -- mkdir -p '$remoteDir' && pct push $LxcId '$tmpPath' '$RemoteFile' && rm -f '$tmpPath'" | Out-Null
}

# Discover modified / staged files via git status.
Push-Location $RepoRoot
try {
    $gitFiles = git status --short --porcelain |
        Where-Object { $_ -match '^[ MADRCU?]{1,2}\s+(.+)$' } |
        ForEach-Object { $Matches[1].Trim() } |
        Where-Object { Test-Path (Join-Path $RepoRoot $_) -PathType Leaf }
} finally {
    Pop-Location
}

# Always include config.example.toml if it exists (it doesn't have src/ prefix).
$alwaysInclude = @('config.example.toml')
$filesToPush = ($gitFiles + $alwaysInclude) |
    Select-Object -Unique |
    Where-Object { Test-Path (Join-Path $RepoRoot $_) -PathType Leaf }

if (-not $filesToPush) {
    Write-Host 'No modified files found — nothing to push.' -ForegroundColor Yellow
    exit 0
}

Write-Host "Pushing $($filesToPush.Count) file(s) to LXC $LxcId ..." -ForegroundColor Cyan

foreach ($rel in $filesToPush) {
    $local  = Join-Path $RepoRoot $rel
    $remote = "$ProjectPath/$($rel.Replace('\', '/'))"
    Write-Host "  $rel" -ForegroundColor DarkCyan
    Push-File -LocalFile $local -RemoteFile $remote
}

Write-Host 'Push complete.' -ForegroundColor Green

if (-not $SkipTest) {
    Write-Host ''
    & (Join-Path $PSScriptRoot 'Invoke-LxcCargoTest.ps1') `
        -SshAlias $SshAlias `
        -LxcId $LxcId `
        -ProjectPath $ProjectPath `
        -Filter $TestFilter
}
