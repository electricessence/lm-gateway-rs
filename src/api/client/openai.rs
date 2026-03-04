//! OpenAI-compatible client API handlers.
//!
//! `POST /v1/chat/completions` and `GET /v1/models`.

use std::sync::Arc;

use axum::{
    body::Body,
    extract::{Extension, State},
    response::{IntoResponse, Response},
    Json,
};
use serde_json::{json, Value};

use crate::{
    api::{client_auth::ClientProfile, request_id::RequestId},
    backends::SseStream,
    error::AppError,
    router::RouterState,
};

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

    let model_name = body
        .get("model")
        .and_then(Value::as_str)
        .unwrap_or("lm-gateway")
        .to_owned();

    if streaming {
        match crate::router::route_stream(&state, body, profile.as_deref(), req_id.as_deref(), expert_gate, false).await {
            Ok((stream, entry, _is_native)) => {
                let mut response = proxy_sse(stream);
                super::inject_routing_headers(response.headers_mut(), &entry, &state.config());
                return Ok(response);
            }
            Err(e) => return Ok(Json(super::error_openai_response(&e, &model_name)).into_response()),
        }
    }

    match crate::router::route(&state, body, profile.as_deref(), req_id.as_deref(), false, expert_gate).await {
        Ok((resp, entry)) => {
            let mut response = Json(resp).into_response();
            super::inject_routing_headers(response.headers_mut(), &entry, &state.config());
            Ok(response)
        }
        Err(e) => Ok(Json(super::error_openai_response(&e, &model_name)).into_response()),
    }
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
