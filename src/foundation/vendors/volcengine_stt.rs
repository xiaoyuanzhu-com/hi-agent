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

use crate::body::capabilities::stt::{DiarizedSpan, Transcript};

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
const ENV_SPEAKER_INFO: &str = "VOLCENGINE_STT_SPEAKER_INFO";

const DEFAULT_MODEL: &str = "bigmodel";

// One PCM chunk per WS frame. 3200 bytes = 100 ms of 16 kHz mono 16-bit.
const PCM_CHUNK_BYTES: usize = 3200;
// Generous overall budget. Real recognitions complete in seconds.
const TOTAL_TIMEOUT: Duration = Duration::from_secs(30);
// Trailing silence (ms) after which the upstream's server-side VAD finalizes an
// utterance (`definite=true`) mid-stream. Empirically this is the lever that
// makes continuous segmentation work; without it the upstream defers all
// finalization to connection close. ~800 ms balances snappy turn handoff
// against clipping a speaker who pauses mid-thought.
const STREAM_END_WINDOW_MS: u32 = 800;

const WAV_HEADER_BYTES: usize = 44;

const PROTO_HEADER_BYTE0: u8 = 0x11; // proto v1, header size 1 (= 4 bytes)
const MSG_TYPE_FULL_CLIENT: u8 = 0b0001;
const MSG_TYPE_AUDIO_ONLY: u8 = 0b0010;
const MSG_TYPE_FULL_SERVER: u8 = 0b1001;
const MSG_TYPE_SERVER_ERROR: u8 = 0b1111;
const FLAG_LAST_CHUNK: u8 = 0b0010;
const SER_JSON: u8 = 0b0001;
const SER_RAW: u8 = 0b0000;

pub struct Config {
    api_key: String,
    model: String,
    resource_id: String,
    endpoint: String,
    speaker_info: bool,
}

impl Config {
    pub fn from_env() -> anyhow::Result<Self> {
        Self::from_env_with_key(None)
    }

    /// Like [`from_env`](Self::from_env) but the API key comes from `key_override`
    /// when non-empty (the BYOK store), falling back to `VOLCENGINE_STT_API_KEY`.
    /// All other params stay env-driven.
    pub fn from_env_with_key(key_override: Option<&str>) -> anyhow::Result<Self> {
        let api_key = match key_override {
            Some(k) if !k.trim().is_empty() => k.trim().to_string(),
            _ => std::env::var(ENV_API_KEY).map_err(|_| {
                anyhow::anyhow!("{ENV_API_KEY} is required when STT_PROVIDER=volcengine")
            })?,
        };
        let model = std::env::var(ENV_MODEL).unwrap_or_else(|_| DEFAULT_MODEL.to_string());
        let resource_id =
            std::env::var(ENV_RESOURCE_ID).unwrap_or_else(|_| DEFAULT_RESOURCE_ID.to_string());
        let endpoint =
            std::env::var(ENV_ENDPOINT).unwrap_or_else(|_| DEFAULT_ENDPOINT.to_string());
        // Speaker clustering is on by default; set the env to 0/false/off to disable.
        let speaker_info = match std::env::var(ENV_SPEAKER_INFO) {
            Ok(v) => {
                let v = v.trim();
                !(v == "0" || v.eq_ignore_ascii_case("false") || v.eq_ignore_ascii_case("off"))
            }
            Err(_) => true,
        };
        Ok(Self {
            api_key,
            model,
            resource_id,
            endpoint,
            speaker_info,
        })
    }
}

pub async fn transcribe(cfg: &Config, audio: Bytes, mime: &str) -> anyhow::Result<String> {
    timeout(TOTAL_TIMEOUT, transcribe_inner(cfg, audio, mime))
        .await
        .map_err(|_| anyhow::anyhow!("volcengine STT timed out after {TOTAL_TIMEOUT:?}"))?
}

/// Open the upstream WS with the custom auth headers shared by both the
/// batch and streaming paths.
async fn connect(cfg: &Config) -> anyhow::Result<WebSocketStream<MaybeTlsStream<TcpStream>>> {
    let connect_id = Uuid::now_v7().to_string();
    let request_id = Uuid::now_v7().to_string();
    let mut request = cfg.endpoint.as_str().into_client_request()?;
    let headers = request.headers_mut();
    headers.insert("X-Api-Key", HeaderValue::from_str(&cfg.api_key)?);
    headers.insert("X-Api-Resource-Id", HeaderValue::from_str(&cfg.resource_id)?);
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
/// per-utterance `definite` flag the streaming path uses for endpointing;
/// when set we also request server-side VAD segmentation via
/// `end_window_size` so utterances finalize mid-stream (after a short pause)
/// rather than only at connection close.
///
/// `speaker` requests speaker clustering. Both the batch and streaming paths opt
/// in (gated on `cfg.speaker_info`): it forces `enable_nonstream` (see below), but
/// that is **two-pass** (二遍识别), not buffer-only — the streaming pass still
/// returns low-latency 逐字 partials for barge-in while a non-streaming second pass
/// re-recognizes each VAD-cut segment and labels it. So diarization does not cost
/// the live stream its partials.
fn config_frame(cfg: &Config, show_utterances: bool, speaker: bool) -> anyhow::Result<Vec<u8>> {
    let mut request = json!({
        "model_name": cfg.model,
        "enable_itn": true,
        "enable_punc": true,
        "result_type": "single",
        "show_utterances": show_utterances,
    });
    if show_utterances {
        request["end_window_size"] = json!(STREAM_END_WINDOW_MS);
    }
    if speaker {
        // Speaker clustering/diarization. On the optimized async endpoint it's
        // gated on enable_nonstream=true (two-pass: real-time partials PLUS a
        // non-streaming re-recognition of each segment) and ssd_version=200 (the
        // ASR 2.0 model). All three must be set together or the upstream ignores
        // the speaker request. The labels then ride the post-processed `definite`
        // utterances as `additions.speaker_id`.
        request["enable_speaker_info"] = json!(true);
        request["enable_nonstream"] = json!(true);
        request["ssd_version"] = json!("200");
    }
    let config = json!({
        "user": { "uid": "hi-agent" },
        "audio": {
            "format": "pcm",
            "codec": "raw",
            "rate": 16000,
            "bits": 16,
            "channel": 1,
        },
        "request": request,
    });
    Ok(frame(MSG_TYPE_FULL_CLIENT, 0, SER_JSON, &serde_json::to_vec(&config)?))
}

async fn transcribe_inner(cfg: &Config, audio: Bytes, mime: &str) -> anyhow::Result<String> {
    let pcm = extract_pcm(&audio, mime)?;
    let (mut tx, mut rx) = connect(cfg).await?.split();

    // 1. FULL_CLIENT_REQUEST — JSON config. Request utterances + speaker
    //    clustering when configured so a multi-speaker clip comes back labeled.
    tx.send(Message::Binary(config_frame(cfg, cfg.speaker_info, cfg.speaker_info)?)).await?;

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
    // When speaker clustering labels ≥2 voices, the post-processed final frame
    // carries a multi-speaker transcript we prefer over the flat `text`.
    let mut last_labeled: Option<String> = None;
    while let Some(msg) = rx.next().await {
        let msg = msg.map_err(|e| anyhow::anyhow!("volcengine STT WS recv: {e}"))?;
        let bytes = match msg {
            Message::Binary(b) => b,
            Message::Close(_) => {
                // Normal end-of-stream. Promote the last rolling preliminary
                // if no frame was flagged `final`. An empty result here is a
                // benign "no speech recognized" — the upstream cannot tell
                // silence from un-transcribable sound, and neither path is an
                // error; the caller treats an empty transcript as a no-op.
                if final_text.is_empty() {
                    final_text = last_labeled.clone().unwrap_or_else(|| last_text.clone());
                }
                break;
            }
            _ => continue,
        };

        if bytes.len() < 4 {
            continue;
        }
        let msg_type = (bytes[1] >> 4) & 0x0F;
        let tail = &bytes[4..];

        match msg_type {
            MSG_TYPE_FULL_SERVER => {
                let Some(payload) = server_payload(&bytes) else {
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

                if let Some(result) = parsed.result.as_ref() {
                    if let Some(text) = result.first_text() {
                        if !text.is_empty() {
                            last_text = text;
                        }
                    }
                    if let Some(labeled) = result.labeled_transcript() {
                        last_labeled = Some(labeled);
                    }
                }

                if parsed.kind.as_deref() == Some("final") {
                    final_text = last_labeled.clone().unwrap_or_else(|| last_text.clone());
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

    // Empty is a valid result (no speech recognized), distinct from the
    // errors above which `bail!` — those still propagate as failures.
    Ok(final_text.trim().to_owned())
}

/// Full-duplex streaming: a sender task forwards incoming PCM as it arrives
/// while this task reads incremental results. `audio_rx` carries raw 16 kHz
/// mono 16-bit PCM (no WAV header — the caller already stripped/never added
/// one). Each non-empty result is emitted on `out`; the polished final is
/// returned. Always-on for the whole mic-open period: there is no time limit —
/// the session ends when the caller drops `audio_rx` (the client closed the
/// audio input) or the upstream errors.
pub async fn transcribe_streaming(
    cfg: &Config,
    mut audio_rx: mpsc::Receiver<Bytes>,
    out: mpsc::Sender<Transcript>,
) -> anyhow::Result<String> {
    let (mut tx, mut rx) = connect(cfg).await?.split();
    // Two-pass (`enable_nonstream`) when speaker info is on: the streaming pass
    // keeps returning low-latency 逐字 partials (the barge-in trigger), while a
    // non-streaming second pass re-recognizes each VAD-cut segment and tags it
    // with `speaker_id`. So diarization here does NOT cost the live partials.
    tx.send(Message::Binary(config_frame(cfg, true, cfg.speaker_info)?)).await?;

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

    // Reader: a continuous session over many utterances. The upstream sends
    // rolling preliminaries (definite=false) then marks an utterance
    // `definite=true` once its server-side VAD detects the endpoint. We emit
    // every non-empty preliminary as a partial (the caller uses it for
    // barge-in) and each finalized utterance as `is_final=true` (the caller
    // dispatches those as discrete signals). The loop runs until the audio
    // input closes and the upstream closes the socket — it does NOT stop at
    // the first final.
    let mut last_partial = String::new();
    let mut last_final = String::new();
    let mut last_returned = String::new();
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
                let Some(payload) = server_payload(&bytes) else { continue };
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

                if parsed.is_definite() {
                    // Utterance finalized. Dispatch its text once; a new
                    // partial resets `last_final` so a later utterance with
                    // identical text still dispatches.
                    let text = parsed
                        .result
                        .as_ref()
                        .and_then(|r| r.definite_text())
                        .unwrap_or_default();
                    if !text.is_empty() && text != last_final {
                        last_final = text.clone();
                        last_returned = text.clone();
                        last_partial.clear();
                        // Carry the segment's speaker label (two-pass diarization)
                        // so the caller can voiceprint that speaker's audio, plus
                        // every diarized span finalized in this frame so the caller
                        // slices each speaker's own audio (not the last one only).
                        let speaker_id = parsed.result.as_ref().and_then(|r| r.definite_speaker_id());
                        let segments = parsed.result.as_ref().map(|r| r.definite_spans()).unwrap_or_default();
                        let _ = out.send(Transcript { text, is_final: true, speaker_id, segments }).await;
                    }
                } else if let Some(text) = parsed.text() {
                    // Rolling preliminary. Dedupe identical updates.
                    if !text.is_empty() && text != last_partial {
                        last_partial = text.clone();
                        last_final.clear();
                        let _ = out.send(Transcript { text, is_final: false, speaker_id: None, segments: Vec::new() }).await;
                    }
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
    // The return value is the last finalized utterance — a best-effort
    // summary for callers that await the whole session; the live signal is
    // the `out` stream above.
    Ok(last_returned.trim().to_owned())
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

/// Extract the JSON payload of a server response frame by honoring the wire
/// layout — *not* by scanning for a `{`. A naive brace-scan corrupts any frame
/// whose 4-byte payload-size field ends in `0x7b`: a 123-byte payload writes a
/// literal `{` byte into the size field that the scan locks onto before the
/// real JSON (observed live as `key must be a string at line 1 column 2`).
///
/// Layout, where `flags` is the low nibble of byte 1:
///
///   ┌──────────────┬───────────────────────────┬──────────────┬──────────┐
///   │ 4-byte header│ 4-byte sequence (iff 0x1) │ 4-byte size  │ payload… │
///   └──────────────┴───────────────────────────┴──────────────┴──────────┘
///
/// Returns None on a frame too short to hold the declared payload.
fn server_payload(bytes: &[u8]) -> Option<&[u8]> {
    let flags = bytes.get(1).map(|b| b & 0x0F)?;
    let mut off = 4;
    if flags & 0x01 != 0 {
        off += 4; // sequence number present
    }
    let size = bytes
        .get(off..off + 4)
        .map(|s| u32::from_be_bytes([s[0], s[1], s[2], s[3]]) as usize)?;
    off += 4;
    let end = off.checked_add(size)?.min(bytes.len());
    bytes.get(off..end)
}

/// Best-effort scan for the first JSON object in a byte slice. Used only to
/// stringify SERVER_ERROR frames for logging, where the exact framing differs
/// (an `[code][size]` prefix) and a lossy result is acceptable.
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

impl ServerResponse {
    /// Top-level recognized text for the current segment (empty if absent).
    fn text(&self) -> Option<String> {
        self.result.as_ref().and_then(|r| r.first_text())
    }

    /// True once the upstream's server-side VAD marks the current utterance as
    /// finalized — the endpoint signal we use instead of client-side VAD. The
    /// `type=="final"`/`is_final` flags only fire at stream close, so the
    /// per-utterance `definite` flag is what drives continuous segmentation.
    fn is_definite(&self) -> bool {
        self.kind.as_deref() == Some("final")
            || self.is_final == Some(true)
            || self
                .result
                .as_ref()
                .map(|r| r.has_definite_utterance())
                .unwrap_or(false)
    }
}

#[derive(Debug, Default, Deserialize, Serialize)]
struct ServerHeader {
    #[serde(default)]
    status: Option<i64>,
    #[serde(default)]
    message: Option<String>,
}

/// The `result` object: a top-level `text` plus, when `show_utterances` is on,
/// a per-utterance breakdown carrying the `definite` endpoint flag.
#[derive(Debug, Default, Deserialize, Serialize)]
struct ServerResult {
    #[serde(default)]
    text: Option<String>,
    #[serde(default)]
    utterances: Vec<Utterance>,
}

#[derive(Debug, Default, Deserialize, Serialize)]
struct Utterance {
    #[serde(default)]
    text: Option<String>,
    /// Set by the upstream's server-side VAD once this utterance is finalized
    /// and will not change.
    #[serde(default)]
    definite: bool,
    /// Start/end of this utterance in milliseconds from the stream's first audio
    /// sample (present when `show_utterances` is on). The voiceprint path slices
    /// the diarized speaker's own audio by this span — see [`ServerResult::definite_spans`].
    #[serde(default)]
    start_time: Option<u64>,
    #[serde(default)]
    end_time: Option<u64>,
    #[serde(default)]
    additions: UtteranceAdditions,
}

/// Per-utterance extras. We only read `speaker_id` (present on post-processed
/// `two_pass` utterances when speaker clustering is on); the rest is ignored.
#[derive(Debug, Default, Deserialize, Serialize)]
struct UtteranceAdditions {
    #[serde(default)]
    speaker_id: Option<String>,
}

impl ServerResult {
    fn first_text(&self) -> Option<String> {
        self.text
            .as_deref()
            .map(|s| s.trim().to_owned())
            .filter(|s| !s.is_empty())
    }

    fn has_definite_utterance(&self) -> bool {
        self.utterances.iter().any(|u| u.definite)
    }

    /// Text of the finalized (definite) utterance, preferred over the top-level
    /// `text` when dispatching a completed utterance.
    fn definite_text(&self) -> Option<String> {
        self.utterances
            .iter()
            .filter(|u| u.definite)
            .filter_map(|u| u.text.as_deref())
            .map(|s| s.trim().to_owned())
            .filter(|s| !s.is_empty())
            .next_back()
            .or_else(|| self.first_text())
    }

    /// Speaker label of the finalized (definite) utterance — aligned with
    /// [`Self::definite_text`] (both take the last definite utterance), so the
    /// dispatched final and its speaker id come from the same segment. `None` when
    /// diarization is off or the segment carries no `speaker_id`.
    fn definite_speaker_id(&self) -> Option<String> {
        self.utterances
            .iter()
            .filter(|u| u.definite)
            .filter_map(|u| u.additions.speaker_id.as_deref())
            .map(str::trim)
            .filter(|s| !s.is_empty())
            .map(str::to_owned)
            .next_back()
    }

    /// Every finalized (definite) utterance that carries a speaker label and a
    /// usable time span, as [`DiarizedSpan`]s in utterance order. Unlike
    /// [`Self::definite_speaker_id`] (which takes only the last definite utterance,
    /// to tag the dispatched sentence), this returns *all* of them — the voiceprint
    /// path slices each speaker's own audio by its `[start_ms, end_ms]` so a frame
    /// that finalized two speakers enrolls each voice cleanly rather than blending
    /// them. Utterances missing a speaker id, missing timing, or with a non-positive
    /// span are dropped.
    fn definite_spans(&self) -> Vec<DiarizedSpan> {
        self.utterances
            .iter()
            .filter(|u| u.definite)
            .filter_map(|u| {
                let speaker_id = u.additions.speaker_id.as_deref().map(str::trim).filter(|s| !s.is_empty())?;
                let (start_ms, end_ms) = (u.start_time?, u.end_time?);
                (end_ms > start_ms).then(|| DiarizedSpan { speaker_id: speaker_id.to_owned(), start_ms, end_ms })
            })
            .collect()
    }

    /// Render the definite utterances as a speaker-labeled transcript, but only
    /// when speaker clustering actually distinguished ≥2 voices. Each line is
    /// `说话人{id}：{text}`, in utterance order. Returns `None` for a single
    /// speaker (or no speaker info) so the caller keeps the flat transcript —
    /// a 1:1 conversation reads exactly as before.
    fn labeled_transcript(&self) -> Option<String> {
        let labeled: Vec<(&str, String)> = self
            .utterances
            .iter()
            .filter(|u| u.definite)
            .filter_map(|u| {
                let id = u.additions.speaker_id.as_deref()?;
                let text = u.text.as_deref()?.trim();
                (!text.is_empty()).then(|| (id, text.to_owned()))
            })
            .collect();
        let distinct = labeled.iter().map(|(id, _)| *id).collect::<std::collections::HashSet<_>>();
        if distinct.len() < 2 {
            return None;
        }
        let transcript = labeled
            .iter()
            .map(|(id, text)| format!("说话人{id}：{text}"))
            .collect::<Vec<_>>()
            .join("\n");
        Some(transcript)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Build a JSON result frame the way the upstream does: header, optional
    /// sequence, big-endian payload size, payload.
    fn result_frame(with_seq: bool, json: &[u8]) -> Vec<u8> {
        let flags = if with_seq { 0b0001 } else { 0 };
        let mut f = vec![0x11, (MSG_TYPE_FULL_SERVER << 4) | flags, 0x10, 0x00];
        if with_seq {
            f.extend_from_slice(&1u32.to_be_bytes());
        }
        f.extend_from_slice(&(json.len() as u32).to_be_bytes());
        f.extend_from_slice(json);
        f
    }

    // Regression: a 123-byte payload puts a literal `{` (0x7b) in the size
    // field. The old brace-scan locked onto it and produced
    // `key must be a string at line 1 column 2`, silently dropping the frame.
    #[test]
    fn payload_size_0x7b_does_not_corrupt_parse() {
        // Pad a real result shape out to exactly 123 bytes.
        let mut json = r#"{"result":{"text":"你好。"},"audio_info":{"duration":981}"#
            .as_bytes()
            .to_vec();
        while json.len() < 122 {
            json.push(b' ');
        }
        json.push(b'}');
        assert_eq!(json.len(), 123, "fixture must hit the 0x7b size byte");

        let frame = result_frame(true, &json);
        // The size field's low byte really is `{`.
        assert!(frame.iter().any(|&b| b == 0x7b));

        let payload = server_payload(&frame).expect("payload extracted");
        let parsed: ServerResponse = serde_json::from_slice(payload).expect("valid JSON");
        assert_eq!(
            parsed.result.as_ref().and_then(|r| r.first_text()).as_deref(),
            Some("你好。")
        );
    }

    #[test]
    fn handles_frame_with_and_without_sequence() {
        let json = br#"{"result":{"text":"hi"}}"#;
        for with_seq in [false, true] {
            let frame = result_frame(with_seq, json);
            let payload = server_payload(&frame).expect("payload");
            assert_eq!(payload, json, "with_seq={with_seq}");
        }
    }

    #[test]
    fn short_or_malformed_frame_returns_none() {
        assert!(server_payload(&[0x11, 0x90]).is_none());
        // Declares a 999-byte payload it doesn't carry: clamped, not panicking.
        let mut frame = vec![0x11, 0x90, 0x10, 0x00];
        frame.extend_from_slice(&999u32.to_be_bytes());
        frame.extend_from_slice(b"{}");
        assert_eq!(server_payload(&frame), Some(&b"{}"[..]));
    }

    // Shape pinned from a real `two_pass` post-processed frame (two voices, one
    // clip): the speaker id lives at `result.utterances[].additions.speaker_id`
    // (a string) on `definite` utterances. A ≥2-speaker clip renders as a
    // labeled transcript, one `说话人{id}：` line per utterance, in order.
    #[test]
    fn multi_speaker_renders_labeled_transcript() {
        let json = r#"{"result":{"utterances":[
            {"definite":true,"text":"你好，我是张经理，明天上午10点的会议还照常进行吗？","additions":{"source":"two_pass","speaker_id":"1"}},
            {"definite":true,"text":"照常进行，我已经通知了所有人，会议室也订好了。","additions":{"source":"two_pass","speaker_id":"0"}}
        ]}}"#;
        let frame = result_frame(true, json.as_bytes());
        let payload = server_payload(&frame).expect("payload");
        let parsed: ServerResponse = serde_json::from_slice(payload).expect("valid JSON");
        let labeled = parsed.result.as_ref().and_then(|r| r.labeled_transcript());
        assert_eq!(
            labeled.as_deref(),
            Some(
                "说话人1：你好，我是张经理，明天上午10点的会议还照常进行吗？\n\
                 说话人0：照常进行，我已经通知了所有人，会议室也订好了。"
            )
        );
    }

    // On the streaming two-pass path, a finalized (definite) segment carries its
    // speaker label at `additions.speaker_id`, aligned with `definite_text` (both
    // take the last definite utterance) so the dispatched sentence and its speaker
    // come from the same segment.
    #[test]
    fn definite_speaker_id_tracks_the_finalized_segment() {
        let json = r#"{"result":{"utterances":[
            {"definite":true,"text":"明天的会还开吗？","additions":{"source":"two_pass","speaker_id":"1"}}
        ]}}"#;
        let frame = result_frame(true, json.as_bytes());
        let payload = server_payload(&frame).expect("payload");
        let parsed: ServerResponse = serde_json::from_slice(payload).expect("valid JSON");
        let result = parsed.result.as_ref().unwrap();
        assert_eq!(result.definite_text().as_deref(), Some("明天的会还开吗？"));
        assert_eq!(result.definite_speaker_id().as_deref(), Some("1"));

        // No speaker info on the segment → no label (1:1, diarization off).
        let plain = r#"{"result":{"utterances":[{"definite":true,"text":"在的。","additions":{}}]}}"#;
        let frame = result_frame(true, plain.as_bytes());
        let payload = server_payload(&frame).expect("payload");
        let parsed: ServerResponse = serde_json::from_slice(payload).expect("valid JSON");
        assert_eq!(parsed.result.as_ref().unwrap().definite_speaker_id(), None);
    }

    // The voiceprint path needs *every* finalized speaker turn with its audio
    // span, not just the last (`definite_speaker_id`). A two-pass frame carries
    // `start_time`/`end_time` (ms) on each utterance; `definite_spans` collects
    // them so each speaker's own audio can be sliced and embedded cleanly.
    #[test]
    fn definite_spans_yields_one_per_speaker_with_timing() {
        let json = r#"{"result":{"utterances":[
            {"definite":true,"text":"你好","start_time":0,"end_time":1500,"additions":{"source":"two_pass","speaker_id":"0"}},
            {"definite":true,"text":"在的","start_time":1500,"end_time":3200,"additions":{"source":"two_pass","speaker_id":"1"}}
        ]}}"#;
        let frame = result_frame(true, json.as_bytes());
        let payload = server_payload(&frame).expect("payload");
        let parsed: ServerResponse = serde_json::from_slice(payload).expect("valid JSON");
        let spans = parsed.result.as_ref().unwrap().definite_spans();
        assert_eq!(spans.len(), 2);
        assert_eq!(spans[0].speaker_id, "0");
        assert_eq!((spans[0].start_ms, spans[0].end_ms), (0, 1500));
        assert_eq!(spans[1].speaker_id, "1");
        assert_eq!((spans[1].start_ms, spans[1].end_ms), (1500, 3200));
    }

    // Drop utterances that can't be sliced/attributed: no speaker, no timing, not
    // yet definite, or a non-positive span.
    #[test]
    fn definite_spans_skips_unusable_utterances() {
        let json = r#"{"result":{"utterances":[
            {"definite":true,"text":"a","start_time":0,"end_time":1000},
            {"definite":true,"text":"b","additions":{"speaker_id":"0"}},
            {"definite":false,"text":"c","start_time":0,"end_time":1000,"additions":{"speaker_id":"0"}},
            {"definite":true,"text":"d","start_time":2000,"end_time":2000,"additions":{"speaker_id":"1"}}
        ]}}"#;
        let frame = result_frame(true, json.as_bytes());
        let payload = server_payload(&frame).expect("payload");
        let parsed: ServerResponse = serde_json::from_slice(payload).expect("valid JSON");
        assert!(parsed.result.as_ref().unwrap().definite_spans().is_empty());
    }

    // A single speaker (or no speaker info) stays a flat transcript — the
    // caller falls back to `text`, so 1:1 conversations are untouched.
    #[test]
    fn single_speaker_is_not_labeled() {
        let one = r#"{"result":{"utterances":[
            {"definite":true,"text":"你好。","additions":{"speaker_id":"0"}},
            {"definite":true,"text":"在的。","additions":{"speaker_id":"0"}}
        ]}}"#;
        let frame = result_frame(true, one.as_bytes());
        let payload = server_payload(&frame).expect("payload");
        let parsed: ServerResponse = serde_json::from_slice(payload).expect("valid JSON");
        assert_eq!(parsed.result.as_ref().and_then(|r| r.labeled_transcript()), None);

        // No speaker_id at all → also flat.
        let none = r#"{"result":{"utterances":[{"definite":true,"text":"你好。","additions":{}}]}}"#;
        let frame = result_frame(true, none.as_bytes());
        let payload = server_payload(&frame).expect("payload");
        let parsed: ServerResponse = serde_json::from_slice(payload).expect("valid JSON");
        assert_eq!(parsed.result.as_ref().and_then(|r| r.labeled_transcript()), None);
    }
}
