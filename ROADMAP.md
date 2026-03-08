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

**v0.3 — Complete**

- **`X-LMG-Priority` scheduling**: per-tier priority gate implementing "fire if top, queue if not" —
  requests with `P > max(in_flight)` fire immediately; lower-priority requests queue in FIFO order.
  Priority logged in every `TrafficEntry`.
  Priority scale: `+N` = high (user traffic), `0` = normal, `-N` = background (audit pipeline).
  Header: `X-LMG-Priority: <integer>` on any request to the client API.
  *Note:* cloud providers (Anthropic, OpenRouter) currently bypass the gate via a hardcoded heuristic.
  This will be replaced by explicit `queue_id` config — see the Long Range priority section below.
- **`request_timeout_ms` gateway-level timeout**: hard wall-clock timeout applied to the entire
  dispatch attempt (all retries included). When a client disconnects mid-request, the backend future
  is dropped, the Ollama connection is closed, and the priority gate permit is released immediately —
  preventing ghost requests from jamming queued work.
  Configured in `[gateway]`; absent = no timeout (legacy behaviour unchanged).

---

## Short Range

> Targeted next

### Queue-depth overflow routing

When a high-priority request arrives and the target tier's queue is already deep, automatically
route to a faster fallback tier instead of waiting. Two control points:

**1. Per-tier config policy** — the gateway enforces a queue-depth ceiling per tier:

```toml
[[tiers]]
name     = "local:deep"
backend  = "ollama"
model    = "qwen3:14b"
# When the queue is this deep AND the request priority is at least 50,
# route to overflow_tier instead of queuing.
overflow_tier          = "cloud:haiku"  # any tier or alias; cloud or another local server
overflow_depth         = 2              # in-flight + pending > this → consider overflow
overflow_min_priority  = 50            # only overflow for requests at or above this priority
```

**2. Per-request header hint** — callers can express their own tolerance:

```
X-LMG-Max-Queue: 3
```

Means: "I'm willing to wait behind up to 3 in-flight requests. If the queue is deeper than that,
route me to the configured overflow tier instead."

Neither alone is sufficient — the config ceiling is what the gateway trusts; the header lets
callers be more conservative on a per-call basis. A request only overflows when **both** conditions
allow it (callers cannot exceed the tier's `overflow_min_priority` policy).

**Overflow destination can be anything:**
- A cloud model (`cloud:haiku`) for fast, low-latency responses
- Another local server with more capacity
- A less powerful but less congested local tier

**What changes in code (~120 lines new Rust):**

- `config/profile.rs` or `config/mod.rs`: add `overflow_tier`, `overflow_depth`,
  `overflow_min_priority` to `TierConfig` (all optional; overflow is disabled when unset)
- `router/priority.rs`: add `TierPriorityGate::depth() -> usize` method returning
  `in_flight.len() + pending.len()`
- `router/mod.rs`: before calling `dispatch()`, check overflow conditions and substitute the
  overflow tier if triggered; parse `X-LMG-Max-Queue` header alongside `X-LMG-Priority`
- `traffic.rs`: flag `overflowed: bool` on `TrafficEntry` so the admin UI can show when
  overflow routing fired

---

### Identity-assigned priority — server-side default and ceiling per client

The current implementation lets callers set their own priority via `X-LMG-Priority`. This is
acceptable for open environments but unsafe for multi-tenant deployments: a low-trust agent
could claim `X-LMG-Priority: 1000` and starve everyone else.

The fix is server-assigned priority tied to the authenticated identity. Each `[[clients]]` entry
gets two new optional fields:

```toml
[[clients]]
key_env          = "TELEGRAM_MCP_KEY"
profile          = "default"
default_priority = 100    # assigned when no X-LMG-Priority header is present
priority_ceiling = 100    # maximum priority this identity may claim (default = default_priority)
```

**Resolution logic (in handler, after auth middleware injects identity):**

1. Identity resolved → read `default_priority` (fallback: 0) and `priority_ceiling` (fallback: `default_priority`)
2. If `X-LMG-Priority` header present → `effective = min(ceiling, header_value)`
3. If header absent → `effective = default_priority`

A client identified as Telegram MCP gets priority 100 automatically — not because it declared
the header, but because the server recognises its key. Setting `priority_ceiling = 100` prevents
the client from requesting anything higher than 100, even if it tries.

**Flexible policy:**
- Locked identity: `default_priority = 100, priority_ceiling = 100` — fixed, client cannot alter it
- Trusted identity: `default_priority = 0, priority_ceiling = 200` — starts at normal priority, can boost for urgent tasks
- Untrusted identity: `default_priority = -10, priority_ceiling = 0` — background by default, cannot claim real-time slots

**What changes in code (~60 lines new Rust):**

- `config/mod.rs`: add `default_priority: i32` and `priority_ceiling: Option<i32>` to `ClientConfig`
- `api/client_auth.rs`: expand `ClientProfile` extension (or add `ClientPriority` extension) to
  carry `default_priority` and `priority_ceiling`
- `api/client/openai.rs` + `ollama.rs`: read identity priority from extension;
  `effective_priority = min(ceiling, parse_priority(&headers).unwrap_or(default_priority))`

For unauthenticated requests (no API key, `public_profile` path): priority defaults to 0.
IP-based priority lookup is a future extension — the identity model covers the authenticated
case cleanly first.

---

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

2. **Per-request visited set (runtime breadcrumbs):** each request carries the *set of profiles already traversed* in its routing path. Before re-entering a profile via `route_to`, check if that profile is already in the visited set. If so, skip the rule and continue to the next one (or fall through to tier-label routing).

   This is stronger than a raw depth counter: it prevents any indirect cycle dynamically, and it gives a semantically clear error — `"auto" already in routing chain: auto → code-auto → auto`. It also naturally produces the `X-LMG-Profile` trace header as a side effect — the visited set *is* the routing breadcrumb trail.

   Example flow for `auto → code-auto`:
   1. Request enters `auto` — visited = `{auto}`
   2. Rule matches `domain=code` → `route_to = "code-auto"` — `code-auto` not in visited → proceed
   3. Request re-enters `code-auto` — visited = `{auto, code-auto}`
   4. If any rule in `code-auto` points back to `auto` → skip (already visited)
   5. Falls through to tier dispatch → `cloud:deep`

**What changes in code (~80 lines new Rust):**

- `config/mod.rs`: after loading all profiles, run a cycle check (DFS over the `route_to` → profile name graph); error out on startup if any input cycle is found
- `router/modes.rs`: thread a `visited: HashSet<&str>` through `classify_and_dispatch`; insert profile name on entry; skip any `route_to` rule whose target is already in the set; populate `X-LMG-Profile` header from the set in traversal order
- `config/profile.rs`: no change — profiles are already stored in a `HashMap<String, ProfileConfig>` accessible to the router

---

### Routing trace headers

As a request travels through the gateway, attach response headers showing the full routing decision. Zero extra latency — the data is already computed internally.

**Proposed headers (returned on every response):**

| Header | Example |
|---|---|
| `X-LMG-Profile` | `auto → code-auto` (cascade path, or just `ha-auto` for single-hop) |
| `X-LMG-Class` | `class=code` (top-level) / `complexity=complex` (second hop) |
| `X-LMG-Tier` | `cloud:deep` |
| `X-LMG-Model` | `claude-sonnet-4-5` |
| `X-Request-ID` | `c4f3a2b1` (already implemented) |

These headers:
- Help clients/agents understand why they ended up on a given model
- Feed the in-memory traffic log automatically — the admin UI can display the full routing decision per request
- Provide opt-in observability without a separate tracing infrastructure
- Are a natural complement to profile cascade routing (without them, cascade hops are opaque to the caller)

Single-hop today: `X-LMG-Class: class=inquiry` / `X-LMG-Tier: local:moderate`  
Cascade: `X-LMG-Profile: auto → code-auto` / `X-LMG-Class: class=code; complexity=complex` / `X-LMG-Tier: cloud:deep`

**What changes in code (~40 lines new Rust):**

- `router/modes.rs`: collect `(profile, class, tier, model)` tuples during routing; attach as response headers before returning
- `traffic.rs`: extend `TrafficEntry` to store routing trace alongside existing fields

---

### Thinking messages — perceived performance for slow tiers

When a streaming request routes to a slow tier (deep, max), inject a brief acknowledgment
into the SSE stream *immediately* — before the backend's first token arrives. This eliminates
the dead silence during 10-30s model loads.

**Decision:** Experiments showed that generating dynamic prefixes via the 1.7b instant model
produces either over-specific echoes (repeating the user's words) or collapses to a single
repeated phrase. A static message pool per tier, randomly selected, is simpler, zero-latency,
and produces better UX.

**Predictive, not reactive.** The gateway knows the tier after classification. If that tier
has thinking messages configured, inject *immediately* — don't wait for a timeout. The model
*will* be slow; there's no reason to delay the acknowledgment.

**How it works:**

1. Request arrives with `stream: true` and routes to a tier that has thinking messages configured
2. Gateway immediately emits a randomly-selected message from the tier's pool
   (or the profile default) as synthetic `chat.completion.chunk` SSE events followed by `\n\n`
3. In parallel, the backend request proceeds normally
4. When the real backend tokens start flowing, continue the stream naturally

The user sees: `"Let me think about that..."` → [real response]

**Only for streaming requests.** Non-streaming clients (like Home Assistant) receive the
complete response — injecting prefix text would pollute the answer. The `stream: true` flag
gates this feature.

**Profile config:**

```toml
# Profile-level default (applies to any tier without a specific override)
thinking_message = "One moment..."

# Per-tier message pools (randomly selected; override the default)
[thinking_messages]
"local:deep" = [
    "That's a good one — bear with me.",
    "Let me think on that.",
    "Give me a moment to work through this.",
]
"local:max" = [
    "This will take a moment...",
    "Working through something complex.",
    "Let me dig into that for you.",
]
```

**Design principles:**
- Predictive injection — emit immediately based on tier, not on a timeout
- Opt-in per profile — agent profiles that make silent tool calls don't get it
- Fast tiers (instant, fast) typically don't need it — they respond in <2s
- Non-streaming requests (`stream: false`) are unaffected — prefix only applies to SSE streams
- The injected text becomes part of the response content (SSE is append-only)
- Per-tier overrides let deeper tiers signal more effort ("thinking" vs "got it")

**What changes in code (~80 lines new Rust):**

- `config/profile.rs`: add `thinking_message`, `thinking_messages` (HashMap<String, Vec<String>>)
- `router/mod.rs`: in `route_stream()`, if the resolved tier has a thinking message,
  prepend synthetic SSE chunks before the backend stream
- `backends/mod.rs`: no change — the wrapping happens at the router level

---

## Medium Range

### TLS for the admin port

The admin UI (port 8081) serves over plain HTTP. Fine on a private network; not acceptable across trust boundaries. The recommended pattern is termination via a reverse proxy (Caddy, nginx) — native TLS in the binary is a secondary option.

### Response caching (opt-in, request-scoped)

For deterministic or near-deterministic prompts, cache the response against a hash of the full request (model + messages + sampling params). Configurable TTL per profile. **Disabled by default; never shared across profiles.** Most useful for classification pipelines that ask the same question repeatedly.

---

## Long Range

### Priority-aware request queue

**Implemented in v0.3** as `X-LMG-Priority` header scheduling. The core gate is live.
See [v0.3 release notes](#v03--complete) and `src/router/priority.rs` for the implementation.

The items below describe further evolution of the priority system:

- **Configurable queue ID per tier** — remove the hardcoded cloud-bypass heuristic and replace with
  an explicit queue assignment. Every tier has a `queue_id` (defaults to the tier name):
  ```toml
  [[tiers]]
  name     = "cloud:haiku"
  backend  = "anthropic"
  model    = "claude-haiku-4-5"
  queue_id = "anthropic"        # share one gate across all Anthropic tiers
  # queue_id = ""               # empty string = no gate, fire immediately
  ```
  Multiple tiers sharing the same `queue_id` share one gate — useful for shaping total throughput
  to a provider without serialising individual tier queues. An empty or absent `queue_id` disables
  gating entirely (current cloud default). This replaces the hardcoded `is_cloud` heuristic with a
  policy the operator controls.

- **Tier default priority**: configure a default `priority` on a tier so all requests going to
  that tier inherit a base priority without the caller needing to set the header
- **Profile priority ceiling**: profiles can set `max_priority` to prevent callers from
  jumping ahead of other profiles' traffic
- **Queue depth limit + 429**: reject or return `429 Too Many Requests` when the pending queue
  exceeds a configurable depth, to bound memory usage under flood conditions
- **Streaming permit lifecycle**: for stream requests, hold the priority permit until the
  stream is fully consumed (today the permit is released at first-byte), giving a tighter
  "this GPU slot is occupied" guarantee for long-running streams

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

### Classifier auto-tuning routine

An automated calibration procedure that tunes classifier prompts and tier boundaries to the
operator's specific model inventory. When an operator installs lm-gateway-rs and configures
multiple tiers, a built-in `auto-tune` profile can iteratively optimise classification accuracy
without manual prompt engineering.

**How it works:**

1. **Discovery**: the auto-tuner inspects the config — what models are available, how many
   tiers are defined, what latency and capability characteristics each tier has.
2. **Synthetic benchmark**: generate a representative prompt set spanning trivial → complex
   difficulty levels. Send each through the classifier and record: classified tier, actual
   response quality, latency, and token usage.
3. **Evaluate**: compute accuracy (did the classifier route to the optimal tier?),
   overclassification rate (wasted expensive model time), and underclassification rate
   (weaker model than needed). Measure per-tier breakdown.
4. **Adjust**: based on the results, the auto-tuner modifies the classifier prompt —
   adjusting label descriptions, adding/removing examples, simplifying label count if the
   model can't distinguish fine-grained tiers. It may also recommend merging tiers or
   adjusting `max_auto_tier`.
5. **Repeat**: run the benchmark again with the new prompt. Loop until accuracy stabilises
   or a maximum iteration count is reached.
6. **Output**: write the optimised classifier prompt and recommended config changes to a
   report file. Optionally update the profile TOML directly (with operator approval).

**Key principles:**
- The auto-tuner uses the gateway's own routing infrastructure — no external tools needed
- Works with any model inventory — adapts label count to what the classifier can handle
- The procedure is an agentic loop: a set of instructions any AI agent (or human) can follow
- Results are reproducible — same models + same prompt set = same accuracy score
- Never destructive — reports recommendations; doesn't overwrite config without approval

**Trigger scenarios:**
- First install with multiple models → "Run `auto-tune` to optimise routing"
- Adding a new model tier → re-tune to include the new tier in classification
- Swapping a model (e.g. upgrading from 7b to 14b) → re-tune boundaries

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
