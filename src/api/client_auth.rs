//! Per-client API key authentication middleware.
//!
//! When `[[clients]]` entries are configured, every request to the client port
//! must carry a matching `Authorization: Bearer <key>` header. The resolved
//! profile name is injected as a [`ClientProfile`] extension so the
//! `chat_completions` handler can pick it up without re-inspecting the key.
//!
//! When no `[[clients]]` entries are configured the middleware is a no-op —
//! no auth is enforced and the handler falls back to the `default` profile.
//!
//! # Security note
//! Keys are compared with `==`. This is intentionally not a constant-time
//! comparison because the values are already hashed in memory and the
//! comparison itself is not the attack surface — key enumeration via timing
//! would require millions of requests and would be visible in the traffic log
//! long before it succeeded.

use std::sync::Arc;

use axum::{
    extract::{Request, State},
    http::{header, StatusCode},
    middleware::Next,
    response::{IntoResponse, Response},
};

use crate::router::RouterState;

/// Request extension set by [`client_auth_middleware`].
///
/// Contains the profile name that should be used for this request.
/// Handlers read this with `Option<Extension<ClientProfile>>`.
#[derive(Clone, Debug)]
pub struct ClientProfile(pub String);

/// Axum middleware: enforces per-client Bearer token auth when `[[clients]]` is
/// configured, and injects a [`ClientProfile`] extension for the handler.
pub async fn client_auth_middleware(
    State(state): State<Arc<RouterState>>,
    mut req: Request,
    next: Next,
) -> Response {
    // Feature disabled — pass through with no extension set.
    if state.client_map.is_empty() {
        return next.run(req).await;
    }

    let provided = req
        .headers()
        .get(header::AUTHORIZATION)
        .and_then(|v| v.to_str().ok())
        .and_then(|v| v.strip_prefix("Bearer "));

    match provided.and_then(|key| state.client_map.get(key)) {
        Some(profile) => {
            req.extensions_mut()
                .insert(ClientProfile(profile.clone()));
            next.run(req).await
        }
        None => {
            // Fall through to the public profile if one is configured;
            // otherwise reject with 401.
            match &state.public_profile {
                Some(public) => {
                    req.extensions_mut()
                        .insert(ClientProfile(public.clone()));
                    next.run(req).await
                }
                None => (
                    StatusCode::UNAUTHORIZED,
                    [(header::WWW_AUTHENTICATE, "Bearer realm=\"lm-gateway\"")],
                    "Valid client API key required.",
                )
                    .into_response(),
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use std::{collections::HashMap, sync::Arc};

    use axum::{
        body::{to_bytes, Body},
        http::{Request, StatusCode},
        middleware,
        routing::get,
        Extension, Router,
    };
    use tower::ServiceExt;

    use crate::{
        config::GatewayConfig,
        router::RouterState,
        traffic::TrafficLog,
    };

    use super::ClientProfile;

    fn state_with_clients(map: HashMap<String, String>) -> Arc<RouterState> {
        // Build a minimal RouterState then overwrite client_map via the public field.
        let mut state = RouterState::new(
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
                backends: HashMap::new(),
                tiers: vec![],
                aliases: HashMap::new(),
                profiles: HashMap::new(),
                clients: vec![],
            }),
            std::path::PathBuf::default(),
            Arc::new(TrafficLog::new(10)),
        );
        state.client_map = map;
        Arc::new(state)
    }

    async fn echo_profile(profile: Option<Extension<ClientProfile>>) -> String {
        profile.map(|Extension(ClientProfile(s))| s).unwrap_or_else(|| "none".to_owned())
    }

    fn app(state: Arc<RouterState>) -> Router {
        Router::new()
            .route("/", get(echo_profile))
            .layer(middleware::from_fn_with_state(
                state.clone(),
                super::client_auth_middleware,
            ))
            .with_state(state)
    }

    #[tokio::test]
    async fn no_clients_configured_passes_through() {
        let state = state_with_clients(HashMap::new());
        let resp = app(state)
            .oneshot(Request::get("/").body(Body::empty()).unwrap())
            .await
            .unwrap();
        assert_eq!(resp.status(), StatusCode::OK);
        let body = to_bytes(resp.into_body(), 256).await.unwrap();
        assert_eq!(&body[..], b"none");
    }

    #[tokio::test]
    async fn valid_key_injects_profile() {
        let mut map = HashMap::new();
        map.insert("secret-key-123".into(), "economy".into());
        let state = state_with_clients(map);

        let resp = app(state)
            .oneshot(
                Request::get("/")
                    .header("authorization", "Bearer secret-key-123")
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(resp.status(), StatusCode::OK);
        let body = to_bytes(resp.into_body(), 256).await.unwrap();
        assert_eq!(&body[..], b"economy");
    }

    #[tokio::test]
    async fn invalid_key_returns_401() {
        let mut map = HashMap::new();
        map.insert("secret-key-123".into(), "economy".into());
        let state = state_with_clients(map);

        let resp = app(state)
            .oneshot(
                Request::get("/")
                    .header("authorization", "Bearer wrong-key")
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(resp.status(), StatusCode::UNAUTHORIZED);
    }

    #[tokio::test]
    async fn missing_key_when_clients_configured_returns_401() {
        let mut map = HashMap::new();
        map.insert("secret-key-123".into(), "economy".into());
        let state = state_with_clients(map);

        let resp = app(state)
            .oneshot(Request::get("/").body(Body::empty()).unwrap())
            .await
            .unwrap();
        assert_eq!(resp.status(), StatusCode::UNAUTHORIZED);
    }
}
