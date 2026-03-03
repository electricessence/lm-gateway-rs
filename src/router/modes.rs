//! Routing execution modes: dispatch, escalate, and classify-then-dispatch.
//!
//! Each mode is a self-contained async function called by [`super::route`] or
//! [`super::route_stream`] after the active profile and target tier have been
//! resolved.  [`is_sufficient`] is the heuristic that drives escalation.

use anyhow::Context;
use serde_json::Value;
use tracing::{debug, warn};

use crate::{
    backends::BackendClient,
    config::{Config, ProfileConfig, TierConfig, DEFAULT_CLASSIFIER_PROMPT},
    traffic::TrafficEntry,
};

use super::{
    RouterState,
    classify::{parse_classification, ParsedClassification, resolve_tier_by_label},
};

/// Resolve the model hint in the request body to a concrete [`TierConfig`].
///
/// Alias indirection is applied first (`hint:fast` → `local:fast`). When the
/// hint is unrecognised, the profile's fallback tier is used. If
/// `profile.expert_requires_flag` is `true` and the resolved tier sits above
/// `max_auto_tier` in the ladder, the request is rejected unless `expert_gate`
/// is `true` (i.e. the client sent `X-Claw-Expert: true`).
///
/// Returns the resolved tier and the original model hint string (needed for
/// traffic log annotations).
pub(super) fn resolve_target_tier<'a>(
    config: &'a Config,
    profile: &crate::config::ProfileConfig,
    request_body: &Value,
    expert_gate: bool,
) -> anyhow::Result<(&'a TierConfig, String)> {
    let model_hint = request_body
        .get("model")
        .and_then(Value::as_str)
        .unwrap_or("hint:fast")
        .to_owned();

    let target_tier: &TierConfig = match config.resolve_tier(&model_hint) {
        Some(tier) => tier,
        None => {
            warn!(%model_hint, "unknown model/alias — falling back to fallback tier");
            config
                .tiers
                .iter()
                .find(|t| t.name == profile.classifier)
                .context("fallback tier not found in config")?
        }
    };

    // Enforce expert gate: tiers above max_auto_tier require an explicit opt-in.
    if profile.expert_requires_flag && !expert_gate {
        let max_idx = config
            .tiers
            .iter()
            .position(|t| t.name == profile.max_auto_tier)
            .unwrap_or_else(|| config.tiers.len().saturating_sub(1));
        let tier_idx = config
            .tiers
            .iter()
            .position(|t| t.name == target_tier.name)
            .unwrap_or(0);
        if tier_idx > max_idx {
            anyhow::bail!(
                "tier `{}` requires the `X-Claw-Expert: true` header",
                target_tier.name
            );
        }
    }

    Ok((target_tier, model_hint))
}

/// Mode A: direct dispatch to a known tier.
///
/// Rewrites `model` and `stream` in the request body and forwards to the
/// backend.  Retries up to `config.gateway.max_retries` times on failure, with
/// exponential backoff starting at `config.gateway.retry_delay_ms` (default
/// 200 ms), doubling per attempt, capped at 2 000 ms.
pub(super) async fn dispatch(
    state: &RouterState,
    body: &mut Value,
    tier: &TierConfig,
    stream: bool,
) -> anyhow::Result<(Value, TrafficEntry)> {
    let config = state.config();
    let backend_cfg = config
        .backends
        .get(&tier.backend)
        .with_context(|| format!("backend `{}` not in config", tier.backend))?;

    // Rewrite the model field to the backend's model name
    if let Some(obj) = body.as_object_mut() {
        obj.insert("model".into(), Value::String(tier.model.clone()));
        obj.insert("stream".into(), Value::Bool(stream));
        // Inject the tier's think preference, but only as a fallback —
        // a per-request think override set by the classifier (via -think labels)
        // may already be present and takes precedence.
        if let Some(think) = tier.think {
            obj.entry("think").or_insert(Value::Bool(think));
        }
    }
    let max_retries = config.gateway.max_retries.unwrap_or(0);
    let retry_delay_ms = config.gateway.retry_delay_ms.unwrap_or(200);

    debug!(
        tier = %tier.name,
        backend = %tier.backend,
        model = %tier.model,
        max_retries,
        "dispatching"
    );

    let client = BackendClient::new(backend_cfg)?;
    let mut last_err: anyhow::Error = anyhow::anyhow!("no attempts made");
    let mut delay_ms = retry_delay_ms;

    // Detect tool-call requests: non-empty `tools` array in the body.
    let has_tools = body
        .get("tools")
        .and_then(Value::as_array)
        .map(|a| !a.is_empty())
        .unwrap_or(false);

    for attempt in 0..=max_retries {
        if attempt > 0 {
            let sleep = std::cmp::min(delay_ms, 2_000);
            warn!(
                tier = %tier.name,
                attempt,
                delay_ms = sleep,
                "retrying after backend error"
            );
            tokio::time::sleep(tokio::time::Duration::from_millis(sleep)).await;
            delay_ms = delay_ms.saturating_mul(2);
        }

        let t0 = std::time::Instant::now();
        let result = if has_tools {
            client.tool_call(body.clone()).await
        } else {
            client.chat_completions(body.clone()).await
        };
        match result {
            Ok(response) => {
                let latency_ms = t0.elapsed().as_millis() as u64;
                let entry = TrafficEntry::new(
                    tier.name.clone(),
                    tier.backend.clone(),
                    latency_ms,
                    true,
                );
                return Ok((response, entry));
            }
            Err(e) => {
                last_err = e;
            }
        }
    }

    Err(last_err)
}

/// Mode B: try tiers cheapest-first and return the first sufficient response.
///
/// Iteration stops at `profile.max_auto_tier`. Backend failures and insufficient
/// responses both cause escalation to the next tier. If every tier is exhausted
/// without a sufficient response an error is returned.
pub(super) async fn escalate(
    state: &RouterState,
    body: &mut Value,
    profile: &ProfileConfig,
    stream: bool,
) -> anyhow::Result<(Value, TrafficEntry)> {
    let config = state.config();
    // Collect candidate tiers up to max_auto_tier
    let max_idx = config
        .tiers
        .iter()
        .position(|t| t.name == profile.max_auto_tier)
        .unwrap_or(config.tiers.len() - 1);

    let candidates: Vec<&TierConfig> = config.tiers[..=max_idx].iter().collect();

    // Pre-fetch backend health snapshot so degraded backends can be skipped.
    let health_window = config.gateway.health_window.unwrap_or(10);
    let health_threshold = config.gateway.health_error_threshold.unwrap_or(0.7);
    let backend_health = if health_window > 0 {
        state.traffic.backend_health(health_window, health_threshold).await
    } else {
        std::collections::HashMap::new()
    };

    for (tier_idx, tier) in candidates.iter().enumerate() {
        // Skip tiers whose backends are currently degraded (too many recent errors).
        if health_window > 0 {
            if let Some(health) = backend_health.get(&tier.backend) {
                if !health.healthy {
                    warn!(
                        tier = %tier.name,
                        backend = %tier.backend,
                        error_rate = health.error_rate,
                        window = health.total,
                        "skipping unhealthy backend — escalating"
                    );
                    continue;
                }
            }
        }

        let backend_cfg = match config.backends.get(&tier.backend) {
            Some(b) => b,
            None => continue,
        };

        if let Some(obj) = body.as_object_mut() {
            obj.insert("model".into(), Value::String(tier.model.clone()));
            obj.insert("stream".into(), Value::Bool(stream));
        }

        let client = match BackendClient::new(backend_cfg) {
            Ok(c) => c,
            Err(e) => {
                warn!(tier = %tier.name, error = %e, "skipping tier — client build failed");
                continue;
            }
        };

        let t0 = std::time::Instant::now();
        match client.chat_completions(body.clone()).await {
            Ok(response) => {
                let latency_ms = t0.elapsed().as_millis() as u64;
                if is_sufficient(&response) {
                    let mut entry =
                        TrafficEntry::new(tier.name.clone(), tier.backend.clone(), latency_ms, true);
                    if tier_idx > 0 {
                        entry = entry.mark_escalated();
                    }
                    return Ok((response, entry));
                }
                debug!(tier = %tier.name, "response insufficient — escalating");
            }
            Err(e) => {
                warn!(tier = %tier.name, error = %e, "tier request failed — escalating");
            }
        }
    }

    // Exhausted all tiers — last resort: use the final candidate anyway
    anyhow::bail!("all tiers exhausted without a sufficient response")
}

/// Mode C: pre-flight classification call, then dispatch to the resolved tier.
///
/// Extracts the last user message, makes a fast non-streaming inference call
/// to the `classifier` tier with the configured prompt, parses the one-word
/// label, and maps it to a tier from the profile's auto range:
///
/// - `simple`   → `tiers[0]`     (cheapest)
/// - `moderate` → `tiers[n / 2]` (middle)
/// - `complex`  → `tiers[n - 1]` (most capable, bounded by `max_auto_tier`)
///
/// Falls back to the classifier tier itself if the classification call fails or
/// there is no user message to classify.
pub(super) async fn classify_and_dispatch(
    state: &RouterState,
    body: &mut Value,
    profile: &ProfileConfig,
    stream: bool,
) -> anyhow::Result<(Value, TrafficEntry)> {
    let config = state.config();

    // Find the classifier tier (used for the classification call).
    let classifier_tier = config
        .tiers
        .iter()
        .find(|t| t.name == profile.classifier)
        .context("classifier tier not found in config")?;

    let backend_cfg = config
        .backends
        .get(&classifier_tier.backend)
        .with_context(|| format!("backend `{}` not in config", classifier_tier.backend))?
        .clone();

    // Extract the last user message to classify.
    let user_text = body
        .pointer("/messages")
        .and_then(Value::as_array)
        .and_then(|arr| {
            arr.iter()
                .rev()
                .find(|m| m.get("role").and_then(Value::as_str) == Some("user"))
        })
        .and_then(|m| m.get("content").and_then(Value::as_str))
        .map(|s| s.to_owned());

    let Some(user_text) = user_text else {
        debug!("no user message found — bypassing classification, using classifier tier");
        return dispatch(state, body, classifier_tier, stream).await;
    };

    let system_prompt = profile
        .classifier_prompt
        .as_deref()
        .unwrap_or(DEFAULT_CLASSIFIER_PROMPT);

    // Build candidate tier slice (first tier up to max_auto_tier inclusive).
    let max_idx = config
        .tiers
        .iter()
        .position(|t| t.name == profile.max_auto_tier)
        .unwrap_or(config.tiers.len().saturating_sub(1));
    let candidates: &[TierConfig] = &config.tiers[..=max_idx];

    // Make the classification call (non-streaming, max_tokens=10, temp=0).
    let classifier_body = serde_json::json!({
        "model": classifier_tier.model,
        "messages": [
            { "role": "system", "content": system_prompt },
            { "role": "user",   "content": &user_text   }
        ],
        "stream": false,
        "think": false,
        // num_predict and temperature go in options for Ollama native /api/chat;
        // max_tokens / temperature are OpenAI-compat fields that the native
        // endpoint silently ignores.
        "options": { "num_predict": 5, "temperature": 0 }
    });

    let client = BackendClient::new(&backend_cfg)?;
    let ParsedClassification { tier_label: label, think_override, tags } =
        match client.classify(classifier_body).await {
            Ok(response) => {
                let parsed = parse_classification(&response);
                debug!(
                    label = %parsed.tier_label,
                    think_override = ?parsed.think_override,
                    tags = ?parsed.tags,
                    "classified request"
                );
                parsed
            }
            Err(e) => {
                warn!(err = %e, "classification call failed — defaulting to first tier");
                ParsedClassification { tier_label: "instant".into(), ..Default::default() }
            }
        };

    // Rule evaluation: sort by priority DESC, first match wins.
    // A rule matches when every `when` key=value pair is present in `tags`
    // (case-insensitive).  Matched rules bypass the normal tier ladder and
    // route directly to `rule.route_to`.
    let mut sorted_rules = profile.rules.clone();
    sorted_rules.sort_by(|a, b| b.priority.cmp(&a.priority));
    if let Some(rule) = sorted_rules.iter().find(|r| {
        r.when.iter().all(|(k, v)| {
            tags.get(k.as_str())
                .map(|tv| tv.eq_ignore_ascii_case(v))
                .unwrap_or(false)
        })
    }) {
        debug!(route_to = %rule.route_to, "routing rule matched");
        let rule_tier = config
            .resolve_tier(&rule.route_to)
            .with_context(|| format!("rule route_to `{}` not found in config", rule.route_to))?;
        if let Some(t) = think_override {
            if let Some(obj) = body.as_object_mut() {
                obj.insert("think".into(), Value::Bool(t));
            }
        }
        return dispatch(state, body, rule_tier, stream).await;
    }

    // If the conversation contains a tool-result the model must synthesise
    // external data — don't send it to the cheapest tier.
    let has_tool_result = body
        .pointer("/messages")
        .and_then(Value::as_array)
        .map(|arr| arr.iter().any(|m| {
            m.get("role").and_then(Value::as_str) == Some("tool")
        }))
        .unwrap_or(false);

    let mut target_tier = resolve_tier_by_label(&label, candidates);
    if has_tool_result && std::ptr::eq(target_tier, &candidates[0]) && candidates.len() > 1 {
        debug!("tool-result — bumping from cheapest tier to next");
        target_tier = &candidates[1];
    }

    debug!(
        label = %label,
        tier = %target_tier.name,
        "classify routing resolved"
    );

    // Inject think override before dispatching.
    if let Some(t) = think_override {
        if let Some(obj) = body.as_object_mut() {
            obj.insert("think".into(), Value::Bool(t));
        }
    }

    dispatch(state, body, target_tier, stream).await
}

/// Decide whether a backend response is good enough to return or should be escalated.
///
/// # ⚠️ Heuristic stopgap
///
/// This is a best-effort heuristic, not a reliable quality gate. It will produce
/// false positives (escalating a valid response) and false negatives (accepting a
/// low-quality one). Use escalation mode only where the occasional wrong call is
/// acceptable. Do not extend this without measuring against real data.
///
/// Current checks:
/// - Responses shorter than 20 characters are almost certainly non-answers.
/// - A small set of refusal phrases indicate the model couldn't or wouldn't help.
pub(crate) fn is_sufficient(response: &Value) -> bool {
    // Extract the content from the first choice
    let content = response
        .pointer("/choices/0/message/content")
        .and_then(Value::as_str)
        .unwrap_or("");

    // Escalate if the response is very short (likely a non-answer)
    if content.len() < 20 {
        return false;
    }

    // Escalate if the model explicitly refuses
    let lower = content.to_lowercase();
    let refusal_phrases = [
        "i don't know",
        "i cannot help",
        "i'm not able to",
        "as an ai",
        "i don't have enough information",
    ];
    if refusal_phrases.iter().any(|p| lower.contains(p)) {
        return false;
    }

    true
}
