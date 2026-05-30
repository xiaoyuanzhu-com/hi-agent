//! Volcengine BigModel STT — bidirectional streaming WebSocket
//! (大模型流式语音识别 / 双向流式 / `volc.bigasr.sauc.duration`).
//!
//! Wire protocol per <https://www.volcengine.com/docs/6561/1354869>:
//!
//!   wss://openspeech.bytedance.com/api/v3/sauc/bigmodel_async  (recommended)
//!   wss://openspeech.bytedance.com/api/v3/sauc/bigmodel         (basic)
//!
//! Resource IDs by model family:
//!   doubao 1.0:  volc.bigasr.sauc.duration  / volc.bigasr.sauc.concurrent
//!   doubao 2.0:  volc.seedasr.sauc.duration / volc.seedasr.sauc.concurrent
//!
//! Connection headers (new console):
//!   X-Api-Key:          <api_key>
//!   X-Api-Resource-Id:  volc.seedasr.sauc.duration
//!   X-Api-Request-Id:   <uuid per request>
//!   X-Api-Connect-Id:   <uuid per connection>
//!   X-Api-Sequence:     -1
//!
//! Every WS frame is binary and laid out as:
//!
//!   ┌────────┬────────┬────────┬────────┬──────────────────┬─────────────────┐
//!   │ b0     │ b1     │ b2     │ b3     │ payload_size u32 │ payload bytes…  │
//!   │ ver/hs │ mt/flg │ ser/cp │ resv   │ (big-endian)     │                 │
//!   └────────┴────────┴────────┴────────┴──────────────────┴─────────────────┘
//!
//!   b0  = (proto_version=1) << 4 | (header_size=1) = 0x11
//!   b1  = (msg_type) << 4 | flags
//!   b2  = (serialization) << 4 | compression
//!   b3  = reserved (0x00)
//!
//! Message types we use:
//!   0b0001 FULL_CLIENT_REQUEST  — initial JSON config, serialization=JSON(1)
//!   0b0010 AUDIO_ONLY_REQUEST   — raw PCM chunks, serialization=raw(0)
//!                                 flags=0b0010 marks the last chunk
//!   0b1001 FULL_SERVER_RESPONSE — incremental + final ASR result JSON
//!   0b1111 SERVER_ERROR         — error payload
//!
//! Server responses prepend a 4-byte header that we skip; on the result frames
//! the Python reference treats bytes 4..12 as reserved and parses JSON from
//! byte 12 onward. We mirror that — bytes [4..8] are an int32 we don't care
//! about, and on result frames bytes [8..12] are a payload-size we ignore in
//! favor of the remainder of the frame.
//!
//! Body bytes posted to `/audio` are expected to be WAV (the SPA encodes WAV
//! before posting). We strip the 44-byte RIFF/WAV header to recover raw PCM
//! before streaming. Anything else is sent as-is and will likely be rejected
//! by the upstream — explicit failure is better than silent garbage.

use std::time::Duration;

use async_trait::async_trait;
use bytes::Bytes;
use futures::{SinkExt, StreamExt};
use serde::{Deserialize, Serialize};
use serde_json::json;
use tokio::net::TcpStream;
use tokio::sync::mpsc;
use tokio::time::timeout;
use tokio_tungstenite::tungstenite::client::IntoClientRequest;
use tokio_tungstenite::tungstenite::http::HeaderValue;
use tokio_tungstenite::tungstenite::Message;
use tokio_tungstenite::{MaybeTlsStream, WebSocketStream};
use uuid::Uuid;

use super::stt::{Stt, Transcript};

// Defaults target the recommended async (optimized) endpoint + doubao 2.0
// hour version. Override the resource id to point at concurrent variants or
// doubao 1.0.
const DEFAULT_ENDPOINT: &str = "wss://openspeech.bytedance.com/api/v3/sauc/bigmodel_async";
const DEFAULT_RESOURCE_ID: &str = "volc.seedasr.sauc.duration";
const SUCCESS_STATUS: i64 = 20_000_000;

const ENV_API_KEY: &str = "VOLCENGINE_STT_API_KEY";
const ENV_MODEL: &str = "VOLCENGINE_STT_MODEL";
const ENV_RESOURCE_ID: &str = "VOLCENGINE_STT_RESOURCE_ID";
const ENV_ENDPOINT: &str = "VOLCENGINE_STT_ENDPOINT";

const DEFAULT_MODEL: &str = "bigmodel";

// One PCM chunk per WS frame. 3200 bytes = 100 ms of 16 kHz mono 16-bit.
const PCM_CHUNK_BYTES: usize = 3200;
// Generous overall budget. Real recognitions complete in seconds.
const TOTAL_TIMEOUT: Duration = Duration::from_secs(30);
// Streaming spans the whole utterance (up to the 30 s mic cap) plus the trailing
// finalization, so it gets a longer ceiling than the batch path.
const STREAM_TIMEOUT: Duration = Duration::from_secs(90);

const WAV_HEADER_BYTES: usize = 44;

const PROTO_HEADER_BYTE0: u8 = 0x11; // proto v1, header size 1 (= 4 bytes)
const MSG_TYPE_FULL_CLIENT: u8 = 0b0001;
const MSG_TYPE_AUDIO_ONLY: u8 = 0b0010;
const MSG_TYPE_FULL_SERVER: u8 = 0b1001;
const MSG_TYPE_SERVER_ERROR: u8 = 0b1111;
const FLAG_LAST_CHUNK: u8 = 0b0010;
const SER_JSON: u8 = 0b0001;
const SER_RAW: u8 = 0b0000;

pub struct VolcengineStt {
    api_key: String,
    model: String,
    resource_id: String,
    endpoint: String,
}

impl VolcengineStt {
    pub fn from_env() -> anyhow::Result<Self> {
        let api_key = std::env::var(ENV_API_KEY).map_err(|_| {
            anyhow::anyhow!("{ENV_API_KEY} is required when STT_PROVIDER=volcengine")
        })?;
        let model = std::env::var(ENV_MODEL).unwrap_or_else(|_| DEFAULT_MODEL.to_string());
        let resource_id =
            std::env::var(ENV_RESOURCE_ID).unwrap_or_else(|_| DEFAULT_RESOURCE_ID.to_string());
        let endpoint =
            std::env::var(ENV_ENDPOINT).unwrap_or_else(|_| DEFAULT_ENDPOINT.to_string());
        Ok(Self {
            api_key,
            model,
            resource_id,
            endpoint,
        })
    }
}

#[async_trait]
impl Stt for VolcengineStt {
    async fn transcribe(&self, audio: Bytes, mime: &str) -> anyhow::Result<String> {
        timeout(TOTAL_TIMEOUT, self.transcribe_inner(audio, mime))
            .await
            .map_err(|_| anyhow::anyhow!("volcengine STT timed out after {TOTAL_TIMEOUT:?}"))?
    }

    async fn transcribe_streaming(
        &self,
        audio_rx: mpsc::Receiver<Bytes>,
        out: mpsc::Sender<Transcript>,
    ) -> anyhow::Result<String> {
        timeout(STREAM_TIMEOUT, self.transcribe_streaming_inner(audio_rx, out))
            .await
            .map_err(|_| anyhow::anyhow!("volcengine STT stream timed out after {STREAM_TIMEOUT:?}"))?
    }
}

impl VolcengineStt {
    /// Open the upstream WS with the custom auth headers shared by both the
    /// batch and streaming paths.
    async fn connect(&self) -> anyhow::Result<WebSocketStream<MaybeTlsStream<TcpStream>>> {
        let connect_id = Uuid::now_v7().to_string();
        let request_id = Uuid::now_v7().to_string();
        let mut request = self.endpoint.as_str().into_client_request()?;
        let headers = request.headers_mut();
        headers.insert("X-Api-Key", HeaderValue::from_str(&self.api_key)?);
        headers.insert("X-Api-Resource-Id", HeaderValue::from_str(&self.resource_id)?);
        headers.insert("X-Api-Request-Id", HeaderValue::from_str(&request_id)?);
        headers.insert("X-Api-Connect-Id", HeaderValue::from_str(&connect_id)?);
        headers.insert("X-Api-Sequence", HeaderValue::from_static("-1"));

        let (ws, response) = tokio_tungstenite::connect_async(request)
            .await
            .map_err(|e| anyhow::anyhow!("volcengine STT WS connect failed: {e}"))?;
        tracing::debug!(
            status = %response.status(),
            connect_id = %connect_id,
            "volcengine STT WS connected"
        );
        Ok(ws)
    }

    /// Build the FULL_CLIENT_REQUEST config frame. `show_utterances` exposes the
    /// per-utterance `definite` flag the streaming path uses to tell the fast
    /// preliminary text from the polished final.
    fn config_frame(&self, show_utterances: bool) -> anyhow::Result<Vec<u8>> {
        let config = json!({
            "user": { "uid": "hi-agent" },
            "audio": {
                "format": "pcm",
                "codec": "raw",
                "rate": 16000,
                "bits": 16,
                "channel": 1,
            },
            "request": {
                "model_name": self.model,
                "enable_itn": true,
                "enable_punc": true,
                "result_type": "single",
                "show_utterances": show_utterances,
            },
        });
        Ok(frame(MSG_TYPE_FULL_CLIENT, 0, SER_JSON, &serde_json::to_vec(&config)?))
    }

    async fn transcribe_inner(&self, audio: Bytes, mime: &str) -> anyhow::Result<String> {
        let pcm = extract_pcm(&audio, mime)?;
        let (mut tx, mut rx) = self.connect().await?.split();

        // 1. FULL_CLIENT_REQUEST — JSON config.
        tx.send(Message::Binary(self.config_frame(false)?)).await?;

        // 2. Audio chunks.
        let mut offset = 0;
        while offset < pcm.len() {
            let end = (offset + PCM_CHUNK_BYTES).min(pcm.len());
            let is_last = end == pcm.len();
            let flags = if is_last { FLAG_LAST_CHUNK } else { 0 };
            tx.send(Message::Binary(frame(
                MSG_TYPE_AUDIO_ONLY,
                flags,
                SER_RAW,
                &pcm[offset..end],
            )))
            .await?;
            offset = end;
        }
        // The reference impl also sends a zero-byte final marker when audio
        // ends — done as part of the last chunk above if it had data, but if
        // pcm was empty we still need to terminate.
        if pcm.is_empty() {
            tx.send(Message::Binary(frame(
                MSG_TYPE_AUDIO_ONLY,
                FLAG_LAST_CHUNK,
                SER_RAW,
                &[],
            )))
            .await?;
        }

        // 3. Drain responses until we see a `final` result or an error.
        let mut final_text = String::new();
        let mut last_text = String::new();
        while let Some(msg) = rx.next().await {
            let msg = msg.map_err(|e| anyhow::anyhow!("volcengine STT WS recv: {e}"))?;
            let bytes = match msg {
                Message::Binary(b) => b,
                Message::Close(frame) => {
                    if !last_text.is_empty() && final_text.is_empty() {
                        final_text = last_text.clone();
                    }
                    if final_text.is_empty() {
                        anyhow::bail!(
                            "volcengine STT closed without final result: {:?}",
                            frame
                        );
                    }
                    break;
                }
                _ => continue,
            };

            if bytes.len() < 4 {
                continue;
            }
            let msg_type = (bytes[1] >> 4) & 0x0F;
            // Skip the 4-byte protocol header. The reference treats bytes
            // 4..12 as opaque (size/seq fields we don't need) and reads JSON
            // from byte 12 — but on some result frames the payload starts at
            // byte 8. Find the first '{' to be tolerant of either.
            let tail = &bytes[4..];

            match msg_type {
                MSG_TYPE_FULL_SERVER => {
                    let Some(payload) = extract_json(tail) else {
                        continue;
                    };
                    let parsed: ServerResponse = match serde_json::from_slice(payload) {
                        Ok(p) => p,
                        Err(err) => {
                            tracing::warn!(error = %err, "volcengine STT JSON parse failed");
                            continue;
                        }
                    };

                    if let Some(status) = parsed.header.as_ref().and_then(|h| h.status) {
                        if status != SUCCESS_STATUS && status != 0 {
                            anyhow::bail!(
                                "volcengine STT error status={status} message={:?}",
                                parsed.header.as_ref().and_then(|h| h.message.clone())
                            );
                        }
                    }

                    if let Some(text) = parsed.result.as_ref().and_then(|r| r.first_text()) {
                        if !text.is_empty() {
                            last_text = text;
                        }
                    }

                    if parsed.kind.as_deref() == Some("final") {
                        final_text = last_text.clone();
                        break;
                    }
                }
                MSG_TYPE_SERVER_ERROR => {
                    let payload = extract_json(tail).unwrap_or(tail);
                    let msg = String::from_utf8_lossy(payload).into_owned();
                    anyhow::bail!("volcengine STT server error: {msg}");
                }
                _ => continue,
            }
        }

        let _ = tx.send(Message::Close(None)).await;

        let text = final_text.trim().to_owned();
        if text.is_empty() {
            anyhow::bail!("volcengine STT returned empty transcript");
        }
        Ok(text)
    }

    /// Full-duplex streaming: a sender task forwards incoming PCM as it arrives
    /// while this task reads incremental results. `audio_rx` carries raw 16 kHz
    /// mono 16-bit PCM (no WAV header — the caller already stripped/never added
    /// one). Each non-empty result is emitted on `out`; the polished final is
    /// returned.
    async fn transcribe_streaming_inner(
        &self,
        mut audio_rx: mpsc::Receiver<Bytes>,
        out: mpsc::Sender<Transcript>,
    ) -> anyhow::Result<String> {
        let (mut tx, mut rx) = self.connect().await?.split();
        tx.send(Message::Binary(self.config_frame(true)?)).await?;

        // Sender task: stream PCM chunks until the input closes, then mark the
        // last chunk so the upstream finalizes.
        let send_task = tokio::spawn(async move {
            while let Some(chunk) = audio_rx.recv().await {
                let mut offset = 0;
                while offset < chunk.len() {
                    let end = (offset + PCM_CHUNK_BYTES).min(chunk.len());
                    tx.send(Message::Binary(frame(
                        MSG_TYPE_AUDIO_ONLY,
                        0,
                        SER_RAW,
                        &chunk[offset..end],
                    )))
                    .await?;
                    offset = end;
                }
            }
            tx.send(Message::Binary(frame(MSG_TYPE_AUDIO_ONLY, FLAG_LAST_CHUNK, SER_RAW, &[])))
                .await?;
            anyhow::Ok(())
        });

        // Reader: emit each new result; the upstream sends rolling preliminaries
        // (definite=false) then a definite/final result per utterance.
        let mut last_emitted = String::new();
        let mut final_text = String::new();
        while let Some(msg) = rx.next().await {
            let msg = msg.map_err(|e| anyhow::anyhow!("volcengine STT WS recv: {e}"))?;
            let bytes = match msg {
                Message::Binary(b) => b,
                Message::Close(_) => break,
                _ => continue,
            };
            if bytes.len() < 4 {
                continue;
            }
            let msg_type = (bytes[1] >> 4) & 0x0F;
            let tail = &bytes[4..];
            match msg_type {
                MSG_TYPE_FULL_SERVER => {
                    let Some(payload) = extract_json(tail) else { continue };
                    let parsed: ServerResponse = match serde_json::from_slice(payload) {
                        Ok(p) => p,
                        Err(err) => {
                            tracing::warn!(error = %err, "volcengine STT JSON parse failed");
                            continue;
                        }
                    };
                    if let Some(status) = parsed.header.as_ref().and_then(|h| h.status) {
                        if status != SUCCESS_STATUS && status != 0 {
                            anyhow::bail!(
                                "volcengine STT error status={status} message={:?}",
                                parsed.header.as_ref().and_then(|h| h.message.clone())
                            );
                        }
                    }
                    let is_final =
                        parsed.kind.as_deref() == Some("final") || parsed.is_final == Some(true);
                    if let Some(text) = parsed.result.as_ref().and_then(|r| r.first_text()) {
                        if !text.is_empty() {
                            if is_final {
                                final_text = text.clone();
                            }
                            // Dedupe identical rolling updates; always pass a final.
                            if is_final || text != last_emitted {
                                last_emitted = text.clone();
                                let _ = out.send(Transcript { text, is_final }).await;
                            }
                        }
                    }
                    if is_final {
                        break;
                    }
                }
                MSG_TYPE_SERVER_ERROR => {
                    let payload = extract_json(tail).unwrap_or(tail);
                    anyhow::bail!(
                        "volcengine STT server error: {}",
                        String::from_utf8_lossy(payload)
                    );
                }
                _ => continue,
            }
        }

        send_task.abort();

        let text = if final_text.trim().is_empty() {
            last_emitted.trim().to_owned()
        } else {
            final_text.trim().to_owned()
        };
        // A final emit may not have been flagged (e.g. stream closed early); make
        // sure the caller's last view matches what we return.
        if !text.is_empty() && text != last_emitted {
            let _ = out.send(Transcript { text: text.clone(), is_final: true }).await;
        }
        Ok(text)
    }
}

/// Build a single protocol frame: 4-byte header + uint32 BE payload size + payload bytes.
fn frame(msg_type: u8, flags: u8, serialization: u8, payload: &[u8]) -> Vec<u8> {
    let mut out = Vec::with_capacity(8 + payload.len());
    out.push(PROTO_HEADER_BYTE0);
    out.push((msg_type << 4) | (flags & 0x0F));
    out.push((serialization << 4) | 0);
    out.push(0);
    out.extend_from_slice(&(payload.len() as u32).to_be_bytes());
    out.extend_from_slice(payload);
    out
}

/// Strip a 44-byte RIFF/WAVE header to recover raw PCM. We don't fully parse
/// the WAV — Volcengine only accepts 16 kHz mono 16-bit PCM, which is exactly
/// what the SPA recorder emits, so the header is always 44 bytes.
fn extract_pcm(audio: &Bytes, mime: &str) -> anyhow::Result<Vec<u8>> {
    let mime_lower = mime.split(';').next().unwrap_or("").trim().to_ascii_lowercase();
    if mime_lower.starts_with("audio/wav") || mime_lower == "audio/wave" || mime_lower == "audio/x-wav" {
        if audio.len() < WAV_HEADER_BYTES + 2 || &audio[0..4] != b"RIFF" {
            anyhow::bail!("audio body is not a valid WAV");
        }
        Ok(audio[WAV_HEADER_BYTES..].to_vec())
    } else if mime_lower == "audio/pcm" || mime_lower == "application/octet-stream" {
        Ok(audio.to_vec())
    } else {
        anyhow::bail!(
            "unsupported audio mime {mime_lower:?} — provide audio/wav (16 kHz mono 16-bit) or audio/pcm"
        )
    }
}

/// Find the first JSON object in a byte slice. The server prepends some
/// reserved/header bytes before the JSON body on some frames; the Python
/// reference scans for the first `{`. We do the same.
fn extract_json(bytes: &[u8]) -> Option<&[u8]> {
    let start = bytes.iter().position(|&b| b == b'{')?;
    Some(&bytes[start..])
}

#[derive(Debug, Default, Deserialize, Serialize)]
struct ServerResponse {
    /// Volcengine sends this as the discriminator (`"final"`, `"partial"`, etc).
    #[serde(rename = "type", default)]
    kind: Option<String>,
    /// Some streaming responses carry an explicit finality flag instead of/in
    /// addition to `type`.
    #[serde(default)]
    is_final: Option<bool>,
    #[serde(default)]
    header: Option<ServerHeader>,
    #[serde(default)]
    result: Option<ServerResult>,
}

#[derive(Debug, Default, Deserialize, Serialize)]
struct ServerHeader {
    #[serde(default)]
    status: Option<i64>,
    #[serde(default)]
    message: Option<String>,
}

/// `result` is either an object with `text`, an array of such, or a string.
/// We accept all three forms and return the longest text we can find.
#[derive(Debug, Deserialize, Serialize)]
#[serde(untagged)]
enum ServerResult {
    Object { text: Option<String> },
    List(Vec<ResultItem>),
    Text(String),
}

#[derive(Debug, Deserialize, Serialize)]
struct ResultItem {
    #[serde(default)]
    text: Option<String>,
}

impl ServerResult {
    fn first_text(&self) -> Option<String> {
        match self {
            ServerResult::Object { text } => text.clone().map(|s| s.trim().to_owned()),
            ServerResult::List(items) => items
                .iter()
                .filter_map(|i| i.text.as_deref())
                .map(|s| s.trim().to_owned())
                .filter(|s| !s.is_empty())
                .max_by_key(|s| s.len()),
            ServerResult::Text(s) => Some(s.trim().to_owned()),
        }
    }
}
