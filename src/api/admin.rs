//! Admin API (port 8081) — operator-facing introspection endpoints.
//!
//! These endpoints are separated onto a different port so they can be
//! network-restricted independently of the client API (e.g. accessible only
//! from the internal Docker network, never exposed to the internet).

use std::sync::Arc;

use axum::{
    extract::{Query, State},
    http::{header, StatusCode},
    middleware,
    response::IntoResponse,
    routing::{get, post},
    Json, Router,
};
use serde::Deserialize;
use serde_json::{json, Value};

use crate::{backends::BackendClient, router::RouterState};

/// Build the admin-facing axum router (port 8081).
pub fn router(state: Arc<RouterState>) -> Router {
    Router::new()
        .route("/", get(dashboard))
        .route("/admin/health", get(health))
        .route("/admin/traffic", get(traffic))
        .route("/admin/config", get(config))
        .route("/admin/backends/health", get(backends_health))
        .route("/admin/reload", post(reload))
        .route("/metrics", get(super::metrics::metrics))
        .layer(middleware::from_fn_with_state(
            state.clone(),
            super::admin_auth::admin_auth_middleware,
        ))
        .with_state(state)
}

/// GET / — admin dashboard (single-page UI)
pub async fn dashboard() -> impl IntoResponse {
    const HTML: &str = include_str!("admin_ui.html");
    (StatusCode::OK, [(header::CONTENT_TYPE, "text/html; charset=utf-8")], HTML)
}

/// GET /admin/health — checks liveness + optional backend probes
pub async fn health(State(state): State<Arc<RouterState>>) -> impl IntoResponse {
    let cfg = state.config();
    let tier_count = cfg.tiers.len();
    let backend_count = cfg.backends.len();
    // Count backends that have a key source configured but couldn't resolve it.
    let unconfigured = cfg
        .backends
        .values()
        .filter(|b| {
            b.has_api_key_configured()
                && b.api_key().map(|k: String| k.is_empty()).unwrap_or(true)
        })
        .count();
    Json(json!({
        "status": "ok",
        "ready": unconfigured == 0,
        "tiers": tier_count,
        "backends": backend_count,
    }))
}

#[derive(Deserialize)]
pub struct TrafficQuery {
    #[serde(default = "default_limit")]
    limit: usize,
}
fn default_limit() -> usize {
    100
}

/// GET /admin/traffic?limit=N — recent N traffic entries (default 100)
pub async fn traffic(
    State(state): State<Arc<RouterState>>,
    Query(q): Query<TrafficQuery>,
) -> impl IntoResponse {
    let entries = state.traffic.recent(q.limit).await;
    let stats = state.traffic.stats().await;
    Json(json!({
        "stats": stats,
        "entries": entries,
    }))
}

/// GET /admin/config — returns the current config with secrets redacted
pub async fn config(State(state): State<Arc<RouterState>>) -> impl IntoResponse {
    let cfg = state.config();

    // Redact secrets completely — only expose whether a key is configured
    let backends: Vec<Value> = cfg
        .backends
        .iter()
        .map(|(name, b)| {
            json!({
                "name": name,
                "provider": b.provider.to_string(),
                "base_url": b.base_url,
                "has_api_key": b.has_api_key_configured(),
                "api_key_source": b.api_key_source_type(),
                // Expose the env var *name* (never the resolved value) for diagnostics.
                "api_key_env": b.api_key_env,
            })
        })
        .collect();

    let tiers: Vec<Value> = cfg
        .tiers
        .iter()
        .map(|t| {
            json!({
                "name": t.name,
                "backend": t.backend,
                "model": t.model,
            })
        })
        .collect();

    let profiles: Value = cfg
        .profiles
        .iter()
        .map(|(name, p)| {
            (
                name.clone(),
                json!({
                    "mode": p.mode.to_string(),
                    "classifier": p.classifier,
                    "max_auto_tier": p.max_auto_tier,
                    "expert_requires_flag": p.expert_requires_flag,
                    "rate_limit_rpm": p.rate_limit_rpm,
                }),
            )
        })
        .collect::<serde_json::Map<_, _>>()
        .into();

    Json(json!({
        "gateway": {
            "client_port": cfg.gateway.client_port,
            "admin_port": cfg.gateway.admin_port,
            "traffic_log_capacity": cfg.gateway.traffic_log_capacity,
        },
        "backends": backends,
        "tiers": tiers,
        "aliases": cfg.aliases,
        "profiles": profiles,
    }))
}

/// GET /admin/backends/health — probe every configured backend
pub async fn backends_health(State(state): State<Arc<RouterState>>) -> impl IntoResponse {
    let cfg = state.config();
    let health_window = cfg.gateway.health_window.unwrap_or(10);
    let health_threshold = cfg.gateway.health_error_threshold.unwrap_or(0.7);
    // Snapshot of traffic-based backend health (empty when no traffic yet or window=0).
    let traffic_health = if health_window > 0 {
        state.traffic.backend_health(health_window, health_threshold).await
    } else {
        std::collections::HashMap::new()
    };

    let mut results = Vec::new();

    for (name, backend_cfg) in &cfg.backends {
        let traffic = traffic_health.get(name).map(|h| {
            json!({
                "window": h.total,
                "errors": h.errors,
                "error_rate": h.error_rate,
                "healthy": h.healthy,
            })
        });

        let client = match BackendClient::new(backend_cfg) {
            Ok(c) => c,
            Err(e) => {
                results.push(json!({
                    "backend": name,
                    "status": "error",
                    "error": e.to_string(),
                    "traffic": traffic,
                }));
                continue;
            }
        };

        match client.health_check().await {
            Ok(_) => results.push(json!({
                "backend": name,
                "status": "ok",
                "traffic": traffic,
            })),
            Err(e) => results.push(json!({
                "backend": name,
                "status": "unreachable",
                "error": e.to_string(),
                "traffic": traffic,
            })),
        }
    }

    let all_ok = results.iter().all(|r| r["status"] == "ok");
    let status = if all_ok {
        StatusCode::OK
    } else {
        StatusCode::MULTI_STATUS
    };

    (status, Json(json!({ "backends": results })))
}

/// POST /admin/reload — re-read the config file from disk and apply it live.
///
/// The response is `200 OK` on success or `422 Unprocessable Entity` if the
/// file cannot be parsed. Either way the currently active config is left
/// unchanged on failure so the gateway keeps running.
pub async fn reload(State(state): State<Arc<RouterState>>) -> impl IntoResponse {
    match crate::config::Config::load(&state.config_path) {
        Ok(new_cfg) => {
            state.replace_config(Arc::new(new_cfg));
            tracing::info!("config reloaded via POST /admin/reload");
            Json(json!({ "status": "reloaded" })).into_response()
        }
        Err(e) => (
            StatusCode::UNPROCESSABLE_ENTITY,
            Json(json!({ "error": e.to_string() })),
        )
            .into_response(),
    }
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
        traffic::{TrafficEntry, TrafficLog},
    };

    // -----------------------------------------------------------------------
    // Test helpers
    // -----------------------------------------------------------------------

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
                        api_key_env: Some("LMG_ADMIN_TEST_KEY".into()), // deliberately unset
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
                        mode: RoutingMode::Escalate,
                        classifier: "local:fast".into(),
                        max_auto_tier: "local:fast".into(),
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

    fn minimal_state() -> Arc<RouterState> {
        state_with_backend("http://127.0.0.1:0")
    }

    async fn body_json(body: Body) -> serde_json::Value {
        let bytes = to_bytes(body, usize::MAX).await.unwrap();
        serde_json::from_slice(&bytes).unwrap()
    }

    // -----------------------------------------------------------------------
    // GET /admin/health
    // -----------------------------------------------------------------------

    #[tokio::test]
    async fn health_returns_ok_with_tier_and_backend_counts() {
        let app = super::router(minimal_state());
        let req = Request::builder()
            .method("GET")
            .uri("/admin/health")
            .body(Body::empty())
            .unwrap();

        let resp = app.oneshot(req).await.unwrap();
        assert_eq!(resp.status(), StatusCode::OK);

        let json = body_json(resp.into_body()).await;
        assert_eq!(json["status"], "ok");
        assert_eq!(json["tiers"], 1);
        assert_eq!(json["backends"], 1);
    }

    // -----------------------------------------------------------------------
    // GET /admin/traffic
    // -----------------------------------------------------------------------

    #[tokio::test]
    async fn traffic_returns_stats_and_empty_entries_list_on_fresh_log() {
        let app = super::router(minimal_state());
        let req = Request::builder()
            .method("GET")
            .uri("/admin/traffic?limit=10")
            .body(Body::empty())
            .unwrap();

        let resp = app.oneshot(req).await.unwrap();
        assert_eq!(resp.status(), StatusCode::OK);

        let json = body_json(resp.into_body()).await;
        assert_eq!(json["stats"]["total_requests"], 0);
        assert!(json["entries"].as_array().unwrap().is_empty());
    }

    #[tokio::test]
    async fn traffic_returns_pushed_entries_newest_first() {
        let state = minimal_state();
        state.traffic.push(TrafficEntry::new("local:fast".into(), "mock".into(), 50, true));
        state.traffic.push(TrafficEntry::new("cloud:economy".into(), "mock".into(), 150, true));

        let app = super::router(Arc::clone(&state));
        let req = Request::builder()
            .method("GET")
            .uri("/admin/traffic?limit=10")
            .body(Body::empty())
            .unwrap();

        let resp = app.oneshot(req).await.unwrap();
        let json = body_json(resp.into_body()).await;
        let entries = json["entries"].as_array().unwrap();
        assert_eq!(entries.len(), 2);
        // Newest first — cloud:economy was pushed last
        assert_eq!(entries[0]["tier"], "cloud:economy");
        assert_eq!(entries[1]["tier"], "local:fast");
    }

    // -----------------------------------------------------------------------
    // GET /admin/config
    // -----------------------------------------------------------------------

    #[tokio::test]
    async fn config_redacts_api_key_value_and_shows_env_var_name() {
        let app = super::router(minimal_state());
        let req = Request::builder()
            .method("GET")
            .uri("/admin/config")
            .body(Body::empty())
            .unwrap();

        let resp = app.oneshot(req).await.unwrap();
        assert_eq!(resp.status(), StatusCode::OK);

        let json = body_json(resp.into_body()).await;
        let backends = json["backends"].as_array().unwrap();
        assert_eq!(backends.len(), 1);

        // The env var *name* is shown, but no resolved secret value
        let b = &backends[0];
        assert_eq!(b["api_key_env"], "LMG_ADMIN_TEST_KEY");
        assert!(b.get("api_key").is_none(), "raw api_key must not be in response");
    }

    #[tokio::test]
    async fn config_serializes_routing_mode_using_display_not_debug() {
        let app = super::router(minimal_state());
        let req = Request::builder()
            .method("GET")
            .uri("/admin/config")
            .body(Body::empty())
            .unwrap();

        let resp = app.oneshot(req).await.unwrap();
        let json = body_json(resp.into_body()).await;

        // RoutingMode::Escalate.to_string() == "escalate" (not "Escalate" from Debug)
        let mode = &json["profiles"]["default"]["mode"];
        assert_eq!(mode, "escalate", "mode should use Display impl: {mode}");
    }

    // -----------------------------------------------------------------------
    // GET /admin/backends/health
    // -----------------------------------------------------------------------

    #[tokio::test]
    async fn backends_health_returns_200_all_ok_when_backends_respond() {
        let server = MockServer::start().await;
        Mock::given(method("GET"))
            .and(path("/v1/models"))
            .respond_with(
                ResponseTemplate::new(200).set_body_json(json!({ "object": "list", "data": [] })),
            )
            .mount(&server)
            .await;

        let app = super::router(state_with_backend(&server.uri()));
        let req = Request::builder()
            .method("GET")
            .uri("/admin/backends/health")
            .body(Body::empty())
            .unwrap();

        let resp = app.oneshot(req).await.unwrap();
        assert_eq!(resp.status(), StatusCode::OK);
        let json = body_json(resp.into_body()).await;
        let backends = json["backends"].as_array().unwrap();
        assert_eq!(backends[0]["status"], "ok");
    }

    #[tokio::test]
    async fn backends_health_returns_multi_status_when_any_backend_is_down() {
        // Port 1 is reserved and never responds
        let app = super::router(state_with_backend("http://127.0.0.1:1"));
        let req = Request::builder()
            .method("GET")
            .uri("/admin/backends/health")
            .body(Body::empty())
            .unwrap();

        let resp = app.oneshot(req).await.unwrap();
        assert_eq!(resp.status(), StatusCode::MULTI_STATUS);
        let json = body_json(resp.into_body()).await;
        let backends = json["backends"].as_array().unwrap();
        assert_eq!(backends[0]["status"], "unreachable");
    }
}



