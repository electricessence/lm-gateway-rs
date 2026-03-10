# LM Gateway — Setup Guide

This guide covers first-time setup and ongoing key rotation for `lm-gateway-rs`.

---

## Clone Setup

After cloning, activate the repo's pre-commit hook (guards against committing
private hostnames, IPs, and credentials):

```bash
git config core.hooksPath .githooks
```

This is a one-time step per clone. The hook runs automatically on every `git commit`.

---

## Prerequisites

- A Linux server with Docker and Docker Compose installed
- Access to the server via SSH
- API keys for whichever cloud providers you want to use

---

## 1. Create the Stack Directory

```bash
sudo mkdir -p /opt/stacks/lm-gateway
cd /opt/stacks/lm-gateway
```

---

## 2. Get the Compose File

Copy [`stacks/claw-router/compose.yaml`](../../stacks/claw-router/compose.yaml) from this repo to the server, or write it directly:

```bash
sudo nano /opt/stacks/lm-gateway/compose.yaml
```

The compose file mounts two things:

- `config.toml` — routing config (tiers, aliases, backends)
- `.env` — API keys (never committed)

---

## 3. Create the Config File

Create `/etc/lm-gateway/config.toml` (or any path, then update the volume mount):

```bash
sudo mkdir -p /etc/lm-gateway
sudo nano /etc/lm-gateway/config.toml
```

Minimum viable config:

```toml
[gateway]
client_port = 8080
admin_port  = 8081

[backends.anthropic]
provider    = "anthropic"
base_url    = "https://api.anthropic.com"
api_key_env = "ANTHROPIC_API_KEY"

[[tiers]]
name    = "cloud:capable"
backend = "anthropic"
model   = "claude-haiku-4-5-20251001"

[aliases]
"hint:capable" = "cloud:capable"

[profiles.default]
mode               = "escalate"
classifier         = "cloud:capable"
max_auto_tier      = "cloud:capable"
expert_requires_flag = false
```

See [`config.example.toml`](../../config.example.toml) for the full annotated example.

---

## 4. Set API Keys (the secure way)

**Keys never go in `config.toml` or in the Docker image.** They live only in
`.env` on the server — a file that is never committed to git.

Create `/opt/stacks/lm-gateway/.env`:

```bash
sudo nano /opt/stacks/lm-gateway/.env
```

Contents (use only the providers you have keys for):

```env
ANTHROPIC_API_KEY=sk-ant-...
OPENROUTER_KEY=sk-or-...
```

Set restrictive permissions:

```bash
sudo chmod 600 /opt/stacks/lm-gateway/.env
sudo chown root:root /opt/stacks/lm-gateway/.env
```

---

## 5. Start the Gateway

```bash
cd /opt/stacks/lm-gateway
sudo docker compose pull
sudo docker compose up -d
```

---

## 6. Verify

```bash
# Liveness
curl http://localhost:8080/healthz

# Public metrics + readiness check
curl http://localhost:8080/status
```

A successful response with `"ready": true` means all backends with keys configured
have their keys resolved. If `ready: false`, check `.env` and restart.

---

## Updating an API Key

The gateway reads keys from `.env` at container startup. To rotate a key:

**Option A — Manual (recommended, most secure):**

```bash
# SSH to server
ssh user@your-server

# Edit the .env file
sudo nano /opt/stacks/lm-gateway/.env
# Update the key value, save, exit

# Restart the gateway to pick up the new key
cd /opt/stacks/lm-gateway
sudo docker compose restart lm-gateway

# Verify
curl http://localhost:8080/status
```

**Option B — PowerShell helper script (from your workstation):**

A helper script is provided in the `stacks/claw-router/` folder:

```powershell
# From your local machine (fills in your SSH key and server details)
.\stacks\claw-router\update-secret.ps1 -BackendKey ANTHROPIC_API_KEY -Value "sk-ant-..."
```

See the script's header comments for prerequisites.

---

## Security Model

| What | Where | Committed? |
| ---- | ----- | ---------- |
| API keys / secrets | `.env` on server | **Never** |
| Routing config (backends, tiers, aliases) | `config.toml` on server | Safe — no secrets |
| Docker image | GHCR (public) | Safe — no keys baked in |
| Stack compose + helper scripts | This repo | Safe — no values, only var names |

The gateway reads `api_key_env = "ANTHROPIC_API_KEY"` from `config.toml` as a variable
**name**, then resolves the actual value from the environment at runtime. The value is
held in memory only and never written to disk or returned by any API endpoint.

---

## Troubleshooting

**`ready: false` on `/status`**
One or more backends have `api_key_env` set but the env var is empty or missing.
Check that `.env` exists, has the correct key name, and the container was restarted after editing.

**`healthz` returns an error**
The binary is not listening. Check `docker compose logs lm-gateway`.

**Anthropic requests fail with 401**
The key in `.env` is invalid or expired. Rotate the key and restart.
