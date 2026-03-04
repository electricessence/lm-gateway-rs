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

### Semantic tagging — multi-label classification and rule-based routing

The current classifier emits a single routing label (`fast`, `deep`, etc.). A structured multi-tag output from the same classifier call unlocks a lightweight rules engine that can route on semantic intent rather than model difficulty alone.

**How it works (single LLM call):**

The classifier prompt is extended to produce `key=value` pairs alongside — or instead of — the bare tier label:

```
tier=fast intent=greeting
tier=fast intent=command domain=home
tier=deep intent=question domain=security complexity=hard
```

**Profile config adds a `rules` table (first-match wins, priority desc):**

```toml
[[profiles.default.rules]]
when     = { intent = "greeting" }
route_to = "local:fast"
priority = 30

[[profiles.default.rules]]
when     = { intent = "command", domain = "home" }
route_to = "local:fast"
priority = 20

[[profiles.default.rules]]
when     = { domain = "security" }
route_to = "cloud:deep"
priority = 10

# No rule matched → fall through to normal tier-label routing
```

**Design principles:**

- Tag keys and values are **user-defined** — the gateway matches strings, not a fixed schema. You design the taxonomy; the gateway evaluates it.
- `conf.d/` overlays let you add or override rules per deployment without touching the base config.
- Rules are a pre-pass: if a rule matches, the request is dispatched immediately and the normal difficulty-tier routing is skipped.
- The existing label output (`tier=fast`) is still honoured as fallback when no rules match.
- Backward-compatible: when no `rules` are configured, behaviour is identical to today.

**What changes in code (~200 lines new Rust):**

- `config/profile.rs`: add `RuleConfig { when: HashMap<String, String>, route_to: String, priority: i32 }` + `rules: Vec<RuleConfig>` on `ProfileConfig`
- `router/classify.rs`: extend `parse_classification_label` to extract `key=value` tags alongside the tier label
- `router/modes.rs`: rule-evaluation loop before tier dispatch in `classify_and_dispatch`

**Typical tag schemas:**

| Schema | Tags |
|--------|------|
| Home Assistant | `intent=command\|question\|status_query`, `domain=home\|sensor\|security\|general` |
| Generic assistant | `intent=greeting\|task\|fact\|code`, `complexity=trivial\|normal\|hard` |
| Custom | Anything — you define it in the classifier prompt |

**Classifier fallback (fits here naturally):**

When the classifier call fails outright (network error, timeout) or returns unusable output, fall back to a more reliable classifier tier rather than aborting or silently routing to the wrong place:

```toml
[profiles.default]
classifier          = "local:instant"  # try this first (fastest, cheapest)
classifier_fallback = "local:fast"     # retry with this on failure / empty output
```

On failure, fire once against `classifier_fallback`, parse the tags, then proceed normally. Zero extra latency on the happy path.

Note: the classifier is always a local model — its purpose is to avoid unnecessary cloud routing. Cloud tiers are never a classifier candidate.

---

### Traffic log export

The traffic ring buffer is in-memory only — it disappears on restart. Two opt-in export modes:

- **JSONL append**: write each completed request to a file (`traffic_log_path` in config)
- **Webhook**: POST each entry as JSON to a configurable URL (`traffic_webhook_url`)

Both are optional and fire async so they don't add latency to the request path.

---

### Profile cascade routing (hierarchical `route_to`)

Currently, `route_to` in classify-mode rules accepts a **tier name**. Allowing it to also accept a **profile name** enables two-level routing without any new API surface.

**How it works:**

When resolving `route_to`, the gateway checks if the value is a known profile name before checking tiers. If it is a profile, the request re-enters that profile's routing loop (same request, same process, same HTTP — no external round-trip). A depth counter prevents infinite cycles.

**Example — domain dispatch at the first level, complexity dispatch at the second:**

```toml
# Top-level domain router
[profiles.auto]
mode          = "classify"
classifier    = "local:instant"
classifier_prompt = """
Classify the domain. Reply with one label only.
home         : "Turn on the lights", "Is the door locked?"
code         : "Write a script", "Create a spreadsheet formula"
document     : "Write me a report", "Draft an email"
general      : "Tell me a joke", "Good morning"
"""

[[profiles.auto.rules]]
when     = { class = "home" }
route_to = "ha-auto"           # ← profile name, not a tier
priority = 30

[[profiles.auto.rules]]
when     = { class = "code" }
route_to = "code-auto"         # ← profile name
priority = 20

# (no rule for general → falls through to tier-label routing → local:instant)

# Second-level: code complexity router
[profiles.code-auto]
mode          = "classify"
classifier    = "local:instant"
max_auto_tier = "cloud:deep"
classifier_prompt = """
How complex is this coding task? Reply with one label only.
simple  : "Write a single function", "Fix this typo"
complex : "Architect a full system", "Create an Excel formula with VBA"
"""
```

This is additive and backward-compatible. Profiles without `route_to` pointing at other profiles work exactly as today.

**Cycle detection — two layers:**

1. **Config-load static check (preferred):** on startup, walk the full `route_to` graph across all profile rules using DFS. If a cycle is found, refuse to start and emit a clear error:
   ```
   Error: circular profile route detected: auto → code-auto → auto
   Fix: break the cycle — no profile may route to itself or to an ancestor
   ```
   This is the right place to catch misconfiguration — fail early, loud, and specifically.

2. **Runtime depth guard (safety net):** even if static validation passes, track hop count on each request (max 8). If exceeded, return a 500 with the routing trace in the error body. Catches any cycles that arise from dynamic aliasing at request time.

**What changes in code (~80 lines new Rust):**

- `config/mod.rs`: after loading all profiles, run a cycle check (DFS over the `route_to` → profile name graph); error out on startup if any cycle is found
- `router/modes.rs`: add a `depth: u8` parameter to `classify_and_dispatch`; increment on each profile re-entry; return a routing-trace error if depth exceeds the limit
- `config/profile.rs`: no change — profiles are already stored in a `HashMap<String, ProfileConfig>` accessible to the router

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
