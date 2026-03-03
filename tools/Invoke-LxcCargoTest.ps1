#Requires -Version 7
<#
.SYNOPSIS
    Run cargo test inside the LXC container and stream the results.
.DESCRIPTION
    Executes 'cargo test' inside the lm-gateway LXC container via pct exec.
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
.PARAMETER Filter
    Optional test name filter passed directly to cargo test (substring match).
.EXAMPLE
    .\Invoke-LxcCargoTest.ps1
.EXAMPLE
    .\Invoke-LxcCargoTest.ps1 -Filter config::tests
.EXAMPLE
    .\Invoke-LxcCargoTest.ps1 -SshAlias myprox -LxcId 201 -ProjectPath /opt/myapp
#>
[CmdletBinding()]
param(
    [string]$SshAlias,

    [int]$LxcId = 0,

    [string]$ProjectPath,

    [string]$Filter = ''
)

$ErrorActionPreference = 'Stop'

# Load shared defaults if present.
$envFile = Join-Path $PSScriptRoot '.env.ps1'
if (Test-Path $envFile) { . $envFile }

if (-not $SshAlias)    { $SshAlias    = if ($DefaultSshAlias)     { $DefaultSshAlias     } else { 'cortex'          } }
if ($LxcId -eq 0)      { $LxcId      = if ($DefaultLxcId)         { $DefaultLxcId         } else { 200               } }
if (-not $ProjectPath) { $ProjectPath = if ($DefaultLmGatewayPath) { $DefaultLmGatewayPath } else { '/opt/lm-gateway' } }

# Build the cargo command (with optional filter).
$cargoCmd = if ($Filter) {
    "cargo test '$Filter' 2>&1"
} else {
    'cargo test 2>&1'
}

# The rustup shim in ~/.cargo/bin/cargo needs RUSTUP_HOME and CARGO_HOME set when
# invoked via pct exec (no login shell, no ~/.profile).
# Use semicolons to avoid CRLF issues when encoding multi-line shell scripts.
$shellCmd = "export PATH=/root/.cargo/bin:`$PATH; export RUSTUP_HOME=/root/.rustup; export CARGO_HOME=/root/.cargo; cd '$ProjectPath' && $cargoCmd"
$b64 = [Convert]::ToBase64String([System.Text.Encoding]::UTF8.GetBytes($shellCmd))

Write-Host "Running cargo test on LXC $LxcId ($ProjectPath) ..." -ForegroundColor Cyan
if ($Filter) { Write-Host "Filter: $Filter" -ForegroundColor DarkCyan }

ssh $SshAlias "pct exec $LxcId -- bash -c 'echo $b64 | base64 -d | bash'"
