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
            // Handle both string and array-of-parts content (for multimodal requests).
            if let Some(content_val) = msg.get("content") {
                match content_val {
                    Value::String(text) => {
                        token_count += bpe.encode_ordinary(text).len();
                    }
                    Value::Array(parts) => {
                        for part in parts {
                            if let Some("text") = part.get("type").and_then(Value::as_str) {
                                if let Some(text) = part.get("text").and_then(Value::as_str) {
                                    token_count += bpe.encode_ordinary(text).len();
                                }
                            }
                        }
                    }
                    _ => {}
                }
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
        let mut entry = TrafficEntry::new("reply".into(), "none".into(), 0, false)
            .with_profile(profile_name)
            .with_requested_model(&model_hint)
            .with_routing_mode("reply");
        if let Some(id) = request_id {
            entry = entry.with_id(id);
        }
        entry = entry.with_priority(priority);
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

    state.traffic.push(entry.clone());

    Ok((stream_response, entry, is_native_ndjson))
}

// ---------------------------------------------------------------------------
// Helpers
// ---------------------------------------------------------------------------

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
mod tests {
    use super::*;
    use serde_json::json;

    use self::classify::{parse_classification, parse_classification_label, resolve_tier_by_label};
    use self::modes::is_sufficient;

    // -----------------------------------------------------------------------
    // is_sufficient — pure heuristic, no I/O required
    // -----------------------------------------------------------------------

    fn response_with_content(content: &str) -> Value {
        json!({
            "choices": [{
                "message": { "content": content }
            }]
        })
    }

    #[test]
    fn sufficient_for_normal_response() {
        let r = response_with_content("Here is a detailed explanation of how Rust lifetimes work.");
        assert!(is_sufficient(&r));
    }

    #[test]
    fn insufficient_when_content_is_very_short() {
        // Under 20 chars — likely a fragment, not a real answer
        assert!(!is_sufficient(&response_with_content("Sure.")));
        assert!(!is_sufficient(&response_with_content("")));
    }

    #[test]
    fn insufficient_when_model_refuses() {
        let refusals = [
            "I cannot help with that request.",
            "As an AI, I must decline to answer.",
            "I don't know the answer to your question.",
            "I'm not able to provide that information.",
            "I don't have enough information to respond accurately.",
        ];
        for phrase in refusals {
            assert!(
                !is_sufficient(&response_with_content(phrase)),
                "expected refusal to be insufficient: {phrase}"
            );
        }
    }

    #[test]
    fn refusal_detection_is_case_insensitive() {
        let r = response_with_content("AS AN AI language model, I cannot do that at all.");
        assert!(!is_sufficient(&r));
    }

    #[test]
    fn insufficient_when_choices_array_is_missing() {
        // Malformed response — treat as insufficient so we try again
        assert!(!is_sufficient(&json!({})));
        assert!(!is_sufficient(&json!({ "choices": [] })));
    }

    // -----------------------------------------------------------------------
    // route() — dispatch and escalate with mock backends
    // -----------------------------------------------------------------------

    use std::sync::Arc;

    use wiremock::matchers::{method, path};
    use wiremock::{Mock, MockServer, ResponseTemplate};

    use crate::{
        config::{BackendConfig, GatewayConfig, ProfileConfig, RoutingMode, TierConfig},
        traffic::TrafficLog,
    };

    async fn mock_state(server: &MockServer, mode: RoutingMode) -> RouterState {
        let config = crate::config::Config {
            gateway: GatewayConfig {
                client_port: 8080,
                admin_port: 8081,
                traffic_log_capacity: 100,
                log_level: None,
                rate_limit_rpm: None,
                admin_token_env: None,
                max_retries: None,
                retry_delay_ms: None,
                health_window: None,
                health_error_threshold: None,
                public_profile: None,
                request_timeout_ms: None,
                profile_dir: None,
            },
            backends: {
                let mut m = std::collections::HashMap::new();
                m.insert(
                    "mock".into(),
                    BackendConfig {
                        base_url: server.uri(),
                        api_key_env: None,
                        api_key_secret: None,
                        timeout_ms: 5_000,
                        provider: crate::config::Provider::default(),
                        default_options: None,
                    },
                );
                m
            },
            tiers: vec![
                TierConfig {
                    name: "local:fast".into(),
                    backend: "mock".into(),
                    model: "fast-model".into(),
                    think: None,
                    max_context_tokens: None,
                },
                TierConfig {
                    name: "cloud:economy".into(),
                    backend: "mock".into(),
                    model: "economy-model".into(),
                    think: None,
                    max_context_tokens: None,
                },
            ],
            aliases: {
                let mut m = std::collections::HashMap::new();
                m.insert("hint:fast".into(), "local:fast".into());
                m
            },
            profiles: {
                let mut m = std::collections::HashMap::new();
                m.insert(
                    "default".into(),
                    ProfileConfig {
                        mode,
                        classifier: "local:fast".into(),
                        max_auto_tier: "cloud:economy".into(),
                        expert_requires_flag: false,
                        rate_limit_rpm: None,
                        classifier_prompt: None,
                        classifier_think: None,
                        system_prompt: None,
                        rules: vec![],
                        ..Default::default()
                    },
                );
                m
            },
            clients: vec![],
        };
        RouterState::new(Arc::new(config), std::path::PathBuf::default(), Arc::new(TrafficLog::new(100)))
    }

    fn long_response(content: &str) -> serde_json::Value {
        json!({
            "choices": [{ "message": { "content": content } }]
        })
    }

    #[tokio::test]
    async fn dispatch_routes_to_resolved_tier_and_returns_response() {
        let server = MockServer::start().await;
        Mock::given(method("POST"))
            .and(path("/v1/chat/completions"))
            .respond_with(ResponseTemplate::new(200).set_body_json(long_response(
                "Here is a comprehensive answer that passes the sufficiency heuristic.",
            )))
            .mount(&server)
            .await;

        let state = mock_state(&server, RoutingMode::Dispatch).await;
        let body = json!({ "model": "hint:fast", "messages": [{"role": "user", "content": "hi"}] });

        let result = route(&state, body, None, None, 0, false, false).await;
        assert!(result.is_ok(), "dispatch failed: {:?}", result.err());

        let (resp, entry) = result.unwrap();
        assert!(resp.pointer("/choices/0/message/content").is_some());
        assert_eq!(entry.tier, "local:fast");
        assert_eq!(entry.backend, "mock");
        assert!(entry.success);
    }

    #[tokio::test]
    async fn dispatch_bumps_tier_when_context_window_exceeded() {
        let server = MockServer::start().await;
        Mock::given(method("POST"))
            .and(path("/v1/chat/completions"))
            .respond_with(ResponseTemplate::new(200).set_body_json(long_response(
                "Response from the bumped tier after context-window gating kicked in.",
            )))
            .mount(&server)
            .await;

        // Build a config where the first tier has a tiny max_context_tokens
        let config = crate::config::Config {
            gateway: GatewayConfig {
                client_port: 8080,
                admin_port: 8081,
                traffic_log_capacity: 100,
                log_level: None,
                rate_limit_rpm: None,
                admin_token_env: None,
                max_retries: None,
                retry_delay_ms: None,
                health_window: None,
                health_error_threshold: None,
                public_profile: None,
                request_timeout_ms: None,
                profile_dir: None,
            },
            backends: {
                let mut m = std::collections::HashMap::new();
                m.insert(
                    "mock".into(),
                    BackendConfig {
                        base_url: server.uri(),
                        api_key_env: None,
                        api_key_secret: None,
                        timeout_ms: 5_000,
                        provider: crate::config::Provider::default(),
                        default_options: None,
                    },
                );
                m
            },
            tiers: vec![
                TierConfig {
                    name: "tiny".into(),
                    backend: "mock".into(),
                    model: "tiny-model".into(),
                    think: None,
                    max_context_tokens: Some(10), // Very small — will overflow
                },
                TierConfig {
                    name: "big".into(),
                    backend: "mock".into(),
                    model: "big-model".into(),
                    think: None,
                    max_context_tokens: None, // No limit
                },
            ],
            aliases: {
                let mut m = std::collections::HashMap::new();
                m.insert("hint:fast".into(), "tiny".into());
                m
            },
            profiles: {
                let mut m = std::collections::HashMap::new();
                m.insert(
                    "default".into(),
                    ProfileConfig {
                        mode: RoutingMode::Dispatch,
                        classifier: "tiny".into(),
                        max_auto_tier: "big".into(),
                        expert_requires_flag: false,
                        rate_limit_rpm: None,
                        classifier_prompt: None,
                        classifier_think: None,
                        system_prompt: None,
                        rules: vec![],
                        ..Default::default()
                    },
                );
                m
            },
            clients: vec![],
        };
        let state = RouterState::new(
            Arc::new(config),
            std::path::PathBuf::default(),
            Arc::new(TrafficLog::new(100)),
        );

        // Send a request that resolves to "tiny" but whose tokens exceed 10
        let body = json!({
            "model": "hint:fast",
            "messages": [{"role": "user", "content": "This message is long enough to exceed the tiny tier context window limit easily."}]
        });

        let (_, entry) = route(&state, body, None, None, 0, false, false).await.unwrap();
        // Should have been bumped from "tiny" to "big"
        assert_eq!(entry.tier, "big", "expected context-window gating to bump from tiny to big");
    }

    #[tokio::test]
    async fn dispatch_resolves_direct_tier_name_without_alias() {
        let server = MockServer::start().await;
        Mock::given(method("POST"))
            .and(path("/v1/chat/completions"))
            .respond_with(ResponseTemplate::new(200).set_body_json(long_response(
                "Direct tier name resolved correctly to the right backend tier.",
            )))
            .mount(&server)
            .await;

        let state = mock_state(&server, RoutingMode::Dispatch).await;
        let body = json!({ "model": "cloud:economy", "messages": [] });

        let (_, entry) = route(&state, body, None, None, 0, false, false).await.unwrap();
        assert_eq!(entry.tier, "cloud:economy");
    }

    #[tokio::test]
    async fn escalate_returns_first_sufficient_response() {
        let server = MockServer::start().await;
        // First tier (local:fast) is sufficient
        Mock::given(method("POST"))
            .and(path("/v1/chat/completions"))
            .respond_with(ResponseTemplate::new(200).set_body_json(long_response(
                "This is a sufficient answer from the cheapest tier, no need to escalate further.",
            )))
            .mount(&server)
            .await;

        let state = mock_state(&server, RoutingMode::Escalate).await;
        let body = json!({ "model": "hint:fast", "messages": [] });

        let (_, entry) = route(&state, body, None, None, 0, false, false).await.unwrap();
        // Should have stopped at the first (cheapest) tier
        assert_eq!(entry.tier, "local:fast");
    }

    #[tokio::test]
    async fn route_records_entry_in_traffic_log() {
        let server = MockServer::start().await;
        Mock::given(method("POST"))
            .and(path("/v1/chat/completions"))
            .respond_with(ResponseTemplate::new(200).set_body_json(long_response(
                "Traffic log entry should be created for every successful route call.",
            )))
            .mount(&server)
            .await;

        let state = mock_state(&server, RoutingMode::Dispatch).await;
        let body = json!({ "model": "local:fast", "messages": [] });

        route(&state, body, None, None, 0, false, false).await.unwrap();

        let entries = state.traffic.recent(10).await;
        assert_eq!(entries.len(), 1);
        assert_eq!(entries[0].tier, "local:fast");
        assert!(entries[0].success);
    }

    #[tokio::test]
    async fn route_errors_when_no_profile_is_configured() {
        let state = RouterState::new(
            Arc::new(crate::config::Config {
                gateway: GatewayConfig {
                    client_port: 8080,
                    admin_port: 8081,
                    traffic_log_capacity: 10,
                    log_level: None,
                    rate_limit_rpm: None,
                    admin_token_env: None,
                    max_retries: None,
                    retry_delay_ms: None,
                    health_window: None,
                    health_error_threshold: None,
                    public_profile: None,
                    request_timeout_ms: None,
                    profile_dir: None,
                },
                backends: std::collections::HashMap::new(),
                tiers: vec![],
                aliases: std::collections::HashMap::new(),
                profiles: std::collections::HashMap::new(), // no default
                clients: vec![],
            }),
            std::path::PathBuf::default(),
            Arc::new(TrafficLog::new(10)),
        );

        let result = route(&state, json!({}), None, None, 0, false, false).await;
        assert!(result.is_err());
        assert!(result
            .unwrap_err()
            .to_string()
            .contains("no matching profile"));
    }

    #[tokio::test]
    async fn dispatch_falls_back_to_classifier_tier_on_unknown_model() {
        let server = MockServer::start().await;
        Mock::given(method("POST"))
            .and(path("/v1/chat/completions"))
            .respond_with(ResponseTemplate::new(200).set_body_json(long_response(
                "Fallback to classifier tier when model hint is unknown.",
            )))
            .mount(&server)
            .await;

        let state = mock_state(&server, RoutingMode::Dispatch).await;
        // "totally:unknown" exists in neither aliases nor tiers — should fall back to classifier
        let body = json!({ "model": "totally:unknown", "messages": [] });

        let (_, entry) = route(&state, body, None, None, 0, false, false).await.unwrap();
        // classifier is "local:fast"
        assert_eq!(entry.tier, "local:fast");
    }

    // -----------------------------------------------------------------------
    // parse_classification_label — pure, no I/O
    // -----------------------------------------------------------------------

    #[test]
    fn parse_label_simple_returns_simple_no_think() {
        let r = json!({"choices": [{"message": {"content": "simple"}}]});
        let (label, think) = parse_classification_label(&r);
        assert_eq!(label, "simple");
        assert!(think.is_none());
    }

    #[test]
    fn parse_label_trailing_punctuation_is_stripped() {
        let cases = [("complex.", "complex"), ("[deep]", "deep"), ("(moderate)", "moderate")];
        for (input, want) in cases {
            let r = json!({"choices": [{"message": {"content": input}}]});
            let (label, _) = parse_classification_label(&r);
            assert_eq!(label, want, "input: {input}");
        }
    }

    #[test]
    fn parse_label_think_suffix_sets_override() {
        for prefix in ["deep", "max", "instant"] {
            let content = format!("{prefix}-think");
            let r = json!({"choices": [{"message": {"content": content}}]});
            let (label, think) = parse_classification_label(&r);
            assert_eq!(label, prefix, "prefix: {prefix}");
            assert_eq!(think, Some(true));
        }
    }

    #[test]
    fn parse_label_no_think_suffix_has_no_override() {
        let r = json!({"choices": [{"message": {"content": "deep"}}]});
        let (label, think) = parse_classification_label(&r);
        assert_eq!(label, "deep");
        assert!(think.is_none());
    }

    #[test]
    fn parse_label_multiword_response_uses_first_token() {
        let r = json!({"choices": [{"message": {"content": "moderate. This task is complex"}}]});
        let (label, _) = parse_classification_label(&r);
        assert_eq!(label, "moderate");
    }

    #[test]
    fn parse_label_is_lowercased() {
        let r = json!({"choices": [{"message": {"content": "COMPLEX"}}]});
        let (label, _) = parse_classification_label(&r);
        assert_eq!(label, "complex");
    }

    #[test]
    fn parse_label_missing_content_falls_back_to_instant() {
        // Missing or empty classifier content now returns "instant" — the safe
        // lowest-cost tier — rather than an empty string that requires callers
        // to handle the edge case themselves.
        let (label, think) = parse_classification_label(&json!({}));
        assert_eq!(label, "instant");
        assert!(think.is_none());
    }

    // -----------------------------------------------------------------------
    // resolve_tier_by_label — pure, no I/O
    // -----------------------------------------------------------------------

    fn make_tiers(names: &[&str]) -> Vec<TierConfig> {
        names
            .iter()
            .map(|&n| TierConfig {
                name: n.to_owned(),
                backend: "b".into(),
                model: "m".into(),
                think: None,
                max_context_tokens: None,
            })
            .collect()
    }

    #[test]
    fn resolve_exact_full_name_matches() {
        let tiers = make_tiers(&["local:instant", "local:fast", "cloud:pro"]);
        let resolved = resolve_tier_by_label("local:fast", &tiers);
        assert_eq!(resolved.name, "local:fast");
    }

    #[test]
    fn resolve_suffix_after_colon_matches() {
        let tiers = make_tiers(&["local:instant", "local:fast", "cloud:pro"]);
        assert_eq!(resolve_tier_by_label("fast", &tiers).name, "local:fast");
        assert_eq!(resolve_tier_by_label("pro", &tiers).name, "cloud:pro");
        assert_eq!(resolve_tier_by_label("instant", &tiers).name, "local:instant");
    }

    #[test]
    fn resolve_unknown_label_falls_back_to_middle_tier() {
        // Routing is name-driven — unrecognised labels fall back to middle.
        // This includes generic words (simple, complex, moderate) that are not
        // tier names; the classifier_prompt is the contract for valid labels.
        let tiers = make_tiers(&["t0", "t1", "t2"]);
        for label in ["simple", "complex", "moderate", "totally_unknown", "haiku", "opus"] {
            assert_eq!(
                resolve_tier_by_label(label, &tiers).name,
                "t1",
                "expected unknown label '{label}' to fall back to middle tier"
            );
        }
    }

    #[test]
    fn resolve_single_tier_always_returns_it() {
        let tiers = make_tiers(&["only"]);
        for label in ["unknown_xyz", "exact_mismatch", ""] {
            assert_eq!(
                resolve_tier_by_label(label, &tiers).name,
                "only",
                "label: {label}"
            );
        }
    }

    #[test]
    fn resolve_exact_name_takes_priority_over_suffix() {
        // A tier named "fast" must be found by exact match, not confused with
        // any other tier whose name ends in ":fast".
        let tiers = make_tiers(&["local:fast", "fast", "cloud:pro"]);
        let resolved = resolve_tier_by_label("fast", &tiers);
        // Exact match for "fast" should win; "local:fast" has it as a suffix
        // but exact match is checked first.
        assert_eq!(resolved.name, "fast");
    }

    // -----------------------------------------------------------------------
    // inject_system_prompt — pure, no I/O
    // -----------------------------------------------------------------------

    #[test]
    fn inject_inserts_system_message_when_none_exists() {
        let mut body = json!({
            "messages": [{"role": "user", "content": "hello"}]
        });
        inject_system_prompt(&mut body, "Be helpful.");
        let msgs = body["messages"].as_array().unwrap();
        assert_eq!(msgs.len(), 2);
        assert_eq!(msgs[0]["role"], "system");
        assert_eq!(msgs[0]["content"], "Be helpful.");
        assert_eq!(msgs[1]["role"], "user");
    }

    #[test]
    fn inject_prepends_to_existing_system_message() {
        let mut body = json!({
            "messages": [
                {"role": "system", "content": "Original context."},
                {"role": "user",   "content": "hello"}
            ]
        });
        inject_system_prompt(&mut body, "Profile prompt.");
        let msgs = body["messages"].as_array().unwrap();
        // No new message — profile prompt merges into the existing system message.
        assert_eq!(msgs.len(), 2);
        assert_eq!(msgs[0]["content"], "Profile prompt.\n\nOriginal context.");
    }

    #[test]
    fn inject_prepends_to_empty_existing_system_message() {
        let mut body = json!({
            "messages": [{"role": "system", "content": ""}]
        });
        inject_system_prompt(&mut body, "Only this.");
        // Empty existing content — result should just be the injected prompt.
        assert_eq!(body["messages"][0]["content"], "Only this.");
    }

    #[test]
    fn inject_is_noop_when_messages_key_is_absent() {
        let mut body = json!({"model": "foo"});
        inject_system_prompt(&mut body, "prompt");
        assert!(body.get("messages").is_none());
    }

    // -----------------------------------------------------------------------
    // parse_classification — structured and legacy formats
    // -----------------------------------------------------------------------

    #[test]
    fn parse_classification_legacy_single_token() {
        let r = json!({"choices": [{"message": {"content": "fast"}}]});
        let p = parse_classification(&r);
        assert_eq!(p.tier_label, "fast");
        assert!(p.think_override.is_none());
        assert!(p.tags.is_empty());
    }

    #[test]
    fn parse_classification_structured_extracts_all_tags() {
        let r = json!({"choices": [{"message": {"content": "tier=fast intent=greeting domain=home"}}]});
        let p = parse_classification(&r);
        assert_eq!(p.tier_label, "fast");
        assert!(p.think_override.is_none());
        assert_eq!(p.tags.get("intent").map(String::as_str), Some("greeting"));
        assert_eq!(p.tags.get("domain").map(String::as_str), Some("home"));
    }

    #[test]
    fn parse_classification_tier_key_wins_over_bare_tokens() {
        // Even if there is a bare token before `tier=`, the explicit key wins.
        let r = json!({"choices": [{"message": {"content": "deep tier=fast"}}]});
        let p = parse_classification(&r);
        assert_eq!(p.tier_label, "fast", "tier= key should override bare token");
    }

    #[test]
    fn parse_classification_think_suffix_in_structured() {
        let r = json!({"choices": [{"message": {"content": "tier=deep-think intent=analysis"}}]});
        let p = parse_classification(&r);
        assert_eq!(p.tier_label, "deep");
        assert_eq!(p.think_override, Some(true));
        assert_eq!(p.tags.get("intent").map(String::as_str), Some("analysis"));
    }

    #[test]
    fn parse_classification_missing_tier_key_uses_first_bare_token() {
        let r = json!({"choices": [{"message": {"content": "deep intent=analysis"}}]});
        let p = parse_classification(&r);
        assert_eq!(p.tier_label, "deep");
        assert_eq!(p.tags.get("intent").map(String::as_str), Some("analysis"));
    }

    #[test]
    fn parse_classification_empty_response_defaults_to_instant() {
        let p = parse_classification(&json!({}));
        assert_eq!(p.tier_label, "instant");
        assert!(p.think_override.is_none());
        assert!(p.tags.is_empty());
    }

    #[test]
    fn parse_classification_label_delegates_to_parse_classification() {
        // Ensure the backward-compat wrapper produces the same label/think as
        // parse_classification for both legacy and structured inputs.
        let cases = [
            "fast",
            "deep-think",
            "tier=fast intent=greeting",
            "tier=deep-think domain=work",
        ];
        for content in cases {
            let r = json!({"choices": [{"message": {"content": content}}]});
            let p = parse_classification(&r);
            let (label, think) = parse_classification_label(&r);
            assert_eq!(label, p.tier_label, "content: {content}");
            assert_eq!(think, p.think_override, "content: {content}");
        }
    }

    // -----------------------------------------------------------------------
    // Token estimation and context-window gating
    // -----------------------------------------------------------------------

    #[test]
    fn estimate_tokens_from_messages() {
        let body = json!({
            "messages": [
                {"role": "system", "content": "You are a helpful assistant."},
                {"role": "user", "content": "Hello world"}
            ]
        });
        // BPE-based: actual tokens + per-message overhead + reply priming + 10% margin
        let tokens = estimate_request_tokens(&body);
        // "You are a helpful assistant." ≈ 6 tokens, "Hello world" ≈ 2 tokens,
        // roles ≈ 2 tokens, 2×4 message overhead + 2 priming = 10 overhead
        // Total ≈ 20, with margin ≈ 22. We just check plausible range.
        assert!((15..=35).contains(&tokens), "expected 15-35, got {tokens}");
    }

    #[test]
    fn estimate_tokens_includes_tools() {
        let body = json!({
            "messages": [{"role": "user", "content": "help"}],
            "tools": [{"type": "function", "function": {"name": "get_weather", "description": "Get weather"}}]
        });
        let tokens = estimate_request_tokens(&body);
        // "help" = 1 token, plus tool JSON tokenization, overhead, margin
        assert!(tokens > 5, "should include tool definition tokens, got {tokens}");
    }

    #[test]
    fn estimate_tokens_empty_body() {
        let body = json!({});
        assert_eq!(estimate_request_tokens(&body), 0);
    }

    #[test]
    fn find_min_tier_skips_small_context() {
        use crate::config::TierConfig;
        let tiers = vec![
            TierConfig { name: "small".into(), backend: "b".into(), model: "m".into(), think: None, max_context_tokens: Some(4096) },
            TierConfig { name: "medium".into(), backend: "b".into(), model: "m".into(), think: None, max_context_tokens: Some(32768) },
            TierConfig { name: "large".into(), backend: "b".into(), model: "m".into(), think: None, max_context_tokens: None },
        ];
        // 5000 tokens exceeds small (4096) but fits medium (32768)
        assert_eq!(find_min_tier_for_tokens(&tiers, 5000, 0), 1);
    }

    #[test]
    fn find_min_tier_fits_first() {
        use crate::config::TierConfig;
        let tiers = vec![
            TierConfig { name: "small".into(), backend: "b".into(), model: "m".into(), think: None, max_context_tokens: Some(8192) },
            TierConfig { name: "large".into(), backend: "b".into(), model: "m".into(), think: None, max_context_tokens: None },
        ];
        // 2000 tokens fits in small (8192)
        assert_eq!(find_min_tier_for_tokens(&tiers, 2000, 0), 0);
    }

    #[test]
    fn find_min_tier_uncapped_always_fits() {
        use crate::config::TierConfig;
        let tiers = vec![
            TierConfig { name: "uncapped".into(), backend: "b".into(), model: "m".into(), think: None, max_context_tokens: None },
        ];
        assert_eq!(find_min_tier_for_tokens(&tiers, 999999, 0), 0);
    }

    #[test]
    fn find_min_tier_all_too_small_falls_back_to_last() {
        use crate::config::TierConfig;
        let tiers = vec![
            TierConfig { name: "tiny".into(), backend: "b".into(), model: "m".into(), think: None, max_context_tokens: Some(1024) },
            TierConfig { name: "small".into(), backend: "b".into(), model: "m".into(), think: None, max_context_tokens: Some(2048) },
        ];
        // 10000 tokens exceeds both — falls back to last
        assert_eq!(find_min_tier_for_tokens(&tiers, 10000, 0), 1);
    }

    #[test]
    fn find_min_tier_respects_start_idx() {
        use crate::config::TierConfig;
        let tiers = vec![
            TierConfig { name: "small".into(), backend: "b".into(), model: "m".into(), think: None, max_context_tokens: Some(8192) },
            TierConfig { name: "medium".into(), backend: "b".into(), model: "m".into(), think: None, max_context_tokens: Some(32768) },
            TierConfig { name: "large".into(), backend: "b".into(), model: "m".into(), think: None, max_context_tokens: None },
        ];
        // start_idx=1 means we skip "small" entirely
        assert_eq!(find_min_tier_for_tokens(&tiers, 100, 1), 1);
    }
}
