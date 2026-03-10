//! Request routing logic — the brain of lm-gateway.
//!
//! Four routing modes are supported:
//!
//! - **Dispatch** (`RoutingMode::Dispatch`): the `model` field in the request is
//!   resolved through aliases and tier names to a target tier, then forwarded
//!   directly. Predictable latency, no wasted backend calls. Unknown hints fall
//!   back to the profile's configured fallback tier.
//!
//! - **Escalate** (`RoutingMode::Escalate`): the cheapest tier is tried first.
//!   If the response passes the [`modes::is_sufficient`] heuristic it is returned;
//!   otherwise the next tier up is tried. This minimises cost for simple queries
//!   at the expense of higher tail latency on hard ones.
//!
//! - **Classify** (`RoutingMode::Classify`): a fast pre-flight inference call
//!   to the `classifier` tier determines request complexity (`simple`, `moderate`,
//!   or `complex`), which is mapped to the first, middle, or last tier in the
//!   profile's auto range. The main inference call is then dispatched to that tier.
//!   Adds ~200–600 ms latency from the classification call.
//!
//! - **Reply** (`RoutingMode::Reply`): returns a static response without calling
//!   any backend. Useful as a dead-end for overflow profiles that are not yet wired
//!   to a model.

use std::{
    collections::HashMap,
    path::PathBuf,
    sync::{Arc, RwLock},
};

use anyhow::Context;
use bytes::Bytes;
use serde_json::Value;
use tracing::debug;

use futures_util::StreamExt as _;

use crate::{
    api::rate_limit::RateLimiter,
    backends::{BackendClient, SseStream},
    config::{Config, RoutingMode},
    traffic::{TrafficEntry, TrafficLog},
};

use self::modes::{classify_and_dispatch, classify_and_resolve, dispatch, escalate, resolve_target_tier};

mod classify;
mod modes;
pub mod priority;

use priority::TierPriorityGate;

// ---------------------------------------------------------------------------
// Text extraction
// ---------------------------------------------------------------------------

/// Extract the text content from a single chat message `content` field.
///
/// Handles both plain string content and OpenAI-style multimodal array-of-parts
/// (`[{type:"text", text:"..."}, ...]`). Non-text parts (images, audio) are
/// skipped; multi-part arrays are joined with a single space. Returns `None`
/// when the field is absent or yields no text.
pub(crate) fn extract_message_text(msg: &Value) -> Option<String> {
    match msg.get("content")? {
        Value::String(s) => Some(s.clone()),
        Value::Array(parts) => {
            let text: String = parts
                .iter()
                .filter_map(|p| {
                    if p.get("type").and_then(Value::as_str) == Some("text") {
                        p.get("text").and_then(Value::as_str).map(str::to_owned)
                    } else {
                        None
                    }
                })
                .collect::<Vec<_>>()
                .join(" ");
            if text.is_empty() { None } else { Some(text) }
        }
        _ => None,
    }
}

// ---------------------------------------------------------------------------
// Token estimation
// ---------------------------------------------------------------------------

/// Token estimate for a JSON request body using BPE tokenization.
///
/// Uses the `o200k_base` tokenizer (GPT-4o family) as the reference encoder.
/// Modern BPE tokenizers agree within ~10-15% across model families, so this
/// gives a reliable estimate even for non-OpenAI models like Qwen and Llama.
///
/// A 10% safety margin is applied on top to ensure the estimate is a pessimistic
/// upper bound — we'd rather bump up a tier unnecessarily than overflow a
/// context window.
pub(crate) fn estimate_request_tokens(body: &Value) -> u32 {
    let bpe = tiktoken_rs::get_bpe_from_model("gpt-4o")
        .expect("gpt-4o BPE should be available in tiktoken_rs");

    let messages = body.pointer("/messages").and_then(Value::as_array);
    let tools = body.pointer("/tools").and_then(Value::as_array);

    let mut token_count: usize = 0;

    if let Some(msgs) = messages {
        for msg in msgs {
            // Per-message overhead (role, separators) — OpenAI uses ~4 tokens per message
            token_count += 4;
            // Handles both string and array-of-parts content (for multimodal requests).
            if let Some(text) = extract_message_text(msg) {
                token_count += bpe.encode_ordinary(&text).len();
            }
            if let Some(role) = msg.get("role").and_then(Value::as_str) {
                token_count += bpe.encode_ordinary(role).len();
            }
            // Tool call results can be large JSON blobs
            if let Some(tc) = msg.get("tool_calls") {
                let tc_str = tc.to_string();
                token_count += bpe.encode_ordinary(&tc_str).len();
            }
        }
        // Reply priming overhead
        token_count += 2;
    }

    if let Some(tool_defs) = tools {
        let defs_str = serde_json::to_string(tool_defs).unwrap_or_default();
        token_count += bpe.encode_ordinary(&defs_str).len();
    }

    // 10% safety margin — round up to ensure pessimistic upper bound
    (token_count as f64 * 1.1).ceil() as u32
}

/// Find the lowest tier whose `max_context_tokens` can fit the estimated token count.
///
/// Iterates the tier ladder from `start_idx` upward through `candidates`. Returns
/// the index of the first tier that fits, or `candidates.len() - 1` if none fit
/// (last tier is always used as a fallback — better to try than to reject).
///
/// Tiers without `max_context_tokens` set are assumed to fit any request.
pub(crate) fn find_min_tier_for_tokens(
    candidates: &[crate::config::TierConfig],
    estimated_tokens: u32,
    start_idx: usize,
) -> usize {
    for (i, candidate) in candidates.iter().enumerate().skip(start_idx) {
        match candidate.max_context_tokens {
            Some(max) if estimated_tokens > max => continue, // won't fit
            _ => return i, // fits or uncapped
        }
    }
    // Nothing fits — fall back to last tier (best chance of largest context)
    candidates.len().saturating_sub(1)
}

/// Shared application state injected into every request handler via [`axum::extract::State`].
pub struct RouterState {
    /// Atomically-swappable live config; the lock is held only for the duration
    /// of `Arc::clone`, so it never blocks request handling.
    config_lock: Arc<RwLock<Arc<Config>>>,
    /// Path to the config file on disk — used by the hot-reload background task.
    pub config_path: PathBuf,
    /// In-memory ring-buffer of recent requests, exposed through the admin API.
    pub traffic: Arc<TrafficLog>,
    /// Gateway start time — used to compute uptime for the public status endpoint.
    pub started_at: std::time::Instant,
    /// Optional per-IP rate limiter. `None` means rate limiting is disabled.
    ///
    /// Note: built once at startup from `config.gateway.rate_limit_rpm`.
    /// A config hot-reload will NOT update the rate limiter; restart required
    /// to change the RPM limit at runtime.
    pub rate_limiter: Option<Arc<RateLimiter>>,
    /// Bearer token required for admin API access.
    ///
    /// `None` means admin auth is disabled (port should then be firewalled).
    /// Resolved at startup from `config.gateway.admin_token_env`; not
    /// updated on hot-reload.
    pub admin_token: Option<String>,
    /// Maps resolved client API key values → profile names.
    ///
    /// Built at startup by reading each `[[clients]]` entry's `key_env`.
    /// An empty map means no client key auth is configured — all requests
    /// use the `default` profile (if present) or no profile.
    /// Not updated on hot-reload; restart required to pick up new client keys.
    pub client_map: HashMap<String, String>,

    /// Fallback profile for unauthenticated requests when `[[clients]]` are configured.
    ///
    /// When set, requests without a valid Bearer token are routed to this profile
    /// instead of receiving a 401. Enables open LAN access alongside keyed clients.
    /// Not updated on hot-reload.
    pub public_profile: Option<String>,

    /// Per-profile shared rate limiters, keyed by profile name.
    ///
    /// Built at startup from profiles that specify a non-zero `rate_limit_rpm`.
    /// Each limiter enforces a total-RPM quota shared across ALL clients that
    /// resolve to the same profile. Not updated on hot-reload.
    pub profile_limiters: HashMap<String, Arc<RateLimiter>>,

    /// Per-tier priority gates that enforce the "fire if top, queue if not" policy.
    ///
    /// Keyed by tier name. Built at startup from the configured tiers. Tiers added
    /// via hot-reload will not have a gate and will fire immediately (safe fallback).
    /// Not updated on hot-reload — restart required to gate newly added tiers.
    pub gates: HashMap<String, TierPriorityGate>,

    /// When `true`, attach full request bodies to traffic log entries.
    /// Requires the `debug-traffic` Cargo feature.
    ///
    /// Read once from config at startup in [`RouterState::new`] and cached here.
    /// **Not updated on hot-reload** — restart the process to pick up a change
    /// to `traffic_log_debug` in the gateway config.
    #[cfg(feature = "debug-traffic")]
    pub debug_traffic: bool,
}

impl RouterState {
    pub fn new(config: Arc<Config>, config_path: PathBuf, traffic: Arc<TrafficLog>) -> Self {
        let rate_limiter = config
            .gateway
            .rate_limit_rpm
            .filter(|&rpm| rpm > 0)
            .map(|rpm| Arc::new(RateLimiter::new(rpm)));
        let admin_token = config
            .gateway
            .admin_token_env
            .as_deref()
            .and_then(|var| std::env::var(var).ok())
            .filter(|t| !t.is_empty());
        let client_map: HashMap<String, String> = config
            .clients
            .iter()
            .filter_map(|c| {
                let key = std::env::var(&c.key_env).ok().filter(|k| !k.is_empty())?;
                Some((key, c.profile.clone()))
            })
            .collect();
        if !client_map.is_empty() {
            tracing::info!(count = client_map.len(), "loaded client key mappings");
        }
        let profile_limiters: HashMap<String, Arc<RateLimiter>> = config
            .profiles
            .iter()
            .filter_map(|(name, profile)| {
                let rpm = profile.rate_limit_rpm.filter(|&r| r > 0)?;
                Some((name.clone(), Arc::new(RateLimiter::new(rpm))))
            })
            .collect();
        if !profile_limiters.is_empty() {
            tracing::info!(count = profile_limiters.len(), "loaded per-profile rate limiters");
        }
        let public_profile = config.gateway.public_profile.clone();
        if let Some(ref p) = public_profile {
            tracing::info!(profile = %p, "public (unauthenticated) profile configured");
        }
        let gates: HashMap<String, TierPriorityGate> = config
            .tiers
            .iter()
            .map(|t| (t.name.clone(), TierPriorityGate::new()))
            .collect();
        tracing::debug!(count = gates.len(), "priority gates initialised");
        #[cfg(feature = "debug-traffic")]
        let debug_traffic = config.gateway.traffic_log_debug;
        #[cfg(feature = "debug-traffic")]
        if debug_traffic && admin_token.is_none() {
            tracing::warn!(
                "debug_traffic is enabled but no admin_token is configured — \
                 request bodies are accessible unauthenticated via /admin/traffic"
            );
        }
        Self {
            config_lock: Arc::new(RwLock::new(config)),
            config_path,
            traffic,
            started_at: std::time::Instant::now(),
            rate_limiter,
            admin_token,
            client_map,
            public_profile,
            profile_limiters,
            gates,
            #[cfg(feature = "debug-traffic")]
            debug_traffic,
        }
    }

    /// Returns a snapshot of the current live config.
    ///
    /// The `RwLock` is held only for the duration of `Arc::clone` (nanoseconds),
    /// so callers get a stable reference with no contention risk.
    pub fn config(&self) -> Arc<Config> {
        self.config_lock.read().expect("config lock poisoned").clone()
    }

    /// Atomically replaces the live config. Called only from the hot-reload task.
    pub fn replace_config(&self, new: Arc<Config>) {
        *self.config_lock.write().expect("config lock poisoned") = new;
    }
}

// ---------------------------------------------------------------------------
// Route entry points
// ---------------------------------------------------------------------------

/// Route a `/v1/chat/completions` request body to the appropriate backend tier.
///
/// - Resolves the `model` field through aliases and tier names.
/// - Selects a routing mode from the active [`crate::config::ProfileConfig`].
/// - Forwards the (rewritten) request and records a [`TrafficEntry`].
///
/// Returns the raw JSON response from the winning backend, plus the traffic entry
/// so callers can surface per-request metadata (e.g. via response headers).
#[tracing::instrument(
    skip(state, request_body),
    fields(
        profile = profile_name.unwrap_or("default"),
        tier = tracing::field::Empty,
    )
)]
pub async fn route(
    state: &RouterState,
    mut request_body: Value,
    profile_name: Option<&str>,
    request_id: Option<&str>,
    priority: i32,
    stream: bool,
    expert_gate: bool,
) -> anyhow::Result<(Value, TrafficEntry)> {
    let profile_name = profile_name.unwrap_or("default");
    let config = state.config();
    let profile = config
        .profile(profile_name)
        .context("no matching profile and no default profile configured")?;

    // Reply mode: return a static response without resolving tiers or calling backends.
    if profile.mode == RoutingMode::Reply {
        let model_hint = request_body.get("model")
            .and_then(|v| v.as_str())
            .unwrap_or("unknown")
            .to_owned();
        let msg = profile.reply_message.as_deref().unwrap_or(DEFAULT_REPLY_MESSAGE);
        let mut entry = TrafficEntry::new("reply".into(), "none".into(), 0, true)
            .with_profile(profile_name)
            .with_requested_model(&model_hint)
            .with_routing_mode("reply");
        if let Some(id) = request_id {
            entry = entry.with_id(id);
        }
        entry = entry.with_priority(priority);
        #[cfg(feature = "debug-traffic")]
        if state.debug_traffic {
            entry = entry.with_debug_request_body(request_body.clone());
        }
        state.traffic.push(entry.clone());
        return Ok((build_reply_response(msg), entry));
    }

    let (mut target_tier, model_hint) =
        resolve_target_tier(&config, profile, &request_body, expert_gate)?;

    // Context-window gating for dispatch mode only. Classify and escalate modes
    // handle their own gating inside classify_and_resolve() / escalate().
    if profile.mode == RoutingMode::Dispatch {
        let estimated_tokens = estimate_request_tokens(&request_body);
        let tier_idx = config.tiers.iter().position(|t| t.name == target_tier.name).unwrap_or(0);
        let min_idx = find_min_tier_for_tokens(&config.tiers, estimated_tokens, tier_idx);
        if min_idx > tier_idx {
            let bumped = &config.tiers[min_idx];
            debug!(
                estimated_tokens,
                from = %target_tier.name,
                to = %bumped.name,
                "context-window floor — bumping tier"
            );
            target_tier = bumped;
        }
    }

    tracing::Span::current().record("tier", target_tier.name.as_str());

    // Inject the profile system prompt before dispatching to any backend.
    if let Some(prompt) = profile.system_prompt.as_deref() {
        inject_system_prompt(&mut request_body, prompt);
    }

    let (response, entry) = match profile.mode {
        RoutingMode::Dispatch => {
            dispatch(state, &mut request_body, target_tier, priority, stream).await?
        }
        RoutingMode::Escalate => {
            escalate(state, &mut request_body, profile, priority, stream).await?
        }
        RoutingMode::Classify => {
            classify_and_dispatch(state, &mut request_body, profile_name, priority, stream).await?
        }
        RoutingMode::Reply => unreachable!("reply mode handled above"),
    };

    // Enrich entry with request-level context only available at route() scope,
    // then record it in the traffic log.
    let mut entry = entry
        .with_profile(profile_name)
        .with_requested_model(&model_hint)
        .with_routing_mode(match profile.mode {
            RoutingMode::Dispatch => "dispatch",
            RoutingMode::Escalate => "escalate",
            RoutingMode::Classify => "classify",
            RoutingMode::Reply => "reply",
        });
    if let Some(id) = request_id {
        entry = entry.with_id(id);
    }
    entry = entry.with_priority(priority);
    #[cfg(feature = "debug-traffic")]
    if state.debug_traffic {
        entry = entry.with_debug_request_body(request_body.clone());
    }

    state.traffic.push(entry.clone());

    Ok((response, entry))
}

/// Route a streaming `/v1/chat/completions` request.
///
/// Streaming bypasses escalation — the first matching tier is dispatched to
/// directly, and the backend's SSE output is returned as an [`SseStream`].
/// In `classify` mode a non-streaming pre-flight call determines which tier to
/// stream from; escalation mode falls back to dispatch behaviour.
/// All backends produce OpenAI-compatible SSE: OpenAI-compatible and Ollama
/// backends proxy bytes verbatim; Anthropic translates on-the-fly.
#[tracing::instrument(skip(state, request_body), fields(profile = profile_name.unwrap_or("default")))]
pub async fn route_stream(
    state: &RouterState,
    mut request_body: Value,
    profile_name: Option<&str>,
    request_id: Option<&str>,
    priority: i32,
    expert_gate: bool,
    use_native: bool,
) -> anyhow::Result<(SseStream, TrafficEntry, bool)> {
    let profile_name = profile_name.unwrap_or("default");
    let config = state.config();
    let profile = config
        .profile(profile_name)
        .context("no matching profile and no default profile configured")?;

    // Reply mode: return a synthetic SSE stream without resolving tiers or calling backends.
    if profile.mode == RoutingMode::Reply {
        let model_hint = request_body.get("model")
            .and_then(|v| v.as_str())
            .unwrap_or("unknown")
            .to_owned();
        let msg = profile.reply_message.as_deref().unwrap_or(DEFAULT_REPLY_MESSAGE);
        let stream = build_reply_sse_stream(msg);
        let mut entry = TrafficEntry::new("reply".into(), "none".into(), 0, true)
            .with_profile(profile_name)
            .with_requested_model(&model_hint)
            .with_routing_mode("reply");
        if let Some(id) = request_id {
            entry = entry.with_id(id);
        }
        entry = entry.with_priority(priority);
        #[cfg(feature = "debug-traffic")]
        if state.debug_traffic {
            entry = entry.with_debug_request_body(request_body.clone());
        }
        state.traffic.push(entry.clone());
        return Ok((stream, entry, false));
    }

    let (mut resolved_tier, model_hint) =
        resolve_target_tier(&config, profile, &request_body, expert_gate)?;

    // Context-window gating for dispatch mode only (classify/escalate handle it internally).
    if profile.mode == RoutingMode::Dispatch {
        let estimated_tokens = estimate_request_tokens(&request_body);
        let tier_idx = config.tiers.iter().position(|t| t.name == resolved_tier.name).unwrap_or(0);
        let min_idx = find_min_tier_for_tokens(&config.tiers, estimated_tokens, tier_idx);
        if min_idx > tier_idx {
            let bumped = &config.tiers[min_idx];
            debug!(
                estimated_tokens,
                from = %resolved_tier.name,
                to = %bumped.name,
                "context-window floor — bumping tier (stream)"
            );
            resolved_tier = bumped;
        }
    }

    // Inject the profile system prompt before dispatching to any backend.
    if let Some(prompt) = profile.system_prompt.as_deref() {
        inject_system_prompt(&mut request_body, prompt);
    }

    // In classify mode, run a non-streaming pre-flight call through classify_and_resolve,
    // which handles rule evaluation and profile cascade routing, then stream from the
    // resolved tier.  This path now shares all routing logic with the non-streaming path.
    let (target_tier_name, routing_trace): (String, Option<(String, Vec<String>)>) =
        if profile.mode == RoutingMode::Classify {
            let visited = vec![profile_name.to_owned()];
            let resolution = classify_and_resolve(state, &request_body, profile_name, visited).await?;
            // Apply per-class system prompt from the final profile in the cascade chain.
            let final_profile_name = resolution.profile_chain.last().map(String::as_str).unwrap_or(profile_name);
            if let Some(final_profile) = config.profiles.get(final_profile_name) {
                if let Some(class_prompt) = final_profile.class_prompts.get(resolution.class_label.as_str()) {
                    inject_system_prompt(&mut request_body, class_prompt);
                }
            }
            // Inject think override before streaming dispatch.
            if let Some(t) = resolution.think_override {
                if let Some(obj) = request_body.as_object_mut() {
                    obj.insert("think".into(), Value::Bool(t));
                }
            }
            debug!(
                tier = %resolution.tier_name,
                label = %resolution.class_label,
                chain = ?resolution.profile_chain,
                "stream classify resolved"
            );
            let trace = (resolution.class_label, resolution.profile_chain);
            (resolution.tier_name, Some(trace))
        } else {
            (resolved_tier.name.clone(), None)
        };

    let target_tier = config
        .tiers
        .iter()
        .find(|t| t.name == target_tier_name)
        .with_context(|| format!("resolved tier `{target_tier_name}` not found"))?;

    let backend_cfg = config
        .backends
        .get(&target_tier.backend)
        .with_context(|| format!("backend `{}` not in config", target_tier.backend))?;

    if let Some(obj) = request_body.as_object_mut() {
        obj.insert("model".into(), Value::String(target_tier.model.clone()));
        obj.insert("stream".into(), Value::Bool(true));
        // Inject the tier's think preference as fallback; per-request overrides
        // (from -think classifier labels) are already in request_body and take precedence.
        if let Some(think) = target_tier.think {
            obj.entry("think").or_insert(Value::Bool(think));
        }
    }

    debug!(tier = %target_tier.name, backend = %target_tier.backend, "streaming dispatch");

    // Snapshot the request body before the client call consumes it.
    #[cfg(feature = "debug-traffic")]
    let debug_body: Option<Value> = if state.debug_traffic { Some(request_body.clone()) } else { None };

    let client = BackendClient::new(backend_cfg)?;
    let t0 = std::time::Instant::now();

    // Detect tool-call requests: Ollama's /v1/chat/completions compat layer fails
    // to translate <tool_call> output to a tool_calls JSON array.  Route through
    // the native /api/chat endpoint instead, which does the translation correctly.
    let has_tools = request_body
        .pointer("/tools")
        .and_then(Value::as_array)
        .map(|t| !t.is_empty())
        .unwrap_or(false);

    let (stream_response, is_native_ndjson) = if use_native {
        client.native_chat_stream(request_body).await?
    } else if has_tools {
        debug!("request has tools — routing via native /api/chat to fix tool_call translation");
        client.tool_call_stream(request_body).await?
    } else {
        (client.chat_completions_stream(request_body).await?, false)
    };
    let latency_ms = t0.elapsed().as_millis() as u64;

    let routing_mode = match profile.mode {
        RoutingMode::Classify => "classify+stream",
        _ => "stream",
    };

    // Latency here is time-to-first-byte (connection + headers), not full response.
    let mut entry = TrafficEntry::new(
        target_tier.name.clone(),
        target_tier.backend.clone(),
        latency_ms,
        true,
    )
    .with_profile(profile_name)
    .with_requested_model(&model_hint)
    .with_routing_mode(routing_mode);
    if let Some(id) = request_id {
        entry = entry.with_id(id);
    }
    if let Some((class_label, profile_chain)) = routing_trace {
        entry = entry.with_routing_trace(class_label, profile_chain);
    }
    entry = entry.with_priority(priority);
    #[cfg(feature = "debug-traffic")]
    if let Some(body) = debug_body {
        entry = entry.with_debug_request_body(body);
    }

    state.traffic.push(entry.clone());

    // Experimental: thinking message — inject a synthetic prefix chunk for perceived
    // responsiveness.  Works for streaming chat UIs; HA voice buffers the full response
    // so the prefix gets concatenated into the spoken answer instead of rendering early.
    let stream_response = if let Some(pool) = profile.thinking_messages.get(&target_tier_name) {
        if let Some(msg) = pick_thinking_message(pool) {
            let prefix = if is_native_ndjson {
                let chunk = serde_json::json!({
                    "model": target_tier.model,
                    "message": { "role": "assistant", "content": format!("{msg} ") },
                    "done": false
                });
                Bytes::from(format!("{chunk}\n"))
            } else {
                let ts = std::time::SystemTime::now()
                    .duration_since(std::time::UNIX_EPOCH)
                    .unwrap_or_default()
                    .as_secs();
                // Known limitation: the synthetic prefix chunk carries a different `id`
                // than the backend stream chunks that follow. The OpenAI spec requires all
                // chunks in a completion to share the same `id`; most clients tolerate the
                // mismatch, but strict implementations may treat them as separate completions.
                let chunk = serde_json::json!({
                    "id": format!("chatcmpl-thinking-{ts}"),
                    "object": "chat.completion.chunk",
                    "created": ts,
                    "model": target_tier.model,
                    "choices": [{
                        "index": 0,
                        "delta": { "role": "assistant", "content": format!("{msg} ") },
                        "finish_reason": null
                    }]
                });
                Bytes::from(format!("data: {chunk}\n\n"))
            };
            let prefix_stream = futures_util::stream::once(async move { Ok(prefix) });
            Box::pin(prefix_stream.chain(stream_response)) as SseStream
        } else {
            stream_response
        }
    } else {
        stream_response
    };

    Ok((stream_response, entry, is_native_ndjson))
}

// ---------------------------------------------------------------------------
// Helpers
// ---------------------------------------------------------------------------

/// Pick a pseudo-random thinking message from the pool.
///
/// Uses nanosecond timestamp instead of a full RNG crate — good enough for
/// UI variety, not for cryptography.
fn pick_thinking_message(pool: &[String]) -> Option<&str> {
    if pool.is_empty() {
        return None;
    }
    let nanos = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap_or_default()
        .subsec_nanos() as usize;
    Some(&pool[nanos % pool.len()])
}

/// Default message returned by reply-mode profiles when no custom message is set.
const DEFAULT_REPLY_MESSAGE: &str =
    "This request cannot be processed. No backend is configured for this profile.";

/// Build a synthetic OpenAI-format `chat.completion` response for reply mode.
///
/// Returns a [`Value`] that looks exactly like a real completion — the caller
/// (agent, client) can parse it without special-casing the reply path.
fn build_reply_response(msg: &str) -> Value {
    serde_json::json!({
        "id": "reply",
        "object": "chat.completion",
        "model": "reply",
        "choices": [{
            "index": 0,
            "message": { "role": "assistant", "content": msg },
            "finish_reason": "stop"
        }],
        "usage": { "prompt_tokens": 0, "completion_tokens": 0, "total_tokens": 0 }
    })
}

/// Build a synthetic SSE stream for reply mode (streaming callers).
///
/// Emits a single `chat.completion.chunk` followed by `[DONE]`, matching
/// the OpenAI streaming wire format so the consumer never knows it wasn't
/// a real backend response.
fn build_reply_sse_stream(msg: &str) -> SseStream {
    let chunk = serde_json::json!({
        "id": "reply",
        "object": "chat.completion.chunk",
        "model": "reply",
        "choices": [{
            "index": 0,
            "delta": { "role": "assistant", "content": msg },
            "finish_reason": "stop"
        }]
    });
    let payload = format!("data: {chunk}\n\ndata: [DONE]\n\n");
    let stream = futures_util::stream::once(async move {
        Ok(Bytes::from(payload))
    });
    Box::pin(stream)
}

/// Prepend the profile system prompt into the request's messages array.
///
/// If the first message already has `role = "system"`, the profile prompt is
/// placed before its content (separated by `\n\n`), so client-provided context
/// is preserved while the profile's instructions take precedence.
/// If there is no existing system message, one is inserted at index 0.
pub(super) fn inject_system_prompt(body: &mut Value, prompt: &str) {
    let Some(messages) = body.pointer_mut("/messages").and_then(Value::as_array_mut) else {
        return;
    };

    if let Some(first) = messages.first_mut() {
        if first.get("role").and_then(Value::as_str) == Some("system") {
            let existing = first
                .get("content")
                .and_then(Value::as_str)
                .unwrap_or("")
                .to_owned();
            let merged = if existing.is_empty() {
                prompt.to_owned()
            } else {
                format!("{prompt}\n\n{existing}")
            };
            if let Some(obj) = first.as_object_mut() {
                obj.insert("content".into(), Value::String(merged));
            }
            return;
        }
    }

    messages.insert(0, serde_json::json!({ "role": "system", "content": prompt }));
}

#[cfg(test)]
mod tests;

