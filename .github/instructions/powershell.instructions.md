---
applyTo: "**/*.ps1"
---
# PowerShell Standards

## Core Mandate

**Always create reusable `.ps1` scripts.** Never leave repeatable work as ad-hoc terminal commands. Every script must be high quality, readable, and production-grade.

## Script Structure (Required)

```powershell
#Requires -Version 7
<#
.SYNOPSIS
    One-line description of what this script does.
.DESCRIPTION
    Detailed explanation: what it connects to, what it changes, what it outputs.
.EXAMPLE
    .\Verb-Noun.ps1
    .\Verb-Noun.ps1 -SshAlias myhost -LxcId 300
#>
[CmdletBinding()]
param(
    [string]$SshAlias = 'cortex',
    [int]$LxcId = 200
)

$ErrorActionPreference = 'Stop'
```

## Naming

- `Verb-Noun.ps1` — standard PowerShell convention
- Approved verbs: `Get-`, `Set-`, `Test-`, `Invoke-`, `Deploy-`, `Restart-`, `New-`, `Remove-`

## Quality Standards

- **Comment-based help**: `.SYNOPSIS`, `.DESCRIPTION`, `.EXAMPLE` on every script
- **Named parameters**: never positional-only; always `[CmdletBinding()]` + `param()`
- **Readable output**: `Write-Host` with `-ForegroundColor` for human status, `Write-Output` for piped data
- **Progress reporting**: for multi-step operations, print what's happening at each step
- **Error messages**: clear, actionable — say what failed and what to check
- **Consistent formatting**: 4-space indent, blank line between logical sections

## Parameterization

- Every environment-specific value (SSH alias, LXC ID, ports, model names, paths) → parameter with a default
- Never hardcode hostnames, IPs, credentials, or machine-specific paths
- Use `[ValidateSet()]`, `[ValidateRange()]`, `[ValidateNotNullOrEmpty()]` where appropriate

## String Quoting

- Embed `"` in double-quoted strings with backtick `` `" `` — **never `\"`**
- Single-quoted `'...'` for literals without variable expansion

## SSH Patterns

### Simple Commands

```powershell
ssh $SshAlias "pct exec $LxcId -- systemctl restart lm-gateway"
```

### JSON Payloads Over SSH

Base64-encode JSON to avoid quoting issues:

```powershell
$json = @{ model = 'qwen3:1.7b'; prompt = 'hi' } | ConvertTo-Json -Compress
$b64 = [Convert]::ToBase64String([System.Text.Encoding]::UTF8.GetBytes($json))
ssh $SshAlias "pct exec $LxcId -- bash -c 'echo $b64 | base64 -d | curl -sf http://localhost:11434/api/generate -d @-'"
```

## Verification Mandate

Every script that makes changes **must verify** each one:
- After file deploy: check service restarted and is active
- After model load: confirm it appears in `ollama ps`
- If verification fails: exit with clear error

## Other Rules

- `Invoke-RestMethod`/`Invoke-WebRequest` not `curl` (for local requests)
- `ConvertTo-Json`/`ConvertFrom-Json` not `jq`
- Native PowerShell pipelines — no bash idioms
