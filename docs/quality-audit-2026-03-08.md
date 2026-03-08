# Quality Audit — lm-gateway-rs

**Date:** 2026-03-08  
**Scope:** Full codebase review — code quality, organization, documentation, security, tests, tooling.  
**Branch reviewed:** `feature/profile-directory`

---

## Executive Summary

The codebase is in excellent overall shape. Architecture is clean, module boundaries are well-drawn, documentation is thorough, and the operational concerns (security, observability, hot-reload, graceful shutdown) are all handled correctly. The items below are refinements and inconsistencies rather than fundamental problems.

---

## Organization

**Strengths:**
- Module layout (`api/`, `backends/`, `config/`, `router/`) cleanly separates concerns with no circular dependencies.
- File sizes are well-controlled and generally respect the ~500-line discipline.
- Tests co-located with production code per Rust convention; integration tests use `wiremock` appropriately.
- `tools/` script library follows the `Verb-Noun.ps1` convention; temporary scripts go in dated subdirectories.
- `etc/lm-gateway/profiles/` keeps live deployment profiles separate from the main config.

**Issues:**

| # | Severity | Location | Issue |
|---|----------|----------|-------|
| O-1 | Low | `src/router/modes.rs` | `classify_and_resolve` is ~260 lines — by far the longest function in the codebase. Acceptable today (overall file stays under 500 lines) but it's accumulating complexity (rule matching, context-window gating, cascade routing, tool-result bumping). Consider extracting internal helpers as complexity grows. |
| O-2 | Low | `src/router/modes.rs` | File size is approaching limit. Depending on what future features touch (overflow routing, identity priority), it may cross 500 lines. Worth tracking. |

---

## Naming & Branding

These violate the project's stated rule: "Do not let Claw-specific framing creep back."

| # | Severity | Location | Issue |
|---|----------|----------|-------|
| N-1 | **Medium** | `src/api/client/openai.rs:36`, `src/api/client/ollama.rs:94`, `src/router/modes.rs:308,350` | **`X-Claw-Expert` header** — still uses old branding. Every other custom header uses `X-LMG-*` prefix (`X-LMG-Priority`, `X-LMG-Tier`, `X-LMG-Profile`, `X-LMG-Class`). This should be `X-LMG-Expert`. The header name is part of the public API surface and appears in external-facing error messages like `"tier … requires the 'X-Claw-Expert: true' header"`. |

---

## Documentation

**Strengths:**
- All public items have doc comments — no exceptions found.
- `config.example.toml` is exhaustively annotated and covers every setting.
- `README.md` is accurate, organized, and leads with the general value proposition.
- `ROADMAP.md` is detailed and contains full API specs for planned features.
- `configuration.md` includes both narrative and a working Mermaid request-flow diagram.
- `CONTRIBUTING.md` is concise and actionable.

**Issues:**

| # | Severity | Location | Issue |
|---|----------|----------|-------|
| D-1 | **Medium** | `src/router/priority.rs:3` | Doc comment says `"See docs/gateway-priority-design.md for the full specification"` — **that file does not exist** in `docs/`. Either create it (summarizing the scheduling algorithm and design decisions, perhaps from the ROADMAP's priority sections) or remove the reference. |
| D-2 | Low | `src/config/profile.rs`, `RoutingMode::Classify` variant doc | Doc says the classifier "responds with one word (`simple`, `moderate`, or `complex`)" — these are the **legacy** label names. The current prompt and code now use `instant`, `fast`, `deep` (with `-think` variants). The doc comment should be updated to match reality. |
| D-3 | Low | `src/config/profile.rs`, `ProfileConfig::classifier_prompt` field doc | Same staleness: `"Respond should be exactly one of: simple, moderate, or complex"`. Should reference current vocabulary. |
| D-4 | Info | Repository root | No `CHANGELOG.md`. The ROADMAP documents past releases well (v0.1–v0.3 complete sections), but a canonical version history file makes it easier for users to understand what changed without reading the roadmap narrative. Low priority since GitHub releases serve this role. |

---

## Code Quality

**Strengths:**
- Idiomatic Rust throughout — `anyhow` error propagation, `tracing` spans, RAII for priority gates.
- Config validation at load time with clear, actionable error messages.
- Hot-reload is safe: fields that can't be updated at runtime are documented with "restart required" notes.
- `deep_merge` for TOML overlays is a neat and well-tested solution.
- Cycle detection for profile cascade routes runs at startup (DFS), not at request time.
- `classify_and_resolve` correctly uses `BoxFuture` to handle the recursive async case.

**Issues:**

| # | Severity | Location | Issue |
|---|----------|----------|-------|
| C-1 | **Medium** | `Cargo.toml:16` | `axum = { version = "0.8", features = ["ws"] }` — the `ws` (WebSocket) feature is compiled in but **no WebSocket code exists anywhere** in `src/`. WebSocket support in axum pulls in `tokio-tungstenite` and related deps. This increases compile time and binary size for no benefit. Remove `"ws"` from the features list unless it's planned imminently. |
| C-2 | Low | `src/router/mod.rs:65`, `estimate_request_tokens` | `tiktoken_rs::get_bpe_from_model("gpt-4o")` is called on every invocation. `tiktoken-rs` likely caches the encoder internally, but this is unverified — and more importantly the function is called **up to three times per classify-mode request** (rule path + label path + each having their own context-window gating block). If caching is not guaranteed, this is a hot-path allocation. Consider a `std::sync::OnceLock<CoreBPE>` at module level initialized once at startup. |
| C-3 | Low | `src/router/modes.rs` | `estimate_request_tokens(body)` is called redundantly. In `classify_and_resolve`, when a rule matches and the rule tier is within `candidates`, the token count is estimated. Then in the no-rule fallback branch, it's estimated again. When cascading doesn't occur, both branches independently estimate tokens from the same `body`. A single early computation (outside both branches) would suffice. |
| C-4 | Low | `src/api/client/mod.rs`, `proxy_sse` | Uses `.expect("proxy_sse: failed to build streaming response")`. `Response::builder()` only fails if header values are invalid, which can't happen with these hardcoded literal strings — so the panic risk is theoretical. However, `.expect()` in async handler code is generally a code smell. Could use `unwrap_or_else` → `StatusCode::INTERNAL_SERVER_ERROR` for belt-and-suspenders correctness. |
| C-5 | Info | `src/config/profile.rs`, `ProfileConfig::classifier` | The field is named `classifier` but serves **two different purposes** depending on mode: (1) in `classify` mode: the tier that performs pre-flight classification; (2) in `dispatch`/`escalate` mode: the fallback tier when model-hint resolution fails. Both uses are documented in the field's doc comment, which mitigates the confusion, but the field name implies only the first role. As the config surface grows, this dual use may trip up new contributors. A future rename to `default_tier` (with `classifier` as a deprecated alias) would clarify intent — but this is not urgent. |
| C-6 | Info | `src/router/priority.rs`, `GateState::try_unblock_next` | `pending.remove(0)` is O(n) due to element shifting. The in-code comment acknowledges this and justifies it for small queue sizes (typically < 10). Acceptable, but if gate contention ever grows (high-concurrency future), a `VecDeque` with `pop_front` would be the straightforward fix. |

---

## Security

**Strengths:**
- API keys are never stored in config — only env var names or file paths.
- Secret source supports Docker secrets (`source = "file"`) — no credential exposure in env.
- Admin and client ports are separated; admin port can be independently firewalled.
- Config redaction in `GET /admin/config` exposes only key-is-configured boolean, not key values.
- `GET /status` exposes no backend, tier, or model names — safe for public exposure.
- Dockerfile runs as non-root `gateway` user, uses `scratch` base (zero OS CVE surface), and patches Alpine packages before building.
- Cycle detection prevents infinite profile cascade loops from crashing the server.

**Issues:**

| # | Severity | Location | Issue |
|---|----------|----------|-------|
| S-1 | Low | `src/api/admin_auth.rs` | Admin token comparison uses `token == expected.as_str()` — not constant-time. `client_auth.rs` has an explicit rationale comment explaining why timing-safe comparison is unnecessary (timing attacks require millions of requests, visible in traffic log). `admin_auth.rs` has no such comment, leaving the same pattern undocumented and potentially surprising to a security reviewer. Add the matching rationale comment here. |
| S-2 | Low | `src/api/client/mod.rs`, error messages | Backend error classifications (e.g., `"tier local:fast requires the X-Claw-Expert: true header"`) leak internal tier names to clients. This is a deliberate UX tradeoff but worth a periodic review as tier names may carry environment-specific information. |
| S-3 | Info | `src/router/modes.rs`, `classify_and_resolve` | User message content is passed verbatim to the classifier. A user could craft a message to influence the routing label (prompt injection → tier escalation to a more expensive model). This is inherent to classify-mode design and the cost impact is bounded by `max_auto_tier`. No code change needed, but it should be acknowledged in documentation for operators who care about cost control. |

---

## Backend Adapters

| # | Severity | Location | Issue |
|---|----------|----------|-------|
| B-1 | Low | `src/backends/anthropic.rs`, `health_check` | Anthropic health check sends a probe to `"claude-haiku-4-5-20251001"`. This hardcoded model name is not derived from config. If Anthropic deprecates that model identifier, the health check begins returning errors even when the backend is healthy and the configured models are fine. Consider probing the first Anthropic model actually configured in `tiers`, or use the least-expensive model name from the backend's assigned tier. |
| B-2 | Info | `src/backends/` | No unit tests in `anthropic.rs`, `openai.rs`, or `ollama.rs`. The schema translation in `anthropic.rs` (OpenAI↔Anthropic) is the most complex backend logic and has no isolated test coverage — it's only exercised via full integration tests through `wiremock`. Dedicated unit tests for `to_anthropic()` / `from_anthropic()` and the SSE translation (`translate_sse_event`) would significantly improve confidence in that code. |

---

## Tests

**Strengths:**
- Good coverage on `error.rs`, `client_auth.rs`, `rate_limit.rs`, `status.rs`, `metrics.rs`, `config/mod.rs`, and `router/mod.rs`.
- `wiremock`-based integration tests for the routing path are thorough.
- Priority gate has its own test suite.
- `test-fixtures/security/` test inputs are well-organized and serve as a useful reference for security review scenarios.

**Issues:**

| # | Severity | Location | Issue |
|---|----------|----------|-------|
| T-1 | Low | `src/backends/anthropic.rs` | No tests for `to_anthropic`, `from_anthropic`, or `translate_sse_event`. These pure translation functions are ideal unit test targets. |
| T-2 | Info | `src/router/modes.rs` | Escalation heuristic (`is_sufficient`) is not independently tested. Covered incidentally through integration tests but isolated tests with known-good/bad response strings would make regressions more visible. |

---

## Tooling & Scripts

**Strengths:**
- PowerShell scripts follow the `Verb-Noun.ps1` convention.
- Live operations fully scripted (`Sync-LmGateway.ps1`, `Test-GatewayE2E.ps1`, etc.).
- Temporary scripts go in dated subdirectories — auditable and obviously ephemeral.

---

## Dependencies

| # | Severity | Item | Note |
|---|----------|------|------|
| Dep-1 | Low | `axum::ws` | See C-1 — unused feature should be removed. |
| Dep-2 | Info | `tiktoken-rs = "0.9"` | Heavy dependency used only for token estimation. If startup time or binary size ever becomes a concern, a lighter `bpe`-based estimator or a simple character-count heuristic (×0.75 tokens/char) could replace it. Not urgent — tiktoken gives accurate estimates with well-understood behaviour. |

---

## Summary Table

| ID | Severity | Category | One-line description |
|----|----------|----------|----------------------|
| N-1 | **Medium** | Naming | `X-Claw-Expert` header should be `X-LMG-Expert` |
| D-1 | **Medium** | Docs | `docs/gateway-priority-design.md` referenced but missing |
| C-1 | **Medium** | Code | Unused `axum::ws` feature wastes compile time and binary size |
| D-2 | Low | Docs | `RoutingMode::Classify` doc lists stale label vocabulary |
| D-3 | Low | Docs | `classifier_prompt` field doc lists stale label vocabulary |
| S-1 | Low | Security | Admin auth missing timing-attack rationale comment |
| S-2 | Low | Security | Some error messages leak tier names to clients |
| B-1 | Low | Backend | Anthropic health check hardcodes model name not from config |
| B-2 | Info | Testing | No unit tests for Anthropic schema translation |
| C-2 | Low | Code | BPE encoder initialization on every token-estimation call |
| C-3 | Low | Code | `estimate_request_tokens` called redundantly in classify path |
| C-4 | Low | Code | `proxy_sse` uses `.expect()` in async handler |
| C-5 | Info | Code | `ProfileConfig::classifier` field name implies only one role |
| C-6 | Info | Code | `pending.remove(0)` is O(n) — fine now, flag for future |
| O-1 | Low | Org | `classify_and_resolve` is ~260 lines — track complexity |
| O-2 | Low | Org | `modes.rs` approaching 500-line discipline limit |
| T-1 | Low | Testing | No unit tests for Anthropic SSE/schema translation |
| T-2 | Info | Testing | Escalation heuristic lacks isolated test coverage |
| S-3 | Info | Security | Classifier prompt injection can influence tier routing (by design; document for operators) |
| D-4 | Info | Docs | No `CHANGELOG.md` |
| Dep-1 | Low | Deps | See C-1 |
| Dep-2 | Info | Deps | `tiktoken-rs` is heavy for the task |

---

*Highest-impact fixes in order: N-1 (`X-LMG-Expert`), D-1 (create the priority design doc), C-1 (drop `axum::ws`), D-2/D-3 (stale doc strings), B-1 (Anthropic health probe model hardcode).*
