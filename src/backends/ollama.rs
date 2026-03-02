//! Ollama adapter.
//!
//! Ollama ships an OpenAI-compatible `/v1/chat/completions` endpoint, so this
//! adapter is intentionally thin — it delegates to the same HTTP path, but
//! handles the keyless-auth case transparently and uses Ollama's root `/`
//! endpoint for health checks rather than `/v1/models`.
//!
//! In the future this adapter can opt into Ollama's native `/api/chat` path
//! to access Ollama-specific features (tool calls, image inputs, etc.) without
//! requiring the compat layer.

use std::time::Duration;

use anyhow::Context;
use futures_util::StreamExt as _;
use reqwest::Client;
use serde_json::Value;

use super::SseStream;

/// Adapter for a locally-running Ollama instance.
pub struct OllamaAdapter {
    /// Buffered requests — has the configured request timeout.
    client: Client,
    /// Streaming requests — no request-level timeout.
    stream_client: Client,
    base_url: String,
}

impl OllamaAdapter {
    /// Build an Ollama adapter. No API key is required for typical local deployments.
    pub fn new(base_url: String, timeout_ms: u64) -> Self {
        let client = Client::builder()
            .timeout(Duration::from_millis(timeout_ms))
            .build()
            .expect("failed to build reqwest client");

        let stream_client = Client::builder()
            .build()
            .expect("failed to build streaming reqwest client");

        Self { client, stream_client, base_url }
    }

    /// Forward a chat completions request via Ollama's OpenAI-compat endpoint.
    pub async fn chat_completions(&self, mut body: Value) -> anyhow::Result<Value> {
        // Keep the model loaded in Ollama's memory indefinitely so subsequent
        // requests don't pay the cold-start penalty (can be 10–30 s for 8b models).
        if let Some(obj) = body.as_object_mut() {
            obj.entry("keep_alive").or_insert(serde_json::json!(-1));
        }
        let url = format!("{}/v1/chat/completions", self.base_url);
        let response = self
            .client
            .post(&url)
            .json(&body)
            .send()
            .await
            .with_context(|| format!("POST {url}"))?;

        let status = response.status();
        let text = response.text().await.context("reading Ollama response body")?;

        if !status.is_success() {
            anyhow::bail!("Ollama returned HTTP {status}: {text}");
        }

        serde_json::from_str(&text)
            .with_context(|| format!("parsing Ollama response as JSON: {text}"))
    }

    /// Send `POST /v1/chat/completions` and return an [`SseStream`] for proxying.
    ///
    /// The backend response bytes are forwarded verbatim.
    pub async fn chat_completions_stream(&self, mut body: Value) -> anyhow::Result<SseStream> {
        // Keep the model hot after streaming responses too.
        if let Some(obj) = body.as_object_mut() {
            obj.entry("keep_alive").or_insert(serde_json::json!(-1));
        }
        let url = format!("{}/v1/chat/completions", self.base_url);
        let response = self
            .stream_client
            .post(&url)
            .json(&body)
            .send()
            .await
            .with_context(|| format!("POST {url} (streaming)"))?;
        let stream = response
            .bytes_stream()
            .map(|r| r.map_err(anyhow::Error::from));
        Ok(Box::pin(stream))
    }

    /// Send a classification request via Ollama's native `/api/chat` endpoint.
    ///
    /// The native path honours Ollama-specific request fields such as `think`,
    /// which the OpenAI-compat `/v1/chat/completions` endpoint silently ignores.
    /// Returns an OpenAI-compat response shape so the caller can use the same
    /// [`parse_classification_label`] logic.
    ///
    /// [`parse_classification_label`]: crate::router::parse_classification_label
    pub async fn classify(&self, body: Value) -> anyhow::Result<Value> {
        let url = format!("{}/api/chat", self.base_url);
        let response = self
            .client
            .post(&url)
            .json(&body)
            .send()
            .await
            .with_context(|| format!("POST {url}"))?;

        let status = response.status();
        let text = response.text().await.context("reading Ollama native response body")?;

        if !status.is_success() {
            anyhow::bail!("Ollama returned HTTP {status}: {text}");
        }

        let native: Value = serde_json::from_str(&text)
            .with_context(|| format!("parsing Ollama native response as JSON: {text}"))?;

        // Convert native /api/chat shape → OpenAI-compat so parse_classification_label works.
        let content = native
            .pointer("/message/content")
            .and_then(Value::as_str)
            .unwrap_or("")
            .to_owned();

        Ok(serde_json::json!({
            "choices": [{
                "message": {
                    "role": "assistant",
                    "content": content
                }
            }]
        }))
    }

    /// Probe Ollama's root endpoint (`GET /`) — returns `"Ollama is running"` on success.
    pub async fn health_check(&self) -> anyhow::Result<()> {
        let url = format!("{}/", self.base_url);
        let response = self
            .client
            .get(&url)
            .send()
            .await
            .with_context(|| format!("GET {url}"))?;

        anyhow::ensure!(
            response.status().is_success(),
            "Ollama health check returned HTTP {}",
            response.status()
        );
        Ok(())
    }
}
