//! Client-facing API (port 8080) — the endpoint clients and agents talk to.
//!
//! This is intentionally a thin layer: all routing logic lives in [`crate::router`].
//! Handlers translate HTTP concerns (status codes, JSON bodies) into calls
//! to the router and back.

use std::sync::Arc;

use axum::{
    body::Body,
    extract::{Extension, State},
    response::{IntoResponse, Response},
    routing::{get, post},
    Json, Router,
};
use serde_json::{json, Value};

use crate::{
    api::{client_auth::ClientProfile, request_id::RequestId},
    backends::SseStream,
    error::AppError,
    router::RouterState,
};

/// Build the client-facing axum router (port 8080).
pub fn router(state: Arc<RouterState>) -> Router {
    Router::new()
        .route("/healthz", get(crate::api::health::healthz))
        .route("/status", get(crate::api::status::status))
        .route("/v1/chat/completions", post(chat_completions))
        .route("/v1/models", get(list_models))
        // Ollama-compatible discovery — used by Home Assistant's Ollama integration
        // and any client that enumerates models via the native Ollama API.
        .route("/api/tags", get(list_models_ollama))
        .route("/api/chat", post(chat_completions_ollama))
        .with_state(state)
}

/// `POST /v1/chat/completions` — route a chat request through the tier ladder.
///
/// When `stream: true` is set in the request body, the response is proxied as
/// a raw SSE stream from the backend (no buffering). Escalation is skipped for
/// streaming requests — the first matching tier is used directly. All backends
/// produce OpenAI-compatible SSE (Anthropic is translated on-the-fly).
pub async fn chat_completions(
    State(state): State<Arc<RouterState>>,
    request_id_ext: Option<Extension<RequestId>>,
    client_profile: Option<Extension<ClientProfile>>,
    headers: axum::http::HeaderMap,
    Json(body): Json<Value>,
) -> Result<Response, AppError> {
    let expert_gate = headers
        .get("x-claw-expert")
        .and_then(|v| v.to_str().ok())
        .map(|v| v.eq_ignore_ascii_case("true"))
        .unwrap_or(false);
    let req_id = request_id_ext.map(|Extension(id)| id.0);
    let profile = client_profile.map(|Extension(p)| p.0);
    let streaming = body.get("stream").and_then(Value::as_bool).unwrap_or(false);

    // Per-profile rate limit: shared quota across all clients on the same profile.
    let profile_name = profile.as_deref().unwrap_or("default");
    if let Some(limiter) = state.profile_limiters.get(profile_name) {
        if let Err(retry_after) = limiter.check_global() {
            use axum::http::StatusCode;
            return Ok((
                StatusCode::TOO_MANY_REQUESTS,
                [
                    ("retry-after", retry_after.to_string()),
                    ("x-ratelimit-limit", limiter.rpm.to_string()),
                    ("x-ratelimit-policy", format!("{};w=60", limiter.rpm)),
                    ("x-ratelimit-scope", "profile".to_string()),
                    ("content-type", "text/plain".to_string()),
                ],
                "Profile rate limit exceeded. Please retry after the indicated delay.",
            )
                .into_response());
        }
    }

    if streaming {
        let (stream, _entry) =
            crate::router::route_stream(&state, body, profile.as_deref(), req_id.as_deref(), expert_gate).await?;
        return Ok(proxy_sse(stream));
    }

    let (resp, _entry) =
        crate::router::route(&state, body, profile.as_deref(), req_id.as_deref(), false, expert_gate).await?;
    Ok(Json(resp).into_response())
}

/// Proxy an [`SseStream`] to the client as a streaming HTTP response.
///
/// Sets `content-type: text/event-stream`, `cache-control: no-cache`, and
/// `x-accel-buffering: no` (disables nginx buffering when lm-gateway sits
/// behind a reverse proxy such as Caddy).
fn proxy_sse(stream: SseStream) -> Response {
    Response::builder()
        .status(200)
        .header("content-type", "text/event-stream")
        .header("cache-control", "no-cache")
        .header("x-accel-buffering", "no")
        .body(Body::from_stream(stream))
        .expect("proxy_sse: failed to build streaming response")
}

/// `GET /v1/models` — list available tiers and aliases as model objects.
///
/// Returns an OpenAI-compatible model list so clients can enumerate what
/// routing targets are available without any out-of-band config.
pub async fn list_models(State(state): State<Arc<RouterState>>) -> impl IntoResponse {
    let config = state.config();
    let tiers = config.tiers.iter().map(|t| {
        json!({
            "id": t.name,
            "object": "model",
            "owned_by": t.backend,
        })
    });

    let aliases = config.aliases.iter().map(|(alias, target)| {
        json!({
            "id": alias,
            "object": "model",
            "owned_by": "alias",
            "lm_gateway": { "resolves_to": target },
        })
    });

    let data: Vec<Value> = tiers.chain(aliases).collect();
    Json(json!({ "object": "list", "data": data }))
}

/// `GET /api/tags` — Ollama-compatible model discovery.
///
/// Exposes configured *profiles* as the visible "models", not the underlying
/// tiers or aliases. This preserves the abstraction: clients (Home Assistant,
/// Open WebUI, etc.) see logical routing profiles — `auto`, `local`, etc. —
/// and remain unaware of the tier ladder beneath.
///
/// Selecting a profile name as the model in `POST /api/chat` causes the
/// gateway to apply that profile's routing mode (classify, dispatch, escalate)
/// transparently.
pub async fn list_models_ollama(State(state): State<Arc<RouterState>>) -> impl IntoResponse {
    let config = state.config();
    let now = chrono::Utc::now().to_rfc3339();

    let mut profile_names: Vec<&String> = config.profiles.keys().collect();
    // Stable order: default first, then alphabetical.
    profile_names.sort_by_key(|n| (n.as_str() != "default", n.as_str()));

    let models: Vec<Value> = profile_names
        .into_iter()
        .map(|name| {
            let mode = config
                .profiles
                .get(name)
                .map(|p| p.mode.to_string())
                .unwrap_or_default();
            json!({
                "name":        format!("{name}:latest"),
                "model":       format!("{name}:latest"),
                "modified_at": now,
                "size":        0,
                "digest":      "sha256:0000000000000000000000000000000000000000000000000000000000000000",
                "details": {
                    "parent_model":       "",
                    "format":             "gguf",
                    "family":             "lm-gateway",
                    "families":           ["lm-gateway"],
                    "parameter_size":     mode,
                    "quantization_level": "auto"
                }
            })
        })
        .collect();

    Json(json!({ "models": models }))
}

/// `POST /api/chat` — Ollama-compatible chat inference.
///
/// Accepts requests in Ollama's native format (`model`, `messages`, `stream`)
/// and routes them through lm-gateway's tier/classify pipeline. The response is
/// returned in Ollama format.
///
/// Non-streaming (`"stream": false` or absent): returns a single JSON object
/// matching Ollama's response schema.
///
/// Streaming (`"stream": true`): returns newline-delimited JSON (NDJSON) in
/// Ollama's streaming format, translated from the OpenAI SSE stream produced
/// by the backend.
///
/// The `model` field may be a profile name (e.g. `auto`, `default`) or any
/// configured tier name or alias. When a profile name is given, the gateway
/// applies that profile's routing mode (classify/dispatch/escalate) and the
/// actual tier selection is handled internally — the caller never needs to
/// know which underlying model answered.
pub async fn chat_completions_ollama(
    State(state): State<Arc<RouterState>>,
    request_id_ext: Option<Extension<RequestId>>,
    client_profile: Option<Extension<ClientProfile>>,
    headers: axum::http::HeaderMap,
    Json(body): Json<Value>,
) -> Result<Response, AppError> {
    let expert_gate = headers
        .get("x-claw-expert")
        .and_then(|v| v.to_str().ok())
        .map(|v| v.eq_ignore_ascii_case("true"))
        .unwrap_or(false);
    let req_id = request_id_ext.map(|Extension(id)| id.0);
    let profile = client_profile.map(|Extension(p)| p.0);
    let streaming = body.get("stream").and_then(Value::as_bool).unwrap_or(false);

    // Strip trailing ":latest" from the model name (added by Ollama clients like HA).
    let mut openai_body = body.clone();
    if let Some(model_str) = openai_body.get("model").and_then(Value::as_str) {
        let normalised = model_str.strip_suffix(":latest").unwrap_or(model_str).to_owned();
        openai_body["model"] = json!(normalised);
    }

    // Profile-as-model: if the (normalised) model name matches a configured profile,
    // route via that profile and point the model at the profile's classifier tier.
    // This is the primary path for Ollama clients — they pick a profile name from
    // /api/tags and the gateway handles all tier selection transparently.
    let mut profile_override: Option<String> = None;
    {
        let config = state.config();
        if let Some(model_str) = openai_body.get("model").and_then(Value::as_str) {
            if let Some(prof) = config.profiles.get(model_str) {
                profile_override = Some(model_str.to_owned());
                // Route the underlying call via the profile's base (classifier) tier.
                openai_body["model"] = json!(&prof.classifier);
            }
        }
    }

    let effective_profile = profile_override.as_deref().or(profile.as_deref());

    if streaming {
        let (stream, _entry) = crate::router::route_stream(
            &state,
            openai_body,
            effective_profile,
            req_id.as_deref(),
            expert_gate,
        )
        .await?;
        // Translate OpenAI SSE stream → Ollama NDJSON stream.
        let model_name = body
            .get("model")
            .and_then(Value::as_str)
            .unwrap_or("lm-gateway")
            .to_owned();
        let ndjson = sse_to_ollama_ndjson(model_name, stream);
        return Ok(axum::response::Response::builder()
            .header("content-type", "application/x-ndjson")
            .header("cache-control", "no-cache")
            .header("x-accel-buffering", "no")
            .body(Body::from_stream(ndjson))
            .expect("ollama_chat: failed to build streaming response"));
    }

    // Non-streaming path: route and convert response.
    openai_body["stream"] = json!(false);
    let (response, _entry) = crate::router::route(
        &state,
        openai_body,
        effective_profile,
        req_id.as_deref(),
        false,
        expert_gate,
    )
    .await?;

    let model_name = body
        .get("model")
        .and_then(Value::as_str)
        .unwrap_or("lm-gateway");
    let content = response
        .pointer("/choices/0/message/content")
        .and_then(Value::as_str)
        .unwrap_or("");
    let eval_count = response
        .pointer("/usage/completion_tokens")
        .and_then(Value::as_u64)
        .unwrap_or(0);
    let prompt_count = response
        .pointer("/usage/prompt_tokens")
        .and_then(Value::as_u64)
        .unwrap_or(0);

    let ollama_response = json!({
        "model":       model_name,
        "created_at":  chrono::Utc::now().to_rfc3339(),
        "message":     { "role": "assistant", "content": content },
        "done":        true,
        "done_reason": "stop",
        "total_duration":     0,
        "load_duration":      0,
        "prompt_eval_count":  prompt_count,
        "eval_count":         eval_count
    });

    Ok(Json(ollama_response).into_response())
}

/// Translate an OpenAI SSE stream into an Ollama-compatible NDJSON stream.
///
/// Each SSE `data: {...}` event is parsed; the delta content is extracted and
/// emitted as an Ollama NDJSON chunk. The final `data: [DONE]` event produces
/// the closing `"done": true` line.
fn sse_to_ollama_ndjson(
    model: String,
    stream: crate::backends::SseStream,
) -> impl futures_util::Stream<Item = anyhow::Result<bytes::Bytes>> {
    use futures_util::StreamExt;

    let model = std::sync::Arc::new(model);
    let mut buf = String::new();

    stream.flat_map(move |chunk_res| {
        let model = model.clone();
        let output: Vec<anyhow::Result<bytes::Bytes>> = match chunk_res {
            Err(e) => vec![Err(e)],
            Ok(bytes) => {
                buf.push_str(&String::from_utf8_lossy(&bytes));
                let mut out = Vec::new();

                while let Some(pos) = buf.find("\n\n") {
                    let event = buf[..pos].to_owned();
                    buf = buf[pos + 2..].to_owned();

                    for line in event.lines() {
                        let data = line.strip_prefix("data: ").unwrap_or(line);
                        if data == "[DONE]" || data.is_empty() {
                            let done_line = serde_json::json!({
                                "model":      model.as_str(),
                                "created_at": chrono::Utc::now().to_rfc3339(),
                                "message":    { "role": "assistant", "content": "" },
                                "done":       true,
                                "done_reason": "stop"
                            });
                            let mut s = done_line.to_string();
                            s.push('\n');
                            out.push(Ok(bytes::Bytes::from(s)));
                        } else if let Ok(v) = serde_json::from_str::<serde_json::Value>(data) {
                            let content = v
                                .pointer("/choices/0/delta/content")
                                .and_then(serde_json::Value::as_str)
                                .unwrap_or("");
                            if !content.is_empty() {
                                let chunk_line = serde_json::json!({
                                    "model":      model.as_str(),
                                    "created_at": chrono::Utc::now().to_rfc3339(),
                                    "message":    { "role": "assistant", "content": content },
                                    "done":       false
                                });
                                let mut s = chunk_line.to_string();
                                s.push('\n');
                                out.push(Ok(bytes::Bytes::from(s)));
                            }
                        }
                    }
                }
                out
            }
        };
        futures_util::stream::iter(output)
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
                },
                TierConfig {
                    name: "cloud:economy".into(),
                    backend: "mock".into(),
                    model: "economy-model".into(),
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
    async fn chat_completions_returns_500_when_backend_is_unreachable() {
        // Port 1 is reserved and never responds — guaranteed connection refusal.
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
        assert_eq!(resp.status(), StatusCode::INTERNAL_SERVER_ERROR);
        let json = body_json(resp.into_body()).await;
        assert!(json["error"].is_string());
    }
}



