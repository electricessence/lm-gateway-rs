# LM Gateway RS

![LM Gateway RS](docs/banner.png)

> **Ultra-lightweight LLM routing gateway written in Rust.**  
> Single binary. No Python. No database. No bloat.

[![License: MIT](https://img.shields.io/badge/license-MIT-blue.svg)](LICENSE)
[![Rust](https://img.shields.io/badge/rust-1.85%2B-orange.svg)](https://www.rust-lang.org)

---

## What it does

LM Gateway RS sits between your application and your LLM backends, providing a unified OpenAI-compatible interface across any number of local or cloud models. It handles credential management, tier-based routing, and intelligent escalation — so your application stays simple.

---

## Why LM Gateway RS?

- Ships as a **single static binary** — `docker run` and done
- **Zero external runtime dependencies** — no Python, no database, no daemon
- Fits on a **Raspberry Pi or a $5 VPS**
- Can be **audited in an afternoon** — under 2 000 lines of Rust
- **100% self-hosted** — no telemetry, no cloud account, no phone-home

---

## Features

- **OpenAI-compatible API** — drop-in replacement endpoint for any client that speaks `/v1/chat/completions`
- **Tier ladder** — define a cheapest→best progression of models, from local Ollama to cloud experts
- **Three routing modes:**
  - **Dispatch** — classify intent with a fast local model, forward to the right tier immediately (predictable latency)
  - **Escalate** — try cheapest tier first; evaluate response quality; escalate only if needed (lowest average cost)
  - **Classify** — single pre-flight call labels complexity as `simple`/`moderate`/`complex`, then dispatches directly to the appropriate tier (ideal for all-local deployments)
- **Ollama-compatible endpoints** — `GET /api/tags` and `POST /api/chat` let any Ollama client (Home Assistant, Open WebUI, etc.) point at this gateway without modification
- **Centralised credential management** — backends reference env vars; clients need no API keys
- **Live admin UI** — dark dashboard at `:8081` with real-time traffic log, backend health, and config view
- **In-memory traffic log** — ring-buffer; zero disk I/O, bounded memory, works on read-only filesystems

---

## Quick start

```bash
# 1. Copy and edit the example config
cp config.example.toml config.toml
$EDITOR config.toml

# 2. Set secrets via environment variables (never in the config file)
export OPENROUTER_KEY="sk-or-..."

# 3. Run
docker run --rm \
  -v $(pwd)/config.toml:/etc/lm-gateway/config.toml:ro \
  -e OPENROUTER_KEY \
  -p 8080:8080 -p 8081:8081 \
  lm-gateway:latest
```

Then send a request:

```bash
curl http://localhost:8080/v1/chat/completions \
  -H "Content-Type: application/json" \
  -d '{"model":"hint:fast","messages":[{"role":"user","content":"Hello"}]}'
```

Open the admin UI: `http://localhost:8081/`

---

## Client API (port 8080)

| Method | Path | Description |
| ------ | ---- | ----------- |
| `POST` | `/v1/chat/completions` | Route a chat request (OpenAI-compatible) |
| `GET` | `/v1/models` | List available tiers and aliases |
| `GET` | `/api/tags` | List models — Ollama-compatible format |
| `POST` | `/api/chat` | Chat inference — Ollama-compatible format |
| `GET` | `/healthz` | Liveness probe |

Use any tier name or alias as the `model` field:

```json
{
  "model": "hint:fast",
  "messages": [{ "role": "user", "content": "Hello" }]
}
```

Built-in aliases: `hint:fast`, `hint:cheap`, `hint:local`, `hint:cloud`, `hint:standard`, `hint:expert`

---

## Admin API (port 8081)

| Method | Path | Description |
| ------ | ---- | ----------- |
| `GET` | `/` | Admin dashboard (web UI) |
| `GET` | `/admin/health` | Gateway health + tier/backend counts |
| `GET` | `/admin/traffic?limit=N` | Recent N requests + aggregate stats |
| `GET` | `/admin/config` | Running config (secrets redacted) |
| `GET` | `/admin/backends/health` | Probe all configured backends |

---

## Configuration

See [config.example.toml](config.example.toml) for a fully annotated example. A typical setup is under 50 lines.

**Key concepts:**

| Concept | What it is |
| ------- | ---------- |
| **Backend** | A named LLM provider — base URL + optional secret env var |
| **Tier** | A named (backend, model) pair in cheapest→best order |
| **Alias** | Short name like `hint:fast` that resolves to a tier |
| **Profile** | Routing behaviour: mode, classifier tier, cost ceiling |

**Environment variable for config path:**

```bash
LMG_CONFIG=/path/to/config.toml   # default: /etc/lm-gateway/config.toml
```

---

## Use cases

- **Homelab / private LLM deployments** — route between local Ollama and cloud fallback
- **AI agent clusters** — serve multiple agents through a single credential-holding gateway
  - Works as-is with [ZeroClaw](https://github.com/zeroclaw-labs/zeroclaw) and any OpenAI-compatible agent framework
- **Cost optimisation** — escalate to expensive cloud models only when local models can't answer
- **Development environments** — keep all API keys in one place, share across projects

---

## Classify mode

Classify mode performs a single, fast non-streaming call to a cheap "classifier" tier before the real request. The classifier responds with a single word — `simple`, `moderate`, or `complex` — and the gateway routes the actual request to:

| Label | Tier selected |
| ----- | ------------- |
| `simple` | `tiers[0]` (cheapest / fastest) |
| `moderate` | `tiers[n/2]` (mid-tier) |
| `complex` | `tiers[n-1]` (most capable) |

This gives you predictable, low-latency routing without needing cloud infrastructure. A 1.7b model classifying adds ~50–150 ms on the first call, then the right model answers.

Minimal local config:

```toml
[backends.ollama]
provider = "ollama"
base_url = "http://127.0.0.1:11434"

[[tiers]]
name = "local:instant"   # maps to: simple
backend = "ollama"
model = "qwen3:1.7b"

[[tiers]]
name = "local:balanced"  # maps to: moderate
backend = "ollama"
model = "qwen3:8b"

[[tiers]]
name = "local:expert"    # maps to: complex
backend = "ollama"
model = "qwen3:14b"

[profiles.default]
mode       = "classify"
classifier = "local:instant"
```

---

## Ollama compatibility

Two Ollama-format endpoints are always available on the client port:

| Endpoint | Purpose |
| -------- | ------- |
| `GET /api/tags` | Returns all tiers and aliases in Ollama `GET /api/tags` format |
| `POST /api/chat` | Accepts an Ollama chat request; routes through the normal pipeline; returns an Ollama response (or NDJSON stream) |

This means you can point **Home Assistant**, **Open WebUI**, or any other Ollama client directly at this gateway. The client sees a list of "models" (your tier names and aliases) and can send requests without knowing anything about the underlying routing.

```text
Home Assistant → GET  lm-gateway-host:8080/api/tags  → sees "qwen-auto", "local:instant", …
               → POST lm-gateway-host:8080/api/chat  → classify mode routes transparently
```

---

## Building

```bash
# Local development (uses platform TLS — schannel on Windows, OpenSSL on Linux)
cargo build

# Production Docker image
# Uses rustls (pure-Rust TLS, no OpenSSL) for a fully static binary.
# Cap RAM for low-memory hosts.
docker build --memory=3g --build-arg CARGO_BUILD_JOBS=2 -t lm-gateway .
```

The release binary is statically linked and has no runtime dependencies beyond libc.

---

## Project layout

```text
src/
├── main.rs          Startup, dual listeners, graceful shutdown
├── config.rs        Config types, TOML loading, validation
├── router.rs        Routing logic (dispatch + escalate + classify modes)
├── traffic.rs       In-memory ring-buffer traffic log
├── error.rs         Unified error type
├── backends/
│   ├── mod.rs       BackendClient enum dispatcher
│   ├── openai.rs    OpenAI / OpenAI-compatible passthrough
│   ├── ollama.rs    Ollama adapter (keyless)
│   └── anthropic.rs Anthropic schema translation
└── api/
    ├── mod.rs       Router assembly
    ├── health.rs    GET /healthz
    ├── client.rs    POST /v1/chat/completions, GET /v1/models,
    │                GET /api/tags, POST /api/chat (Ollama compat)
    ├── admin.rs     Admin endpoints
    └── admin_ui.html Single-page admin dashboard
```

---

## Design principles

- **One job** — route LLM traffic. Nothing else.
- **No magic** — config is a single TOML file; behaviour is deterministic and auditable.
- **Small surface** — no database, no agent, no scheduler. A process that starts fast and uses < 10 MB RAM at idle.
- **Transparent** — config endpoint redacts secrets; traffic log captures routing decisions.
- **Upstream-friendly** — clean Rust, idiomatic error handling, documented public API surface.

---

## License

MIT — see [LICENSE](LICENSE).
