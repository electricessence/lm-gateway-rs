//! Backend client factory and unified dispatch interface.
//!
//! [`BackendClient`] is an enum that wraps a concrete provider adapter chosen
//! at construction time from [`BackendConfig::provider`]. All routing code
//! interacts with the same two-method API (`chat_completions`, `health_check`);
//! adapter-specific protocol differences — schema translation, auth headers,
//! endpoint paths — are fully encapsulated in the adapter modules.

mod anthropic;
mod ollama;
mod openai;

pub use anthropic::AnthropicAdapter;
pub use ollama::OllamaAdapter;
pub use openai::OpenAIAdapter;

use std::pin::Pin;

use bytes::Bytes;
use futures_util::Stream;
use serde_json::Value;

use crate::config::{BackendConfig, Provider};

/// A `Send`-able, heap-allocated SSE byte stream.
///
/// Each item is either a chunk of raw SSE data (already in OpenAI wire format)
/// or an error. The stream terminates when all data has been yielded.
pub type SseStream = Pin<Box<dyn Stream<Item = anyhow::Result<Bytes>> + Send>>;

/// Unified backend client — enum dispatch over concrete provider adapters.
///
/// Constructed via [`BackendClient::new`] from a [`BackendConfig`]. All callers
/// see a single API; the correct adapter is selected once at construction time.
pub enum BackendClient {
    /// OpenAI-compatible passthrough (also used for OpenRouter).
    OpenAI(OpenAIAdapter),
    /// Anthropic Messages API with request/response translation.
    Anthropic(AnthropicAdapter),
    /// Ollama local inference server (OpenAI-compat endpoint).
    Ollama(OllamaAdapter),
}

impl BackendClient {
    /// Build a backend client from config, resolving any API key from the environment.
    ///
    /// # Errors
    /// Returns an error if the configured `api_key_env` variable is required but
    /// unset in the environment (Anthropic always requires a key).
    pub fn new(cfg: &BackendConfig) -> anyhow::Result<Self> {
        let base_url = cfg.base_url.trim_end_matches('/').to_string();
        let api_key = cfg.api_key();

        Ok(match cfg.provider {
            Provider::OpenAI | Provider::OpenRouter => {
                Self::OpenAI(OpenAIAdapter::new(base_url, cfg.timeout_ms, api_key))
            }
            Provider::Ollama => {
                Self::Ollama(OllamaAdapter::new(base_url, cfg.timeout_ms))
            }
            Provider::Anthropic => {
                let key = api_key.ok_or_else(|| {
                    anyhow::anyhow!(
                        "Anthropic backend requires an API key; \
                         configure api_key_env or api_key_secret in the backend config"
                    )
                })?;
                Self::Anthropic(AnthropicAdapter::new(base_url, cfg.timeout_ms, key))
            }
        })
    }

    /// Forward a `/v1/chat/completions` request to the configured backend.
    ///
    /// The request body should have `model` and `stream` already rewritten by
    /// the router before this is called.
    pub async fn chat_completions(&self, request: Value) -> anyhow::Result<Value> {
        match self {
            Self::OpenAI(a) => a.chat_completions(request).await,
            Self::Anthropic(a) => a.chat_completions(request).await,
            Self::Ollama(a) => a.chat_completions(request).await,
        }
    }

    /// Forward a streaming request and return an [`SseStream`].
    ///
    /// All backends produce OpenAI-compatible SSE output:
    /// - OpenAI-compatible and Ollama backends proxy bytes verbatim.
    /// - Anthropic backends translate on-the-fly from Anthropic's SSE schema.
    pub async fn chat_completions_stream(
        &self,
        request: Value,
    ) -> anyhow::Result<SseStream> {
        match self {
            Self::OpenAI(a) => a.chat_completions_stream(request).await,
            Self::Ollama(a) => a.chat_completions_stream(request).await,
            Self::Anthropic(a) => a.chat_completions_stream(request).await,
        }
    }

    /// Stream via the backend's native endpoint when available.
    ///
    /// For Ollama this uses `/api/chat` which honours Ollama-specific fields
    /// such as `think: false`. Returns `(stream, is_native_ndjson)` where
    /// `is_native_ndjson == true` means the bytes are already Ollama NDJSON
    /// and should be proxied directly without SSE→NDJSON conversion.
    ///
    /// All other backends fall back to [`chat_completions_stream`] (SSE) and
    /// return `false`.
    pub async fn native_chat_stream(
        &self,
        request: Value,
    ) -> anyhow::Result<(SseStream, bool)> {
        match self {
            Self::Ollama(a) => Ok((a.native_chat_stream(request).await?, true)),
            _ => Ok((self.chat_completions_stream(request).await?, false)),
        }
    }

    /// Send a classification request to the backend.
    ///
    /// For Ollama backends this routes to the native `/api/chat` endpoint so
    /// that Ollama-specific request fields (e.g. `think`) are honoured.
    /// Other backends fall back to the standard `/v1/chat/completions` path.
    pub async fn classify(&self, request: Value) -> anyhow::Result<Value> {
        match self {
            Self::Ollama(a) => a.classify(request).await,
            _ => self.chat_completions(request).await,
        }
    }

    /// Probe this backend for liveness. Implementation varies by provider.
    pub async fn health_check(&self) -> anyhow::Result<()> {
        match self {
            Self::OpenAI(a) => a.health_check().await,
            Self::Anthropic(a) => a.health_check().await,
            Self::Ollama(a) => a.health_check().await,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::Provider;
    use serde_json::json;
    use wiremock::matchers::{method, path};
    use wiremock::{Mock, MockServer, ResponseTemplate};

    // -----------------------------------------------------------------------
    // Helpers
    // -----------------------------------------------------------------------

    fn cfg_for(server: &MockServer) -> BackendConfig {
        BackendConfig {
            base_url: server.uri(),
            api_key_env: None,
            api_key_secret: None,
            timeout_ms: 5_000,
            provider: Provider::OpenAI,
        }
    }

    fn ok_completion_body() -> serde_json::Value {
        json!({
            "choices": [{
                "message": {
                    "content": "Here is a comprehensive response that is definitely long enough."
                }
            }]
        })
    }

    // -----------------------------------------------------------------------
    // BackendClient::new
    // -----------------------------------------------------------------------

    #[test]
    fn new_succeeds_without_api_key() {
        let cfg = BackendConfig {
            base_url: "http://localhost:11434".into(),
            api_key_env: None,
            api_key_secret: None,
            timeout_ms: 5_000,
            provider: Provider::OpenAI,
        };
        assert!(BackendClient::new(&cfg).is_ok());
    }

    #[test]
    fn new_succeeds_when_configured_api_key_env_var_is_unset() {
        // A missing env var is tolerated for non-Anthropic providers; the key is omitted.
        let cfg = BackendConfig {
            base_url: "http://localhost:11434".into(),
            api_key_env: Some("LMG_TEST_DEFINITELY_NOT_SET_XYZ_99".into()),
            api_key_secret: None,
            timeout_ms: 5_000,
            provider: Provider::OpenAI,
        };
        assert!(BackendClient::new(&cfg).is_ok());
    }

    #[test]
    fn new_resolves_api_key_from_env_var() {
        // Use a unique var name to avoid cross-test interference.
        let var = "LMG_BACKEND_TEST_KEY_RESOLVE_123";
        // SAFETY: single-threaded test setup; env mutation is acceptable here.
        unsafe { std::env::set_var(var, "sk-test-resolved") };
        let cfg = BackendConfig {
            base_url: "http://localhost:11434".into(),
            api_key_env: Some(var.into()),
            api_key_secret: None,
            timeout_ms: 5_000,
            provider: Provider::OpenAI,
        };
        let resolved = cfg.api_key();
        assert_eq!(resolved.as_deref(), Some("sk-test-resolved"));
        unsafe { std::env::remove_var(var) };
    }

    #[test]
    fn api_key_returns_none_when_env_var_field_is_none() {
        let cfg = BackendConfig {
            base_url: "http://x".into(),
            api_key_env: None,
            api_key_secret: None,
            timeout_ms: 5_000,
            provider: Provider::OpenAI,
        };
        assert!(cfg.api_key().is_none());
    }

    // -----------------------------------------------------------------------
    // chat_completions
    // -----------------------------------------------------------------------

    #[tokio::test]
    async fn chat_completions_returns_parsed_json_on_success() {
        let server = MockServer::start().await;
        Mock::given(method("POST"))
            .and(path("/v1/chat/completions"))
            .respond_with(ResponseTemplate::new(200).set_body_json(ok_completion_body()))
            .mount(&server)
            .await;

        let client = BackendClient::new(&cfg_for(&server)).unwrap();
        let result = client
            .chat_completions(json!({"model": "test", "messages": []}))
            .await;

        assert!(result.is_ok(), "expected Ok, got: {result:?}");
        assert!(result.unwrap().pointer("/choices/0/message/content").is_some());
    }

    #[tokio::test]
    async fn chat_completions_errors_on_non_2xx_status() {
        let server = MockServer::start().await;
        Mock::given(method("POST"))
            .and(path("/v1/chat/completions"))
            .respond_with(ResponseTemplate::new(429).set_body_string("rate limited"))
            .mount(&server)
            .await;

        let err = BackendClient::new(&cfg_for(&server))
            .unwrap()
            .chat_completions(json!({"model": "test", "messages": []}))
            .await
            .unwrap_err();

        assert!(
            err.to_string().contains("429"),
            "expected HTTP 429 in error, got: {err}"
        );
    }

    #[tokio::test]
    async fn chat_completions_errors_on_invalid_json_response_body() {
        let server = MockServer::start().await;
        Mock::given(method("POST"))
            .and(path("/v1/chat/completions"))
            .respond_with(ResponseTemplate::new(200).set_body_string("not valid json {{{{"))
            .mount(&server)
            .await;

        let err = BackendClient::new(&cfg_for(&server))
            .unwrap()
            .chat_completions(json!({"model": "test", "messages": []}))
            .await
            .unwrap_err();

        let msg = err.to_string().to_lowercase();
        assert!(
            msg.contains("json") || msg.contains("parsing"),
            "expected json parse error, got: {err}"
        );
    }

    // -----------------------------------------------------------------------
    // health_check
    // -----------------------------------------------------------------------

    #[tokio::test]
    async fn health_check_returns_ok_on_2xx() {
        let server = MockServer::start().await;
        Mock::given(method("GET"))
            .and(path("/v1/models"))
            .respond_with(
                ResponseTemplate::new(200).set_body_json(json!({ "object": "list", "data": [] })),
            )
            .mount(&server)
            .await;

        assert!(
            BackendClient::new(&cfg_for(&server))
                .unwrap()
                .health_check()
                .await
                .is_ok()
        );
    }

    #[tokio::test]
    async fn health_check_errors_on_non_2xx() {
        let server = MockServer::start().await;
        Mock::given(method("GET"))
            .and(path("/v1/models"))
            .respond_with(ResponseTemplate::new(503))
            .mount(&server)
            .await;

        let err = BackendClient::new(&cfg_for(&server))
            .unwrap()
            .health_check()
            .await
            .unwrap_err();

        assert!(
            err.to_string().contains("503"),
            "expected HTTP 503 in error, got: {err}"
        );
    }
}
