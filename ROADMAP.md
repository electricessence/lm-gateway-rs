# lm-gateway-rs Roadmap

A lightweight, single-binary LLM routing gateway in Rust. No Python. No database. No bloat.

> This is a living document. Items move as priorities clarify.  
> Contributions and discussion welcome — open an issue.

---

## Where We Are Today

**v0.1 — Stable**

- Single binary, zero runtime dependencies
- Transparent multi-backend routing (Anthropic, OpenAI-compatible, Ollama, OpenRouter)
- Tier-based escalation: route cheapest-first, escalate when needed
- Model aliasing: expose simple names (`hint:fast`, `hint:capable`) regardless of backend
- Per-client API keys mapped to named routing profiles
- Profile-level routing policies: mode, classifier tier, max auto-escalation tier
- In-memory traffic log (ring buffer) — no disk I/O
- `GET /status` — zero-leak public metrics (uptime, request counts, error rate)
- `GET /` admin UI — live traffic table, backend health, profiles, config view (no secrets)
- `GET /v1/models` — OpenAI-compatible model list; returns all configured tier names and aliases so standard clients auto-discover routing targets
- TOML config under 50 lines for a full production setup
- Docker image under 15 MB (`scratch` base, static musl binary)

**v0.2 — Complete**

- **Anthropic streaming**: on-the-fly SSE translation — `stream: true` works end-to-end with Anthropic backends
- **`GET /metrics`**: Prometheus-compatible scrape endpoint (TYPE gauge; ring-buffer windowed stats)
- **Config hot-reload**: `POST /admin/reload` applies config changes without restart; `↺ Reload` button in admin UI
- **Request ID tracing**: `X-Request-ID` propagated or generated per request; matches traffic log entry IDs
- **Per-IP rate limiting**: token bucket on the client port; configurable via `rate_limit_rpm`
- **Admin Bearer token auth**: all admin routes optionally protected via `admin_token_env`
- **Retry / backend failover**: configurable `max_retries` + `retry_delay_ms`; automatic failover to next tier on error
- **Backend health tracking**: recent-window error-rate snapshot; degraded backends skipped during escalation
- **Per-profile rate limits**: shared RPM quota per profile — all keys mapped to the same profile share a single bucket
- **Pluggable secret backends**: `api_key_secret = { source = "env", var = "..." }` or `{ source = "file", path = "..." }` — supports Docker secrets, Kubernetes mounts, any file-based store
- **Admin dashboard improvements**: backend cards show live health + traffic error rate; profiles section; secret source badge (env/file); setup warning banner when keys are unresolved

---

## Short Range

> Targeted next

### Traffic log export

The traffic ring buffer is in-memory only — it disappears on restart. Two opt-in export modes:

- **JSONL append**: write each completed request to a file (`traffic_log_path` in config)
- **Webhook**: POST each entry as JSON to a configurable URL (`traffic_webhook_url`)

Both are optional and fire async so they don't add latency to the request path.

---

## Medium Range

### TLS for the admin port

The admin UI (port 8081) serves over plain HTTP. Fine on a private network; not acceptable across trust boundaries. The recommended pattern is termination via a reverse proxy (Caddy, nginx) — native TLS in the binary is a secondary option.

### Response caching (opt-in, request-scoped)

For deterministic or near-deterministic prompts, cache the response against a hash of the full request (model + messages + sampling params). Configurable TTL per profile. **Disabled by default; never shared across profiles.** Most useful for classification pipelines that ask the same question repeatedly.

---

## Long Range

### Priority-aware request queue

A per-model request queue with priority scheduling. The gateway holds back requests to avoid overloading the backend, and processes them in priority order so interactive traffic is never starved by batch work.

**Core idea:** every request carries a `priority` value (integer, default `0`). Lower numbers execute first. The gateway drains all priority-0 requests before starting priority-1, and so on. Tiers can define a default priority so that classification and interactive requests naturally jump ahead of background work.

**Typical priority mapping:**
| Priority | Use case | Example |
|----------|----------|---------|
| `0` (default) | Classification pre-flights, instant-tier | Router classify calls, greetings |
| `1` | Fast-tier interactive | Device commands, short explanations |
| `2` | Deep-tier interactive | Complex analysis, code generation |
| `3+` | Background / batch | Email classification, document labelling |

**"Light" mode is just high-priority-number.** There is no separate model or CPU-only tier. A client sends a request with `priority: 3` (or uses a `light:` alias that maps to an existing tier with `default_priority = 3`). The gateway queues it behind all interactive work. The same model handles it — it just waits its turn.

**Design sketch:**

```toml
# Tier-level default priority (clients can override downward but not upward)
[[tiers]]
name     = "local:instant"
backend  = "ollama"
model    = "qwen3:1.7b"
priority = 0

[[tiers]]
name     = "local:balanced"
backend  = "ollama"
model    = "qwen3:4b"
priority = 1

[[tiers]]
name     = "local:expert"
backend  = "ollama"
model    = "qwen3:8b"
priority = 2

# "light" alias — same model, low priority
[aliases]
"light:expert" = "local:expert"   # priority overridden to 3 by profile
```

Key design questions:
- **Queue per model vs. global**: per-model queues are simpler and match Ollama's internal scheduler (which already serialises within a model). A global queue would allow cross-model priority but adds complexity.
- **Concurrency limit**: configurable `max_concurrent` per model (default 1 for local, unlimited for cloud backends). Ollama already serialises GPU inference, so the gateway queue prevents piling up HTTP connections that would just block.
- **Priority ceiling**: profiles can set `max_priority` to prevent clients from jumping the queue — a batch profile would have `default_priority = 3` and `max_priority = 3` (can't escalate to 0).
- **Queue depth limit**: reject or 429 when the queue exceeds a configurable depth, so a flood of batch requests doesn't consume unbounded memory.
- **Cloud backends skip the queue**: external backends (Anthropic, OpenRouter) have their own rate limits and don't contend for local GPU — requests to cloud tiers bypass the local queue entirely.
- **API surface**: `priority` field in the request body (OpenAI-compat extension), or set via profile/tier defaults. No new endpoints needed.

### Deeper model access

Explore options for accessing more capable models within the 17.6 GiB Vulkan VRAM budget on the current hardware:

- **Quantisation**: smaller quants (Q3_K_S, IQ3_XS) of larger models — e.g. qwen3:14b at IQ3_XS (~6 GiB) might fit alongside 1.7b + 4b
- **Offloading**: partial GPU + CPU offload (`num_gpu = N` layers) for a 14b+ model — slow but functional for deep-tier requests where latency is less critical
- **External backends**: route deep-tier to a cloud provider (Anthropic, OpenRouter) for tasks that genuinely need 70b+ capability; the gateway already supports this via backend config
- **Hardware upgrade**: a dedicated GPU card in the Proxmox host would provide a separate VRAM pool; even a used 16 GB card would double available capacity

---

## Vision

lm-gateway-rs is built on a simple principle: **the deployment model should never become the problem.** One binary. One config file. Zero external state. Runs anywhere.

No Python runtime. No database. No framework you have to understand before you can understand the router. The source is small enough to read in an afternoon, and the config is under 50 lines for a full production setup.

The routing intelligence grows over time — semantic routing based on prompt content, automatic cost/quality tradeoffs, backend reliability tracking. But the shape of the thing stays the same.

---

## Not In Scope

- A database (the traffic log is an in-memory ring buffer by design)
- A Python runtime or scripting layer
- A UI for configuring routing rules (config file is the interface)
- Autonomous model fine-tuning or training
