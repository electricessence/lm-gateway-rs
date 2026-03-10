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
            traffic_log_debug: false,
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
            traffic_log_debug: false,
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
                traffic_log_debug: false,
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

// -----------------------------------------------------------------------
// debug-traffic capture — only compiled with the feature flag
// -----------------------------------------------------------------------

#[cfg(feature = "debug-traffic")]
#[tokio::test]
async fn debug_traffic_captures_request_body_in_entry() {
    let server = MockServer::start().await;
    Mock::given(method("POST"))
        .and(path("/v1/chat/completions"))
        .respond_with(ResponseTemplate::new(200).set_body_json(long_response(
            "Debug body capture response.",
        )))
        .mount(&server)
        .await;

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
            traffic_log_debug: true,
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
        tiers: vec![TierConfig {
            name: "local:fast".into(),
            backend: "mock".into(),
            model: "fast-model".into(),
            think: None,
            max_context_tokens: None,
        }],
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
                    mode: RoutingMode::Dispatch,
                    classifier: "local:fast".into(),
                    max_auto_tier: "local:fast".into(),
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

    let body = json!({
        "model": "hint:fast",
        "messages": [{"role": "user", "content": "capture this body"}]
    });
    let (_, entry) = route(&state, body.clone(), None, None, 0, false, false)
        .await
        .unwrap();

    assert!(
        entry.debug_request_body.is_some(),
        "debug_request_body must be populated when debug_traffic = true"
    );
    // The original hint is preserved in requested_model; the captured body
    // reflects what was actually sent to the backend (model rewritten by dispatch).
    assert_eq!(entry.requested_model.as_deref(), Some("hint:fast"));
    assert_eq!(
        entry.debug_request_body.as_ref().and_then(|b| b["model"].as_str()),
        Some("fast-model")
    );
}

#[cfg(feature = "debug-traffic")]
#[tokio::test]
async fn debug_traffic_omits_body_when_disabled() {
    let server = MockServer::start().await;
    Mock::given(method("POST"))
        .and(path("/v1/chat/completions"))
        .respond_with(ResponseTemplate::new(200).set_body_json(long_response(
            "No debug body expected here.",
        )))
        .mount(&server)
        .await;

    // mock_state uses traffic_log_debug: false
    let state = mock_state(&server, RoutingMode::Dispatch).await;
    let body = json!({
        "model": "hint:fast",
        "messages": [{"role": "user", "content": "do not capture"}]
    });
    let (_, entry) = route(&state, body, None, None, 0, false, false).await.unwrap();
    assert!(
        entry.debug_request_body.is_none(),
        "debug_request_body must be None when debug_traffic = false"
    );
}
