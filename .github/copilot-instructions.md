# GitHub Copilot Instructions — lm-gateway-rs

> A minimal LLM routing gateway written in Rust. Single binary. No Python. No database. No bloat.

---

## ⚠️ No Secrets / No PII

**NEVER commit:**
- API keys, tokens, passwords, or any credentials
- Real hostnames, IP addresses, or internal server names
- Personal information (names, emails, phone numbers)
- SSH key paths or machine-specific paths

Use environment variables. Reference env var **names**, never **values**.

---

## Project Identity

This is a **general-purpose LLM routing gateway**. It is not a Claw-specific product.

- Users are: developers, homelab operators, anyone who wants a lightweight LLM proxy
- Use case mentions like "AI agent clusters" or "ZeroClaw" are valid examples, not the primary framing
- The README and docs lead with the general value proposition: lightweight, single binary, works anywhere

Do not let Claw-specific framing creep back into comments, docs, or APIs.

---

## Design Principles (Non-Negotiable)

1. **Single binary** — no runtime dependencies beyond libc. Runs anywhere.
2. **No Python** — forbidden. The entire point of this project is to not be LiteLLM.
3. **No database** — the traffic log is an in-memory ring buffer. No disk I/O required.
4. **File size discipline** — keep source files under ~500 lines. Split when approaching that limit.
5. **Small surface** — every feature must earn its place. Resist scope creep.
6. **Transparent config** — TOML, under 50 lines for a full production setup.

---

## Code Standards

- Idiomatic Rust — `anyhow` for errors, `tracing` for observability, `tokio` for async
- Every public item has a doc comment
- Tests live in the same file as the code they test (Rust convention)
- Follow the existing patterns in `router.rs`, `traffic.rs`, `config.rs`

---

## Build Constraints

Docker builds for Rust on low-RAM hosts:
```
docker build --memory=3g --build-arg CARGO_BUILD_JOBS=2 -t lm-gateway .
```

Never exceed 2 parallel Cargo jobs in Docker. The host has limited RAM.

---

## Script Library — Reusable `.ps1` Tools

**Always create named `.ps1` scripts** for repeatable operations — never leave work as ad-hoc terminal commands. Scripts are **self-documenting**: they capture not just what ran but why, with parameters, comments, and structure that makes patterns reusable and auditable by any agent or operator.

- **Naming**: `Verb-Noun.ps1` (PowerShell convention). E.g. `Deploy-GatewayConfig.ps1`, `Test-HaPrompts.ps1`.
- **Location**: `tools/<scope>/` for persistent scripts. Temporary/one-off scripts go in `tools/temp/MMDD/description.ps1` (e.g. `tools/temp/0710/debug-gateway.ps1`) — subfolder per day, auditable, obviously ephemeral.
- **Structure**: `#Requires -Version 7`, `[CmdletBinding()]`, comment-based help, `param()` block, `$ErrorActionPreference = 'Stop'`.
- **Fully parameterized**: Every environment-specific value (SSH alias, LXC ID, ports, model names) must be a parameter with a sensible default. Scripts should work for different people and environments.
- **No hardcoded secrets**: Credentials and connection details come from parameters or environment variables.

Full PowerShell quality standards → `.github/instructions/powershell.instructions.md`

---

## Commit & Push Procedure

See `.github/instructions/phased-commit.instructions.md` for the full procedure.

**Summary:** Stage → Critical Review → Security Audit → Commit → **await explicit push approval** → request fresh Copilot review if resolving PR comments.
