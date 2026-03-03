#Requires -Version 7
<#
.SYNOPSIS
    Push a local file into an LXC container via Proxmox pct push.
.DESCRIPTION
    Copies a local file to a temporary path on the Proxmox host, then uses
    pct push to place it inside the target LXC container.
    Resolves defaults from tools/.env.ps1 if present.
.PARAMETER LocalPath
    Absolute path to the file on this machine.
.PARAMETER RemotePath
    Absolute destination path inside the LXC container.
.PARAMETER SshAlias
    SSH Host alias that resolves to the Proxmox node (see ~/.ssh/config).
    Default: value from .env.ps1, or 'cortex'.
.PARAMETER LxcId
    Proxmox container ID to push into.
    Default: value from .env.ps1, or 200.
.EXAMPLE
    .\Push-ToLxc.ps1 -LocalPath src\config\mod.rs -RemotePath /opt/lm-gateway/src/config/mod.rs
.EXAMPLE
    .\Push-ToLxc.ps1 src\main.rs /opt/lm-gateway/src/main.rs -SshAlias myprox -LxcId 201
#>
[CmdletBinding()]
param(
    [Parameter(Mandatory, Position = 0)]
    [ValidateNotNullOrEmpty()]
    [string]$LocalPath,

    [Parameter(Mandatory, Position = 1)]
    [ValidateNotNullOrEmpty()]
    [string]$RemotePath,

    [string]$SshAlias,

    [int]$LxcId = 0
)

$ErrorActionPreference = 'Stop'

# Load shared defaults if present.
$envFile = Join-Path $PSScriptRoot '.env.ps1'
if (Test-Path $envFile) { . $envFile }

if (-not $SshAlias) { $SshAlias = if ($DefaultSshAlias) { $DefaultSshAlias } else { 'cortex' } }
if ($LxcId -eq 0)   { $LxcId   = if ($DefaultLxcId)    { $DefaultLxcId    } else { 200        } }

# Resolve absolute local path.
$LocalPath = Resolve-Path $LocalPath | Select-Object -ExpandProperty Path

if (-not (Test-Path $LocalPath -PathType Leaf)) {
    Write-Error "Not a file: $LocalPath"
}

$fileName = Split-Path $LocalPath -Leaf
$tmpPath  = "/tmp/lmg-push-$([System.IO.Path]::GetRandomFileName().Replace('.',''))-$fileName"

Write-Host "Uploading $LocalPath -> ${SshAlias}:$tmpPath ..." -ForegroundColor Cyan
scp $LocalPath "${SshAlias}:${tmpPath}"

$remoteDir = [System.IO.Path]::GetDirectoryName($RemotePath).Replace('\', '/')
Write-Host "Installing into LXC $LxcId at $RemotePath ..." -ForegroundColor Cyan
ssh $SshAlias "pct exec $LxcId -- mkdir -p '$remoteDir' && pct push $LxcId '$tmpPath' '$RemotePath' && rm -f '$tmpPath'"

Write-Host "Done: $RemotePath" -ForegroundColor Green
