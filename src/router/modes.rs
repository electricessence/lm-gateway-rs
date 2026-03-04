//! Routing execution modes: dispatch, escalate, and classify-then-dispatch.
//!
//! Each mode is a self-contained async function called by [`super::route`] or
//! [`super::route_stream`] after the active profile and target tier have been
//! resolved.  [`is_sufficient`] is the heuristic that drives escalation.

use anyhow::Context;
use futures_util::future::BoxFuture;
use serde_json::Value;
use tracing::{debug, warn};

use crate::{
    backends::BackendClient,
    config::{Config, ProfileConfig, Provider, TierConfig, DEFAULT_CLASSIFIER_PROMPT},
    traffic::TrafficEntry,
};

use super::{
    RouterState,
    classify::{parse_classification, ParsedClassification, resolve_tier_by_label},
};

/// Outcome of a classify-mode routing pass.
///
/// Carries the resolved tier name, optional `think` override to inject before
/// dispatch, the top-level classification label (for logging/trace headers), and
/// the chain of profile names traversed — e.g. `["auto", "ha-auto"]` for a
/// two-hop cascade.
pub(super) struct RoutingResolution {
    pub tier_name: String,
    pub think_override: Option<bool>,
    pub class_label: String,
    pub profile_chain: Vec<String>,
}

/// Classify a request against the named profile and resolve it to a concrete tier.
///
/// Supports **profile cascade routing**: when a matched rule's `route_to` names
/// another profile (rather than a tier name or alias), this function recurses
/// into that profile's classifier and rule set.  The `visited` list prevents
/// infinite loops at runtime — a static DFS at config-load catches any declared
/// cycles before the server starts.
///
/// Called by [`classify_and_dispatch`] for the non-streaming path and directly
/// from `route_stream` for the streaming path so both share identical routing
/// logic.
pub(super) fn classify_and_resolve<'a>(
    state: &'a RouterState,
    body: &'a Value,
    profile_name: &'a str,
    visited: Vec<String>,
) -> BoxFuture<'a, anyhow::Result<RoutingResolution>> {
    Box::pin(async move {
        let config = state.config();
        let profile = config
            .profiles
            .get(profile_name)
            .with_context(|| format!("profile `{profile_name}` not found in config"))?;

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
            debug!(profile = %profile_name, "no user message — bypassing classification");
            return Ok(RoutingResolution {
                tier_name: classifier_tier.name.clone(),
                think_override: None,
                class_label: String::new(), // no classification performed — skip class_prompts
                profile_chain: visited,
            });
        };

        let system_prompt = profile
            .classifier_prompt
            .as_deref()
            .unwrap_or(DEFAULT_CLASSIFIER_PROMPT);

        let max_idx = config
            .tiers
            .iter()
            .position(|t| t.name == profile.max_auto_tier)
            .unwrap_or(config.tiers.len().saturating_sub(1));
        let candidates: &[TierConfig] = &config.tiers[..=max_idx];

        let classifier_think = profile.classifier_think.unwrap_or(false);
        let classifier_body = serde_json::json!({
            "model": classifier_tier.model,
            "messages": [
                { "role": "system", "content": system_prompt },
                { "role": "user",   "content": &user_text   }
            ],
            "stream": false,
            "think": classifier_think,
            "options": { "num_predict": 10, "temperature": 0 }
        });

        let client = BackendClient::new(&backend_cfg)?;
        let ParsedClassification { tier_label: label, think_override, tags } =
            match client.classify(classifier_body).await {
                Ok(response) => {
                    let parsed = parse_classification(&response);
                    debug!(
                        profile = %profile_name,
                        label = %parsed.tier_label,
                        think_override = ?parsed.think_override,
                        tags = ?parsed.tags,
                        "classified request"
                    );
                    parsed
                }
                Err(e) => {
                    warn!(err = %e, profile = %profile_name, "classification call failed — defaulting to first tier");
                    ParsedClassification { tier_label: "instant".into(), ..Default::default() }
                }
            };

        // Rule evaluation: rules are pre-sorted by priority DESC at config load time
        // (Config::normalize), so we iterate directly without cloning or re-sorting.
        // A rule matches when every `when` key=value pair is present in `tags`.
        // Cyclic cascade rules are skipped and lower-priority rules are tried.
        //
        // class_label: prefer the explicit `class=` tag. If absent, check whether any
        // other tag value matches a class_prompts key (defensive: handles prompts that
        // emit `intent=greeting` or similar). Fall back to the tier label as last resort.
        let class_label: String = tags
            .get("class")
            .cloned()
            .or_else(|| {
                tags.values()
                    .find(|v| profile.class_prompts.contains_key(v.as_str()))
                    .cloned()
            })
            .unwrap_or_else(|| label.clone());

        for rule in &profile.rules {
            if !rule.when.iter().all(|(k, v)| {
                tags.get(k.as_str())
                    .map(|tv| tv.eq_ignore_ascii_case(v))
                    .unwrap_or(false)
            }) {
                continue; // rule doesn't match tags
            }
            debug!(profile = %profile_name, route_to = %rule.route_to, "routing rule matched");

            if config.profiles.contains_key(&rule.route_to) {
                // Cascade: route_to names a profile — recurse unless already visited.
                if visited.contains(&rule.route_to) {
                    warn!(
                        profile = %profile_name,
                        route_to = %rule.route_to,
                        "cascade cycle at runtime — skipping cyclic rule, trying lower-priority rules"
                    );
                    continue; // skip this rule; lower-priority rules may still match safely
                }
                let mut next_visited = visited.clone();
                next_visited.push(rule.route_to.clone());
                let target_name = rule.route_to.clone();
                debug!(cascade_to = %target_name, chain = ?next_visited, "cascading to profile");
                let inner =
                    classify_and_resolve(state, body, &target_name, next_visited).await?;
                return Ok(inner);
            } else {
                // route_to is a tier name or alias — dispatch directly.
                let rule_tier = config
                    .resolve_tier(&rule.route_to)
                    .with_context(|| format!("rule route_to `{}` not found in config", rule.route_to))?;
                return Ok(RoutingResolution {
                    tier_name: rule_tier.name.clone(),
                    think_override,
                    class_label: class_label.clone(),
                    profile_chain: visited,
                });
            }
        }

        // No rule matched (or all matching rules were cyclic) — resolve tier from the label.
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
            profile = %profile_name,
            label = %label,
            tier = %target_tier.name,
            "classify routing resolved"
        );

        Ok(RoutingResolution {
            tier_name: target_tier.name.clone(),
            think_override,
            class_label,
            profile_chain: visited,
        })
    })
}

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
///
/// For local providers (Ollama, OpenAI-compat), the request waits for a
/// priority permit before calling the backend, serialising lower-priority work
/// behind higher-priority in-flight requests. Cloud providers bypass the gate.
pub(super) async fn dispatch(
    state: &RouterState,
    body: &mut Value,
    tier: &TierConfig,
    priority: i32,
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

    // Acquire the priority gate for local providers.
    // Cloud-managed providers (Anthropic, OpenRouter) bypass the gate — the
    // cloud handles its own scheduling and adding a gateway queue would only
    // increase tail latency without benefit.
    let is_cloud = matches!(backend_cfg.provider, Provider::Anthropic | Provider::OpenRouter);
    let _gate_permit = if !is_cloud {
        if let Some(gate) = state.gates.get(&tier.name) {
            Some(gate.acquire(priority).await)
        } else {
            None // Tier was added after startup (hot-reload) — fire immediately.
        }
    } else {
        None
    };

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
    priority: i32,
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

        // Acquire gate for local providers (same policy as dispatch).
        let is_cloud = matches!(backend_cfg.provider, Provider::Anthropic | Provider::OpenRouter);
        let _gate_permit = if !is_cloud {
            if let Some(gate) = state.gates.get(&tier.name) {
                Some(gate.acquire(priority).await)
            } else {
                None
            }
        } else {
            None
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

/// Mode C: pre-flight classification + cascade-aware routing, then dispatch.
///
/// Delegates the classification and rule-evaluation to [`classify_and_resolve`],
/// which handles profile cascade routing, then dispatches to the resolved tier.
///
/// The `profile_name` parameter seeds the visited-set used for cascade cycle
/// detection.  Pass the name of the profile being actively processed.
pub(super) async fn classify_and_dispatch(
    state: &RouterState,
    body: &mut Value,
    profile_name: &str,
    priority: i32,
    stream: bool,
) -> anyhow::Result<(Value, TrafficEntry)> {
    let config = state.config();
    let visited = vec![profile_name.to_owned()];
    let resolution = classify_and_resolve(state, body, profile_name, visited).await?;

    // Apply per-class system prompt from the final profile in the cascade chain.
    let final_profile_name = resolution.profile_chain.last().map(String::as_str).unwrap_or(profile_name);
    if let Some(final_profile) = config.profiles.get(final_profile_name) {
        if let Some(class_prompt) = final_profile.class_prompts.get(resolution.class_label.as_str()) {
            super::inject_system_prompt(body, class_prompt);
        }
    }

    // Inject think override before dispatching.
    if let Some(t) = resolution.think_override {
        if let Some(obj) = body.as_object_mut() {
            obj.insert("think".into(), Value::Bool(t));
        }
    }

    let tier = config
        .tiers
        .iter()
        .find(|t| t.name == resolution.tier_name)
        .with_context(|| format!("resolved tier `{}` not found in config", resolution.tier_name))?;

    let (response, entry) = dispatch(state, body, tier, priority, stream).await?;
    let entry = entry.with_routing_trace(resolution.class_label, resolution.profile_chain);
    Ok((response, entry))
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
