//! Ollama-compatible client API handlers.
//!
//! `GET /api/tags` and `POST /api/chat`.

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
    error::AppError,
    router::RouterState,
};

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

    let model_name = body
        .get("model")
        .and_then(Value::as_str)
        .unwrap_or("lm-gateway")
        .to_owned();

    if streaming {
        match crate::router::route_stream(
            &state,
            openai_body,
            effective_profile,
            req_id.as_deref(),
            expert_gate,
            true, // use native /api/chat for Ollama — honours think:false
        )
        .await
        {
            Ok((stream, _entry, is_native)) => {
                if is_native {
                    // Native NDJSON from /api/chat — passthrough directly.
                    return Ok(axum::response::Response::builder()
                        .header("content-type", "application/x-ndjson")
                        .header("cache-control", "no-cache")
                        .header("x-accel-buffering", "no")
                        .body(Body::from_stream(stream))
                        .expect("ollama_chat: failed to build native ndjson response"));
                }
                // Translate OpenAI SSE stream → Ollama NDJSON stream.
                let ndjson = sse_to_ollama_ndjson(model_name.clone(), stream);
                return Ok(axum::response::Response::builder()
                    .header("content-type", "application/x-ndjson")
                    .header("cache-control", "no-cache")
                    .header("x-accel-buffering", "no")
                    .body(Body::from_stream(ndjson))
                    .expect("ollama_chat: failed to build streaming response"));
            }
            Err(e) => return Ok(Json(super::error_ollama_response(&e, &model_name)).into_response()),
        }
    }

    // Non-streaming path: route and convert response.
    openai_body["stream"] = json!(false);
    let response = match crate::router::route(
        &state,
        openai_body,
        effective_profile,
        req_id.as_deref(),
        false,
        expert_gate,
    )
    .await
    {
        Ok((r, _entry)) => r,
        Err(e) => return Ok(Json(super::error_ollama_response(&e, &model_name)).into_response()),
    };

    let model_name = model_name.as_str();
    let choice_message = response.pointer("/choices/0/message");
    let content = choice_message
        .and_then(|m| m.get("content"))
        .and_then(Value::as_str)
        .unwrap_or("");
    // Pass tool_calls through so HA's Ollama integration can execute service calls.
    // OpenAI format has arguments as a JSON string; Ollama native format expects a dict.
    let tool_calls_opt = choice_message
        .and_then(|m| m.get("tool_calls"))
        .filter(|v| !v.is_null())
        .filter(|v| v.as_array().map(|a| !a.is_empty()).unwrap_or(true))
        .cloned()
        .map(|tc| {
            let arr = tc.as_array().cloned().unwrap_or_default();
            let fixed: Vec<serde_json::Value> = arr.into_iter().map(|mut call| {
                if let Some(args_str) = call
                    .pointer("/function/arguments")
                    .and_then(Value::as_str)
                    .map(|s| s.to_owned())
                {
                    if let Ok(parsed) = serde_json::from_str::<serde_json::Value>(&args_str) {
                        if let Some(func) = call.get_mut("function") {
                            func["arguments"] = parsed;
                        }
                    }
                }
                call
            }).collect();
            serde_json::Value::Array(fixed)
        });
    let eval_count = response
        .pointer("/usage/completion_tokens")
        .and_then(Value::as_u64)
        .unwrap_or(0);
    let prompt_count = response
        .pointer("/usage/prompt_tokens")
        .and_then(Value::as_u64)
        .unwrap_or(0);

    let mut message = json!({ "role": "assistant", "content": content });
    if let Some(tc) = tool_calls_opt {
        message["tool_calls"] = tc;
    }

    let ollama_response = json!({
        "model":       model_name,
        "created_at":  chrono::Utc::now().to_rfc3339(),
        "message":     message,
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
/// Content delta chunks are forwarded immediately. `tool_calls` deltas are
/// accumulated across chunks and emitted in full on the closing `done: true`
/// line, which is what the Ollama Python client and Home Assistant expect.
/// This allows HA's Ollama conversation integration to receive properly-formed
/// `tool_calls` and execute service calls rather than seeing empty content.
fn sse_to_ollama_ndjson(
    model: String,
    stream: crate::backends::SseStream,
) -> impl futures_util::Stream<Item = anyhow::Result<bytes::Bytes>> {
    use futures_util::StreamExt;
    use std::collections::BTreeMap;
    use std::sync::{Arc, Mutex};

    /// Accumulates OpenAI streaming tool_call deltas by index.
    #[derive(Default)]
    struct ToolCallBuilder {
        id:        String,
        name:      String,
        arguments: String,
    }

    struct State {
        buf:        String,
        content:    String,
        tool_calls: BTreeMap<usize, ToolCallBuilder>,
    }

    let model = Arc::new(model);
    let state = Arc::new(Mutex::new(State {
        buf:        String::new(),
        content:    String::new(),
        tool_calls: BTreeMap::new(),
    }));

    stream.flat_map(move |chunk_res| {
        let model  = model.clone();
        let state  = state.clone();
        let mut st = state.lock().expect("sse_to_ollama_ndjson state lock");

        let output: Vec<anyhow::Result<bytes::Bytes>> = match chunk_res {
            Err(e) => vec![Err(e)],
            Ok(bytes) => {
                st.buf.push_str(&String::from_utf8_lossy(&bytes));
                let mut out = Vec::new();

                while let Some(pos) = st.buf.find("\n\n") {
                    let event = st.buf[..pos].to_owned();
                    st.buf = st.buf[pos + 2..].to_owned();

                    for line in event.lines() {
                        let data = line.strip_prefix("data: ").unwrap_or(line);

                        if data == "[DONE]" || data.is_empty() {
                            // Assemble accumulated tool_calls (BTreeMap keeps index order).
                            let assembled: Vec<serde_json::Value> = st
                                .tool_calls
                                .iter()
                                .map(|(_, b)| {
                                    // Ollama native format requires arguments as a dict,
                                    // not the JSON string that OpenAI SSE carries.
                                    let args: serde_json::Value =
                                        serde_json::from_str(&b.arguments)
                                            .unwrap_or(serde_json::Value::Object(Default::default()));
                                    serde_json::json!({
                                        "id":   b.id,
                                        "type": "function",
                                        "function": {
                                            "name":      b.name,
                                            "arguments": args
                                        }
                                    })
                                })
                                .collect();

                            let mut done_message =
                                serde_json::json!({ "role": "assistant", "content": st.content });
                            if !assembled.is_empty() {
                                done_message["tool_calls"] = serde_json::json!(assembled);
                            }

                            let done_line = serde_json::json!({
                                "model":       model.as_str(),
                                "created_at":  chrono::Utc::now().to_rfc3339(),
                                "message":     done_message,
                                "done":        true,
                                "done_reason": "stop"
                            });
                            let mut s = done_line.to_string();
                            s.push('\n');
                            out.push(Ok(bytes::Bytes::from(s)));
                        } else if let Ok(v) =
                            serde_json::from_str::<serde_json::Value>(data)
                        {
                            // Accumulate tool_call deltas.
                            if let Some(tc_arr) = v
                                .pointer("/choices/0/delta/tool_calls")
                                .and_then(serde_json::Value::as_array)
                            {
                                for delta in tc_arr {
                                    let idx = delta
                                        .get("index")
                                        .and_then(serde_json::Value::as_u64)
                                        .unwrap_or(0) as usize;
                                    let entry =
                                        st.tool_calls.entry(idx).or_default();
                                    if let Some(id) =
                                        delta.get("id").and_then(serde_json::Value::as_str)
                                    {
                                        entry.id = id.to_owned();
                                    }
                                    if let Some(func) = delta.get("function") {
                                        if let Some(name) = func
                                            .get("name")
                                            .and_then(serde_json::Value::as_str)
                                        {
                                            entry.name.push_str(name);
                                        }
                                        if let Some(args) = func
                                            .get("arguments")
                                            .and_then(serde_json::Value::as_str)
                                        {
                                            entry.arguments.push_str(args);
                                        }
                                    }
                                }
                                // Don't emit a content chunk for tool_call-only deltas.
                            } else {
                                // Regular content delta.
                                let content = v
                                    .pointer("/choices/0/delta/content")
                                    .and_then(serde_json::Value::as_str)
                                    .unwrap_or("");
                                if !content.is_empty() {
                                    st.content.push_str(content);
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
                }
                out
            }
        };
        futures_util::stream::iter(output)
    })
}
