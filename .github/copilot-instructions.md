# GitHub Copilot Instructions — lm-gateway-rs

> A minimal LLM routing gateway written in Rust. Single binary. No Python. No database. No bloat.

---

## ⚠️ No Secrets / No PII — Absolute Rule

**NEVER commit:**
- API keys, tokens, passwords, or any credentials
- Real hostnames, IP addresses, or internal server names
- Personal information (names, emails, phone numbers)
- SSH key paths or machine-specific paths

Use environment variables. Reference env var **names**, never **values**.

---

## ⚠️ No Infrastructure-Specific Tooling — Absolute Rule

**NEVER add to this repo:**
- Deploy scripts (`Push-ToLxc.ps1`, `Sync-*.ps1`, etc.)
- SSH wrappers or LXC management scripts
- Operational test runners that target a specific server
- `.env.ps1` / `.env.example.ps1` files with host aliases or container IDs
- Any file that assumes a particular server, LXC setup, or SSH topology

**Why:** This is a public, general-purpose project. Infrastructure-specific tooling leaks deployment context and does not belong here. It belongs in a separate private ops/infrastructure repo.

If you find yourself writing a script that contains `$SshAlias`, `$LxcId`, or any host-specific default — stop and put it in your private infrastructure repo instead.

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

## Commit & Push Procedure

See `.github/instructions/phased-commit.instructions.md` for the full procedure.

**Summary:** Stage → Critical Review → Security Audit → Commit → **await explicit push approval** → request fresh Copilot review if resolving PR comments.

---

## Config Deploy Discipline

Config files deployed to production (`etc/lm-gateway/config.toml`) must **always** originate from the repo. Never edit the live server config without the change being in the repo first.

- **Stage immediately** after any config change that will be deployed — even if not ready to commit yet. This prevents accidental loss during future syncs.
- **Deploy from repo** — use your preferred sync mechanism (e.g. `scp`, `rsync`, or a private deploy script) to push repo files to the server. The repo is the source of truth.
- **Profile deletion = explicit intent** — removing a profile section from the config requires a clear justification (not an accidental side-effect of a large rewrite).
