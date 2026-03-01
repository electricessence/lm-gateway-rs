//! wyoming-bridge — translates Wyoming STT/TTS protocol to OpenAI-compatible HTTP.
//!
//! Wyoming is the TCP-based voice protocol used by Home Assistant voice pipelines.
//! This bridge translates between Wyoming's binary framing and the HTTP APIs
//! exposed by whisper.cpp (STT) and Kokoro (TTS) — no extra models, no extra RAM.
//!
//! ## Configuration (environment variables)
//!
//! | Variable    | Default                 | Description                     |
//! |-------------|-------------------------|---------------------------------|
//! | `STT_URL`   | `http://127.0.0.1:8001` | Base URL for the STT service    |
//! | `TTS_URL`   | `http://127.0.0.1:8003` | Base URL for the TTS service    |
//! | `STT_PORT`  | `10300`                 | TCP port for the ASR listener   |
//! | `TTS_PORT`  | `10200`                 | TCP port for the TTS listener   |
//! | `TTS_VOICE` | `am_onyx`               | Voice name for Kokoro TTS       |
//! | `LOG_LEVEL` | `info`                  | tracing log level               |

use std::sync::Arc;

use anyhow::{Context, Result};
use reqwest::{multipart, Client};
use serde_json::{json, Value};
use tokio::{
    io::{AsyncBufReadExt, AsyncReadExt, AsyncWriteExt, BufReader},
    net::{TcpListener, TcpStream},
};
use tracing::{error, info, warn};

// ---------------------------------------------------------------------------
// Config
// ---------------------------------------------------------------------------

#[derive(Clone, Debug)]
struct BridgeConfig {
    stt_url: String,
    tts_url: String,
    stt_port: u16,
    tts_port: u16,
    tts_voice: String,
}

impl BridgeConfig {
    fn from_env() -> Self {
        Self {
            stt_url: std::env::var("STT_URL")
                .unwrap_or_else(|_| "http://127.0.0.1:8001".into()),
            tts_url: std::env::var("TTS_URL")
                .unwrap_or_else(|_| "http://127.0.0.1:8003".into()),
            stt_port: std::env::var("STT_PORT")
                .ok()
                .and_then(|v| v.parse().ok())
                .unwrap_or(10300),
            tts_port: std::env::var("TTS_PORT")
                .ok()
                .and_then(|v| v.parse().ok())
                .unwrap_or(10200),
            tts_voice: std::env::var("TTS_VOICE").unwrap_or_else(|_| "am_onyx".into()),
        }
    }
}

// ---------------------------------------------------------------------------
// Wyoming message framing
// ---------------------------------------------------------------------------

/// A decoded Wyoming protocol message.
///
/// The wire format is:
/// 1. A single JSON line (UTF-8, terminated with `\n`)
/// 2. Optional `data_length` bytes of additional JSON (merged into `data`)
/// 3. Optional `payload_length` bytes of binary payload (e.g. PCM audio)
struct Msg {
    type_: String,
    data: Value,
    payload: Vec<u8>,
}

impl Msg {
    /// Read one Wyoming message from a buffered async reader.
    async fn read<R>(reader: &mut BufReader<R>) -> Result<Self>
    where
        R: AsyncReadExt + Unpin,
    {
        let mut line = String::new();
        let n = reader.read_line(&mut line).await?;
        if n == 0 {
            anyhow::bail!("connection closed");
        }

        let header: Value =
            serde_json::from_str(line.trim()).context("invalid Wyoming header")?;
        let type_ = header["type"]
            .as_str()
            .context("missing 'type' in Wyoming header")?
            .to_owned();

        let data = header
            .get("data")
            .cloned()
            .unwrap_or(Value::Object(Default::default()));
        let data_len = header
            .get("data_length")
            .and_then(Value::as_u64)
            .unwrap_or(0) as usize;
        let payload_len = header
            .get("payload_length")
            .and_then(Value::as_u64)
            .unwrap_or(0) as usize;

        // Merge additional data block (rarely used in practice).
        let data = if data_len > 0 {
            let mut extra = vec![0u8; data_len];
            reader.read_exact(&mut extra).await?;
            let extra: Value =
                serde_json::from_slice(&extra).unwrap_or(Value::Object(Default::default()));
            merge_json(data, extra)
        } else {
            data
        };

        let mut payload = vec![0u8; payload_len];
        if payload_len > 0 {
            reader.read_exact(&mut payload).await?;
        }

        Ok(Msg { type_, data, payload })
    }

    /// Write one Wyoming message to a writable async stream.
    async fn write<W>(writer: &mut W, type_: &str, data: Value, payload: &[u8]) -> Result<()>
    where
        W: AsyncWriteExt + Unpin,
    {
        let mut header = json!({ "type": type_, "data": data });
        if !payload.is_empty() {
            header["payload_length"] = json!(payload.len());
        }
        let mut line = serde_json::to_string(&header)?;
        line.push('\n');
        writer.write_all(line.as_bytes()).await?;
        if !payload.is_empty() {
            writer.write_all(payload).await?;
        }
        writer.flush().await?;
        Ok(())
    }
}

/// Shallow-merge two JSON objects (extra keys overwrite base on collision).
fn merge_json(mut base: Value, extra: Value) -> Value {
    if let (Value::Object(b), Value::Object(e)) = (&mut base, extra) {
        for (k, v) in e {
            b.insert(k, v);
        }
    }
    base
}

// ---------------------------------------------------------------------------
// Wyoming Info responses
// ---------------------------------------------------------------------------

fn attribution() -> Value {
    json!({
        "name": "wyoming-bridge (lm-gateway-rs)",
        "url":  "https://github.com/electricessence/lm-gateway-rs"
    })
}

fn asr_info() -> Value {
    json!({
        "asr": [{
            "name":        "wyoming-bridge-asr",
            "attribution": attribution(),
            "installed":   true,
            "description": "whisper.cpp via HTTP bridge",
            "version":     null,
            "models": [{
                "name":        "whisper-1",
                "attribution": attribution(),
                "installed":   true,
                "description": "Whisper via HTTP",
                "version":     null,
                "languages":   ["en"]
            }],
            "supports_transcript_streaming": false
        }],
        "tts":    [],
        "handle": [],
        "intent": [],
        "wake":   [],
        "mic":    [],
        "snd":    []
    })
}

fn tts_info(voice: &str) -> Value {
    json!({
        "asr": [],
        "tts": [{
            "name":        "wyoming-bridge-tts",
            "attribution": attribution(),
            "installed":   true,
            "description": "Kokoro TTS via HTTP bridge",
            "version":     null,
            "voices": [{
                "name":        voice,
                "attribution": attribution(),
                "installed":   true,
                "description": "Kokoro voice",
                "version":     null,
                "languages":   ["en"],
                "speakers":    null
            }],
            "supports_synthesize_streaming": false
        }],
        "handle": [],
        "intent": [],
        "wake":   [],
        "mic":    [],
        "snd":    []
    })
}

// ---------------------------------------------------------------------------
// WAV helpers
// ---------------------------------------------------------------------------

/// Wrap raw 16-bit PCM bytes in a minimal RIFF/WAV container.
fn encode_wav(pcm: &[u8], sample_rate: u32, channels: u16, bits_per_sample: u16) -> Vec<u8> {
    let byte_rate = sample_rate * u32::from(channels) * u32::from(bits_per_sample / 8);
    let block_align = channels * (bits_per_sample / 8);
    let data_len = pcm.len() as u32;
    let file_len = 36 + data_len;
    let mut wav = Vec::with_capacity(44 + pcm.len());
    wav.extend_from_slice(b"RIFF");
    wav.extend_from_slice(&file_len.to_le_bytes());
    wav.extend_from_slice(b"WAVE");
    wav.extend_from_slice(b"fmt ");
    wav.extend_from_slice(&16u32.to_le_bytes());
    wav.extend_from_slice(&1u16.to_le_bytes()); // PCM
    wav.extend_from_slice(&channels.to_le_bytes());
    wav.extend_from_slice(&sample_rate.to_le_bytes());
    wav.extend_from_slice(&byte_rate.to_le_bytes());
    wav.extend_from_slice(&block_align.to_le_bytes());
    wav.extend_from_slice(&bits_per_sample.to_le_bytes());
    wav.extend_from_slice(b"data");
    wav.extend_from_slice(&data_len.to_le_bytes());
    wav.extend_from_slice(pcm);
    wav
}

/// Parse a WAV file to locate the raw PCM region.
///
/// Returns `(sample_rate, channels, bits_per_sample, pcm_start_offset)`.
fn parse_wav(data: &[u8]) -> Option<(u32, u16, u16, usize)> {
    if data.len() < 44 {
        return None;
    }
    if &data[0..4] != b"RIFF" || &data[8..12] != b"WAVE" || &data[12..16] != b"fmt " {
        return None;
    }
    let channels = u16::from_le_bytes([data[22], data[23]]);
    let rate = u32::from_le_bytes([data[24], data[25], data[26], data[27]]);
    let bps = u16::from_le_bytes([data[34], data[35]]);
    let fmt_size = u32::from_le_bytes([data[16], data[17], data[18], data[19]]) as usize;
    // Scan for the "data" sub-chunk that follows the fmt chunk.
    let mut pos = 20 + fmt_size;
    while pos + 8 <= data.len() {
        let tag = &data[pos..pos + 4];
        let chunk_len =
            u32::from_le_bytes([data[pos + 4], data[pos + 5], data[pos + 6], data[pos + 7]])
                as usize;
        if tag == b"data" {
            return Some((rate, channels, bps, pos + 8));
        }
        pos += 8 + chunk_len;
    }
    None
}

// ---------------------------------------------------------------------------
// STT handler
// ---------------------------------------------------------------------------

async fn handle_stt(stream: TcpStream, config: Arc<BridgeConfig>, client: Client) {
    let peer = stream.peer_addr().ok();
    info!(?peer, "new STT connection");
    if let Err(e) = run_stt(stream, &config, &client).await {
        // "connection closed" is normal; only log unexpected errors.
        if !e.to_string().contains("connection closed") {
            warn!(?peer, "STT session error: {e:#}");
        } else {
            info!(?peer, "STT connection closed");
        }
    }
}

async fn run_stt(stream: TcpStream, config: &BridgeConfig, client: &Client) -> Result<()> {
    let (read_half, mut write_half) = stream.into_split();
    let mut reader = BufReader::new(read_half);

    // Wyoming servers send info immediately on connect; HA does not send describe first.
    Msg::write(&mut write_half, "info", asr_info(), &[]).await?;

    let mut pcm_buf: Vec<u8> = Vec::new();
    let mut audio_rate: u32 = 16_000;
    let mut audio_channels: u16 = 1;
    let mut audio_width: u16 = 2; // bytes per sample

    loop {
        let msg = Msg::read(&mut reader).await?;
        match msg.type_.as_str() {
            "describe" => {
                // Re-send info if HA asks again (e.g. during re-discovery).
                Msg::write(&mut write_half, "info", asr_info(), &[]).await?;
            }
            "transcribe" => {
                // Optional pre-flight hint — no action needed.
            }
            "audio-start" => {
                pcm_buf.clear();
                audio_rate =
                    msg.data.get("rate").and_then(Value::as_u64).unwrap_or(16_000) as u32;
                audio_channels =
                    msg.data.get("channels").and_then(Value::as_u64).unwrap_or(1) as u16;
                audio_width =
                    msg.data.get("width").and_then(Value::as_u64).unwrap_or(2) as u16;
            }
            "audio-chunk" => {
                pcm_buf.extend_from_slice(&msg.payload);
            }
            "audio-stop" => {
                // Wrap accumulated PCM in a WAV container and send to whisper.cpp.
                let wav = encode_wav(&pcm_buf, audio_rate, audio_channels, audio_width * 8);
                let text = transcribe(client, &config.stt_url, wav)
                    .await
                    .unwrap_or_else(|e| {
                        error!("STT HTTP call failed: {e:#}");
                        String::new()
                    });
                info!(%text, "STT transcript");
                Msg::write(&mut write_half, "transcript", json!({ "text": text }), &[])
                    .await?;
            }
            other => {
                warn!(type_ = other, "STT: ignoring unknown event type");
            }
        }
    }
}

/// POST accumulated PCM (as WAV) to the whisper.cpp OpenAI-compatible endpoint.
async fn transcribe(client: &Client, stt_url: &str, wav: Vec<u8>) -> Result<String> {
    let file_part = multipart::Part::bytes(wav)
        .file_name("audio.wav")
        .mime_str("audio/wav")?;
    let form = multipart::Form::new()
        .part("file", file_part)
        .text("model", "whisper-1")
        .text("response_format", "json");

    let resp = client
        .post(format!("{stt_url}/v1/audio/transcriptions"))
        .multipart(form)
        .send()
        .await
        .context("STT HTTP request failed")?;

    let body: Value = resp.json().await.context("STT response parse failed")?;
    Ok(body
        .get("text")
        .and_then(Value::as_str)
        .unwrap_or("")
        .to_owned())
}

// ---------------------------------------------------------------------------
// TTS handler
// ---------------------------------------------------------------------------

async fn handle_tts(stream: TcpStream, config: Arc<BridgeConfig>, client: Client) {
    let peer = stream.peer_addr().ok();
    info!(?peer, "new TTS connection");
    if let Err(e) = run_tts(stream, &config, &client).await {
        if !e.to_string().contains("connection closed") {
            warn!(?peer, "TTS session error: {e:#}");
        } else {
            info!(?peer, "TTS connection closed");
        }
    }
}

async fn run_tts(stream: TcpStream, config: &BridgeConfig, client: &Client) -> Result<()> {
    let (read_half, mut write_half) = stream.into_split();
    let mut reader = BufReader::new(read_half);

    // Wyoming servers send info immediately on connect; HA does not send describe first.
    Msg::write(&mut write_half, "info", tts_info(&config.tts_voice), &[]).await?;

    loop {
        let msg = Msg::read(&mut reader).await?;
        match msg.type_.as_str() {
            "describe" => {
                // Re-send info if HA asks again.
                Msg::write(&mut write_half, "info", tts_info(&config.tts_voice), &[]).await?;
            }
            "synthesize" => {
                let text = msg
                    .data
                    .get("text")
                    .and_then(Value::as_str)
                    .unwrap_or("")
                    .to_owned();
                let voice = msg
                    .data
                    .pointer("/voice/name")
                    .and_then(Value::as_str)
                    .unwrap_or(&config.tts_voice)
                    .to_owned();

                info!(voice = %voice, "TTS synthesize: {:.80}", text);

                match synthesize(client, &config.tts_url, &text, &voice).await {
                    Ok(wav_bytes) => {
                        // Parse WAV header so we report the correct audio format to HA.
                        let (rate, channels, bps, pcm_start) =
                            parse_wav(&wav_bytes).unwrap_or((24_000, 1, 16, 44));
                        let pcm = &wav_bytes[pcm_start..];
                        let width = bps / 8;

                        Msg::write(
                            &mut write_half,
                            "audio-start",
                            json!({ "rate": rate, "width": width, "channels": channels }),
                            &[],
                        )
                        .await?;

                        // Stream PCM back in 4 KB chunks.
                        for chunk in pcm.chunks(4096) {
                            Msg::write(
                                &mut write_half,
                                "audio-chunk",
                                json!({ "rate": rate, "width": width, "channels": channels }),
                                chunk,
                            )
                            .await?;
                        }

                        Msg::write(&mut write_half, "audio-stop", json!({}), &[]).await?;
                    }
                    Err(e) => {
                        error!("TTS HTTP call failed: {e:#}");
                    }
                }
            }
            other => {
                warn!(type_ = other, "TTS: ignoring unknown event type");
            }
        }
    }
}

/// POST text to the Kokoro OpenAI-compatible TTS endpoint; returns WAV bytes.
async fn synthesize(client: &Client, tts_url: &str, text: &str, voice: &str) -> Result<Vec<u8>> {
    let resp = client
        .post(format!("{tts_url}/v1/audio/speech"))
        .json(&json!({
            "model":           "kokoro",
            "input":           text,
            "voice":           voice,
            "response_format": "wav"
        }))
        .send()
        .await
        .context("TTS HTTP request failed")?;

    let bytes = resp.bytes().await.context("TTS response body read failed")?;
    Ok(bytes.to_vec())
}

// ---------------------------------------------------------------------------
// Entry point
// ---------------------------------------------------------------------------

#[tokio::main]
async fn main() -> Result<()> {
    let log_level = std::env::var("LOG_LEVEL").unwrap_or_else(|_| "info".into());
    tracing_subscriber::fmt()
        .with_env_filter(tracing_subscriber::EnvFilter::new(log_level))
        .init();

    let config = Arc::new(BridgeConfig::from_env());
    let client = Client::new();

    info!(
        stt_url = %config.stt_url,
        tts_url = %config.tts_url,
        stt_port = config.stt_port,
        tts_port = config.tts_port,
        tts_voice = %config.tts_voice,
        "wyoming-bridge starting"
    );

    let stt_listener = TcpListener::bind(format!("0.0.0.0:{}", config.stt_port))
        .await
        .with_context(|| format!("failed to bind STT port {}", config.stt_port))?;

    let tts_listener = TcpListener::bind(format!("0.0.0.0:{}", config.tts_port))
        .await
        .with_context(|| format!("failed to bind TTS port {}", config.tts_port))?;

    info!(port = config.stt_port, "STT (ASR) listener ready");
    info!(port = config.tts_port, "TTS listener ready");

    let stt_config = config.clone();
    let stt_client = client.clone();
    let stt_task = tokio::spawn(async move {
        loop {
            match stt_listener.accept().await {
                Ok((stream, _)) => {
                    let cfg = stt_config.clone();
                    let cl = stt_client.clone();
                    tokio::spawn(handle_stt(stream, cfg, cl));
                }
                Err(e) => error!("STT accept error: {e}"),
            }
        }
    });

    let tts_config = config.clone();
    let tts_client = client.clone();
    let tts_task = tokio::spawn(async move {
        loop {
            match tts_listener.accept().await {
                Ok((stream, _)) => {
                    let cfg = tts_config.clone();
                    let cl = tts_client.clone();
                    tokio::spawn(handle_tts(stream, cfg, cl));
                }
                Err(e) => error!("TTS accept error: {e}"),
            }
        }
    });

    tokio::select! {
        _ = stt_task => warn!("STT listener exited unexpectedly"),
        _ = tts_task => warn!("TTS listener exited unexpectedly"),
    }

    Ok(())
}
