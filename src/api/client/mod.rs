//! Client-facing API (port 8080) — the endpoint clients and agents talk to.
//!
//! This is intentionally a thin layer: all routing logic lives in [`crate::router`].
//! Handlers translate HTTP concerns (status codes, JSON bodies) into calls
//! to the router and back.

use std::sync::Arc;

use axum::{
    routing::{get, post},
    Router,
};
use serde_json::{json, Value};

use crate::router::RouterState;

mod ollama;
mod openai;

/// Build the client-facing axum router (port 8080).
pub fn router(state: Arc<RouterState>) -> Router {
    Router::new()
        .route("/healthz", get(crate::api::health::healthz))
        .route("/status", get(crate::api::status::status))
        .route("/v1/chat/completions", post(openai::chat_completions))
        .route("/v1/models", get(openai::list_models))
        // Ollama-compatible discovery — used by Home Assistant's Ollama integration
        // and any client that enumerates models via the native Ollama API.
        .route("/api/tags", get(ollama::list_models_ollama))
        .route("/api/chat", post(ollama::chat_completions_ollama))
        .with_state(state)
}

/// Classify a backend error into a short, user-readable message.
///
/// Inspects the error chain for known patterns (timeouts, HTTP status codes,
/// missing models) and returns an appropriate explanation. The message is
/// intentionally terse — it will appear directly in the chat UI.
fn classify_backend_error(err: &anyhow::Error) -> &'static str {
    let msg = err.to_string();
    let lower = msg.to_ascii_lowercase();
    if lower.contains("timed out") || lower.contains("timeout") || lower.contains("elapsed") {
        "The language model took too long to respond. Please try again."
    } else if lower.contains("http 404") || lower.contains("not found") {
        "The requested model isn't available right now. Please check the gateway configuration."
    } else if lower.contains("http 5") || lower.contains("502") || lower.contains("503") || lower.contains("504") {
        "The language model backend returned an error. Please try again in a moment."
    } else if lower.contains("connection refused") || lower.contains("connect error") {
        "Cannot reach the language model backend. Please try again later."
    } else if lower.contains("no profile") || lower.contains("unknown profile") || lower.contains("not configured") {
        "No routing profile is configured for this request."
    } else {
        "Something went wrong while processing your request. Please try again."
    }
}

/// Build an OpenAI-compatible chat completion response carrying an error message.
///
/// Returns HTTP 200 with a valid `chat.completion` object so that clients
/// such as Home Assistant Assist render the message in the chat UI instead
/// of showing a generic "Oops" error dialog.
fn error_openai_response(err: &anyhow::Error, model: &str) -> Value {
    let text = classify_backend_error(err);
    tracing::warn!(error = %err, user_message = text, "returning error as chat response");
    json!({
        "id":      "chatcmpl-error",
        "object":  "chat.completion",
        "created": chrono::Utc::now().timestamp(),
        "model":   model,
        "choices": [{
            "index":   0,
            "message": { "role": "assistant", "content": text },
            "finish_reason": "stop"
        }],
        "usage": { "prompt_tokens": 0, "completion_tokens": 0, "total_tokens": 0 }
    })
}

/// Build an Ollama-compatible chat response carrying an error message.
///
/// Same intent as [`error_openai_response`] but in the Ollama wire format
/// used by `POST /api/chat`.
fn error_ollama_response(err: &anyhow::Error, model: &str) -> Value {
    let text = classify_backend_error(err);
    tracing::warn!(error = %err, user_message = text, "returning error as ollama chat response");
    json!({
        "model":      model,
        "created_at": chrono::Utc::now().to_rfc3339(),
        "message":    { "role": "assistant", "content": text },
        "done":       true,
        "done_reason": "stop",
        "total_duration":    0,
        "load_duration":     0,
        "prompt_eval_count": 0,
        "eval_count":        0
    })
}

#[cfg(test)]
mod tests {
    use std::sync::Arc;

    use axum::{
        body::{to_bytes, Body},
        http::{Request, StatusCode},
    };
    use serde_json::json;
    use tower::ServiceExt; // oneshot
    use wiremock::matchers::{method, path};
    use wiremock::{Mock, MockServer, ResponseTemplate};

    use crate::{
        config::{BackendConfig, Config, GatewayConfig, ProfileConfig, RoutingMode, TierConfig},
        router::RouterState,
        traffic::TrafficLog,
    };

    // -----------------------------------------------------------------------
    // Test helpers
    // -----------------------------------------------------------------------

    fn minimal_state() -> Arc<RouterState> {
        state_with_backend("http://127.0.0.1:0") // unreachable — only for non-routing tests
    }

    fn state_with_backend(base_url: &str) -> Arc<RouterState> {
        let config = Config {
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
            },
            backends: {
                let mut m = std::collections::HashMap::new();
                m.insert(
                    "mock".into(),
                    BackendConfig {
                        base_url: base_url.into(),
                        api_key_env: None,
                        api_key_secret: None,
                        timeout_ms: 5_000,
                        provider: crate::config::Provider::default(),
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
                },
                TierConfig {
                    name: "cloud:economy".into(),
                    backend: "mock".into(),
                    model: "economy-model".into(),
                    think: None,
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
                        mode: RoutingMode::Dispatch,
                        classifier: "local:fast".into(),
                        max_auto_tier: "cloud:economy".into(),
                        expert_requires_flag: false,
                        rate_limit_rpm: None,
                        classifier_prompt: None,
                        system_prompt: None,
                    },
                );
                m
            },
            clients: vec![],
        };
        Arc::new(RouterState::new(
            Arc::new(config),
            std::path::PathBuf::default(),
            Arc::new(TrafficLog::new(100)),
        ))
    }

    async fn body_json(body: Body) -> serde_json::Value {
        let bytes = to_bytes(body, usize::MAX).await.unwrap();
        serde_json::from_slice(&bytes).unwrap()
    }

    // -----------------------------------------------------------------------
    // GET /healthz
    // -----------------------------------------------------------------------

    #[tokio::test]
    async fn healthz_returns_200_ok() {
        let app = super::router(minimal_state());
        let req = Request::builder()
            .method("GET")
            .uri("/healthz")
            .body(Body::empty())
            .unwrap();

        let resp = app.oneshot(req).await.unwrap();
        assert_eq!(resp.status(), StatusCode::OK);
        let json = body_json(resp.into_body()).await;
        assert_eq!(json["status"], "ok");
    }

    // -----------------------------------------------------------------------
    // GET /v1/models
    // -----------------------------------------------------------------------

    #[tokio::test]
    async fn list_models_returns_all_tiers() {
        let app = super::router(minimal_state());
        let req = Request::builder()
            .method("GET")
            .uri("/v1/models")
            .body(Body::empty())
            .unwrap();

        let resp = app.oneshot(req).await.unwrap();
        assert_eq!(resp.status(), StatusCode::OK);

        let json = body_json(resp.into_body()).await;
        assert_eq!(json["object"], "list");
        let data = json["data"].as_array().unwrap();
        let ids: Vec<&str> = data
            .iter()
            .filter_map(|v| v["id"].as_str())
            .collect();
        assert!(ids.contains(&"local:fast"), "missing local:fast: {ids:?}");
        assert!(ids.contains(&"cloud:economy"), "missing cloud:economy: {ids:?}");
    }

    #[tokio::test]
    async fn list_models_includes_aliases() {
        let app = super::router(minimal_state());
        let req = Request::builder()
            .method("GET")
            .uri("/v1/models")
            .body(Body::empty())
            .unwrap();

        let resp = app.oneshot(req).await.unwrap();
        let json = body_json(resp.into_body()).await;
        let data = json["data"].as_array().unwrap();
        let alias_entry = data.iter().find(|v| v["id"] == "hint:fast");
        assert!(alias_entry.is_some(), "alias hint:fast not in model list");
        assert_eq!(alias_entry.unwrap()["owned_by"], "alias");
        assert!(alias_entry.unwrap()["lm_gateway"]["resolves_to"].is_string());
    }

    // -----------------------------------------------------------------------
    // POST /v1/chat/completions
    // -----------------------------------------------------------------------

    #[tokio::test]
    async fn chat_completions_proxies_to_backend_and_returns_response() {
        let server = MockServer::start().await;
        Mock::given(method("POST"))
            .and(path("/v1/chat/completions"))
            .respond_with(ResponseTemplate::new(200).set_body_json(json!({
                "choices": [{ "message": { "content": "This is a long enough answer from the mock backend to satisfy the sufficiency check." } }]
            })))
            .mount(&server)
            .await;

        let app = super::router(state_with_backend(&server.uri()));
        let req = Request::builder()
            .method("POST")
            .uri("/v1/chat/completions")
            .header("Content-Type", "application/json")
            .body(Body::from(
                serde_json::to_vec(
                    &json!({ "model": "local:fast", "messages": [{"role": "user", "content": "hello"}] }),
                )
                .unwrap(),
            ))
            .unwrap();

        let resp = app.oneshot(req).await.unwrap();
        assert_eq!(resp.status(), StatusCode::OK);
        let json = body_json(resp.into_body()).await;
        assert!(json.pointer("/choices/0/message/content").is_some());
    }

    #[tokio::test]
    async fn chat_completions_returns_user_friendly_message_when_backend_is_unreachable() {
        // Port 1 is reserved and never responds — guaranteed connection refusal.
        // The gateway wraps backend errors as HTTP 200 chat responses so that
        // clients like Home Assistant Assist show the message in the chat UI
        // instead of a generic error dialog.
        let app = super::router(state_with_backend("http://127.0.0.1:1"));
        let req = Request::builder()
            .method("POST")
            .uri("/v1/chat/completions")
            .header("Content-Type", "application/json")
            .body(Body::from(
                serde_json::to_vec(
                    &json!({ "model": "local:fast", "messages": [] }),
                )
                .unwrap(),
            ))
            .unwrap();

        let resp = app.oneshot(req).await.unwrap();
        // HTTP 200 — error is surfaced as a chat message, not a raw HTTP error.
        assert_eq!(resp.status(), StatusCode::OK);
        let json = body_json(resp.into_body()).await;
        // Should be a valid OpenAI chat.completion with an assistant error message.
        let content = json
            .pointer("/choices/0/message/content")
            .and_then(|v| v.as_str())
            .expect("error response should carry a content message");
        assert!(!content.is_empty(), "error message should not be empty");
    }
}
