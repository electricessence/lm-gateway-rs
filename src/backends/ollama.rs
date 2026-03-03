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

    /// Send `POST /api/chat` with `stream: true` and return an [`SseStream`] of raw NDJSON.
    ///
    /// Unlike [`chat_completions_stream`], this uses Ollama's native endpoint which
    /// honours Ollama-specific fields such as `think: false`. The returned bytes are
    /// newline-delimited JSON (not SSE) and should be proxied directly to callers
    /// that expect Ollama native format rather than being passed through
    /// `sse_to_ollama_ndjson`.
    pub async fn native_chat_stream(&self, mut body: Value) -> anyhow::Result<SseStream> {
        if let Some(obj) = body.as_object_mut() {
            obj.entry("keep_alive").or_insert(serde_json::json!(-1));
            obj.insert("stream".into(), serde_json::json!(true));
        }
        let url = format!("{}/api/chat", self.base_url);
        let response = self
            .stream_client
            .post(&url)
            .json(&body)
            .send()
            .await
            .with_context(|| format!("POST {url} (native streaming)"))?;
        let stream = response
            .bytes_stream()
            .map(|r| r.map_err(anyhow::Error::from));
        Ok(Box::pin(stream))
    }

    /// Call Ollama's native `/api/chat` for a tool request, returning an
    /// OpenAI-compatible non-streaming response `Value`.
    ///
    /// Handles both native `tool_calls` arrays and the plain-text fallback for
    /// thinking models that emit `HassTurnOn(...)` format instead.
    pub async fn tool_call(&self, mut body: Value) -> anyhow::Result<Value> {
        let (message, model) = self.fetch_native_tool_response(&mut body).await?;
        let ts = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap_or_default()
            .as_secs();
        let id = format!("chatcmpl-tools-{ts}");
        Ok(serde_json::json!({
            "id": id,
            "object": "chat.completion",
            "created": ts,
            "model": model,
            "choices": [{
                "index": 0,
                "message": message,
                "finish_reason": if message.get("tool_calls").is_some() { "tool_calls" } else { "stop" }
            }]
        }))
    }

    /// Buffer a tool-call request via Ollama's native `/api/chat` and return a
    /// synthetic OpenAI-compatible SSE stream.
    ///
    /// Ollama's `/v1/chat/completions` compat layer fails to translate
    /// `<tool_call>` model output to a `tool_calls` JSON array; the native
    /// `/api/chat` endpoint performs that translation correctly.  The full
    /// response is buffered — tool calls are not meaningfully streamed — and
    /// re-emitted as OpenAI SSE chunks so the upstream client (e.g. HA) sees
    /// the standard OpenAI format.
    pub async fn tool_call_stream(&self, mut body: Value) -> anyhow::Result<SseStream> {
        let (message, model) = self.fetch_native_tool_response(&mut body).await?;
        let ts = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap_or_default()
            .as_secs();
        let id = format!("chatcmpl-tools-{ts}");
        let finish_reason = if message.get("tool_calls").is_some() { "tool_calls" } else { "stop" };
        let chunk1 = serde_json::json!({
            "id": id, "object": "chat.completion.chunk", "created": ts, "model": model,
            "choices": [{"index": 0, "delta": message, "finish_reason": null}]
        });
        let chunk2 = serde_json::json!({
            "id": id, "object": "chat.completion.chunk", "created": ts, "model": model,
            "choices": [{"index": 0, "delta": {}, "finish_reason": finish_reason}]
        });
        let parts: Vec<bytes::Bytes> = vec![
            bytes::Bytes::from(format!("data: {}\n\n", chunk1)),
            bytes::Bytes::from(format!("data: {}\n\n", chunk2)),
            bytes::Bytes::from_static(b"data: [DONE]\n\n"),
        ];
        Ok(Box::pin(futures_util::stream::iter(
            parts.into_iter().map(Ok::<_, anyhow::Error>),
        )))
    }

    /// Shared core: POST to `/api/chat` with stream=false, resolve tool calls
    /// (native or plain-text fallback), and return an OpenAI-format message `Value`
    /// plus the model name.
    async fn fetch_native_tool_response(
        &self,
        body: &mut Value,
    ) -> anyhow::Result<(Value, String)> {
        if let Some(obj) = body.as_object_mut() {
            obj.entry("keep_alive").or_insert(serde_json::json!(-1));
            obj.insert("stream".into(), serde_json::json!(false));
        }
        let model = body
            .get("model")
            .and_then(Value::as_str)
            .unwrap_or("unknown")
            .to_owned();
        let url = format!("{}/api/chat", self.base_url);
        let response = self
            .stream_client
            .post(&url)
            .json(&body)
            .send()
            .await
            .with_context(|| format!("POST {url} (tool call)"))?;
        let status = response.status();
        let text = response.text().await.context("reading Ollama native tool response")?;
        if !status.is_success() {
            anyhow::bail!("Ollama returned HTTP {status}: {text}");
        }
        let native: Value = serde_json::from_str(&text)
            .with_context(|| format!("parsing Ollama native tool response: {text}"))?;
        let native_msg = native.pointer("/message").cloned().unwrap_or(Value::Null);
        let tool_calls = native_msg.get("tool_calls").and_then(Value::as_array).cloned();
        let content = native_msg
            .get("content")
            .and_then(Value::as_str)
            .unwrap_or("")
            .to_owned();
        let resolved = tool_calls.or_else(|| Self::parse_plain_text_tool_calls(&content));
        let message = if let Some(tc) = resolved {
            let openai_calls: Vec<Value> = tc
                .iter()
                .enumerate()
                .map(|(i, call)| {
                    let func = call.get("function").cloned().unwrap_or(Value::Null);
                    let name = func.get("name").and_then(Value::as_str).unwrap_or("").to_owned();
                    let args = func.get("arguments").cloned().unwrap_or(Value::Null);
                    let args_str = serde_json::to_string(&args).unwrap_or_default();
                    serde_json::json!({
                        "index": i,
                        "id": format!("call_{i}"),
                        "type": "function",
                        "function": { "name": name, "arguments": args_str }
                    })
                })
                .collect();
            serde_json::json!({ "role": "assistant", "content": null, "tool_calls": openai_calls })
        } else {
            serde_json::json!({ "role": "assistant", "content": content })
        };
        Ok((message, model))
    }

/// Parse Python-style plain-text tool calls emitted by thinking models when
/// the structured `tool_calls` array is absent.
///
/// Matches one or more occurrences of `FunctionName(key="val", ...)` in the
/// content string and converts them to the Ollama native tool_calls format so
/// the existing conversion path can handle them uniformly.
///
/// Example input: `HassTurnOn(area="Office", domain="light")`
fn parse_plain_text_tool_calls(content: &str) -> Option<Vec<Value>> {
    let mut calls = Vec::new();
    let mut remaining = content;

    while let Some(paren) = remaining.find('(') {
        // Walk backwards from the '(' to extract the function name.
        let before = &remaining[..paren];
        let name_start = before
            .rfind(|c: char| !c.is_alphanumeric() && c != '_')
            .map(|i| i + 1)
            .unwrap_or(0);
        let name = before[name_start..].trim();

        if name.is_empty() || !name.chars().next().map(|c| c.is_alphabetic()).unwrap_or(false) {
            remaining = &remaining[paren + 1..];
            continue;
        }

        // Find the matching closing paren.
        let after_paren = &remaining[paren + 1..];
        let Some(close) = after_paren.find(')') else { break };
        let args_str = &after_paren[..close];

        // Parse key="value" pairs.
        let mut args = serde_json::Map::new();
        for pair in args_str.split(',') {
            let pair = pair.trim();
            if let Some(eq) = pair.find('=') {
                let key = pair[..eq].trim().to_owned();
                let val = pair[eq + 1..].trim().trim_matches('"').to_owned();
                if !key.is_empty() {
                    args.insert(key, Value::String(val));
                }
            }
        }

        calls.push(serde_json::json!({
            "function": { "name": name, "arguments": args }
        }));

        remaining = &after_paren[close + 1..];
    }

    if calls.is_empty() { None } else { Some(calls) }
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
