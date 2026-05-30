//! Volcengine seed-tts-2.0 TTS — bidirectional streaming WebSocket V3
//! (语音合成大模型 / 双向流式-V3).
//!
//! Wire protocol per <https://www.volcengine.com/docs/6561/1329505>:
//!
//!   wss://openspeech.bytedance.com/api/v3/tts/bidirection
//!
//! Connection headers (new console):
//!   X-Api-Key:          <api_key>
//!   X-Api-Resource-Id:  seed-tts-2.0   (selects model version + billing)
//!   X-Api-Connect-Id:   <uuid per connection>  (recommended, not reused)
//!
//! Same single-API-key scheme as the v3 ASR endpoint. `X-Api-Resource-Id`
//! picks the model/billing tier: seed-tts-2.0, seed-tts-1.0, etc.
//!
//! Every WS frame is binary:
//!
//!   ┌────────┬────────┬────────┬────────┬───────────────────────────────────┐
//!   │ b0     │ b1     │ b2     │ b3     │ optional fields, then payload      │
//!   │ ver/hs │ mt/flg │ ser/cp │ resv   │                                    │
//!   └────────┴────────┴────────┴────────┴───────────────────────────────────┘
//!
//!   b0 = (proto_version=1) << 4 | (header_size=1) = 0x11
//!   b1 = (msg_type) << 4 | flags        flags=WithEvent(0x4) on every frame here
//!   b2 = (serialization) << 4 | compression
//!   b3 = reserved (0x00)
//!
//! With the WithEvent flag set the header is followed by:
//!   [event: i32 BE]
//!   [session_id_len: u32 BE][session_id bytes]   — omitted for connection events
//!   [payload_len: u32 BE][payload bytes]
//!
//! Event sequence (client → / server ←):
//!   → StartConnection(1)   ← ConnectionStarted(50)
//!   → StartSession(100)    ← SessionStarted(150)
//!   → TaskRequest(200)     (text to synthesize)
//!   → FinishSession(102)
//!   ← AudioOnlyServer(0xB) frames (raw audio) … until TTSEnded(359)/SessionFinished(152)
//!   → FinishConnection(2)  ← ConnectionFinished(52)
//!
//! We expose the provider-agnostic `Tts::synthesize(text) -> AudioBlob`: one
//! sentence per call, one WS connection per call, all returned audio frames
//! concatenated into a single clip.

use std::time::Duration;

use async_trait::async_trait;
use bytes::Bytes;
use futures::{SinkExt, StreamExt};
use serde_json::json;
use tokio::time::timeout;
use tokio_tungstenite::tungstenite::client::IntoClientRequest;
use tokio_tungstenite::tungstenite::http::HeaderValue;
use tokio_tungstenite::tungstenite::Message;
use uuid::Uuid;

use super::tts::{AudioBlob, Tts};

const DEFAULT_ENDPOINT: &str = "wss://openspeech.bytedance.com/api/v3/tts/bidirection";
const DEFAULT_RESOURCE_ID: &str = "seed-tts-2.0";
const DEFAULT_VOICE: &str = "zh_female_vv_uranus_bigtts";
const DEFAULT_ENCODING: &str = "mp3";
const DEFAULT_SAMPLE_RATE: u32 = 24_000;

const ENV_API_KEY: &str = "VOLCENGINE_TTS_API_KEY";
const ENV_VOICE: &str = "VOLCENGINE_TTS_VOICE";
const ENV_ENCODING: &str = "VOLCENGINE_TTS_ENCODING";
const ENV_RESOURCE_ID: &str = "VOLCENGINE_TTS_RESOURCE_ID";
const ENV_ENDPOINT: &str = "VOLCENGINE_TTS_ENDPOINT";

const NAMESPACE: &str = "BidirectionalTTS";
const TOTAL_TIMEOUT: Duration = Duration::from_secs(30);

const PROTO_HEADER_BYTE0: u8 = 0x11; // proto v1, header size 1 (= 4 bytes)
const MSG_TYPE_FULL_CLIENT: u8 = 0b0001;
const MSG_TYPE_FULL_SERVER: u8 = 0b1001;
const MSG_TYPE_AUDIO_SERVER: u8 = 0b1011;
const MSG_TYPE_ERROR: u8 = 0b1111;
const FLAG_WITH_EVENT: u8 = 0b0100;
const SER_JSON: u8 = 0b0001;

// Event codes (server-side ConnectionStarted=50 / SessionStarted=150 are
// observed in the drain loop but not matched explicitly).
const EV_START_CONNECTION: i32 = 1;
const EV_FINISH_CONNECTION: i32 = 2;
const EV_CONNECTION_FAILED: i32 = 51;
const EV_START_SESSION: i32 = 100;
const EV_FINISH_SESSION: i32 = 102;
const EV_SESSION_FAILED: i32 = 153;
const EV_SESSION_FINISHED: i32 = 152;
const EV_TASK_REQUEST: i32 = 200;
const EV_TTS_ENDED: i32 = 359;

/// Events that travel without a `session_id` on the wire.
fn is_connection_event(event: i32) -> bool {
    matches!(event, 1 | 2 | 50 | 51 | 52)
}

pub struct VolcengineTts {
    api_key: String,
    resource_id: String,
    endpoint: String,
    voice: String,
    encoding: String,
}

impl VolcengineTts {
    pub fn from_env() -> anyhow::Result<Self> {
        let api_key = std::env::var(ENV_API_KEY).map_err(|_| {
            anyhow::anyhow!("{ENV_API_KEY} is required when TTS_PROVIDER=volcengine")
        })?;
        let resource_id =
            std::env::var(ENV_RESOURCE_ID).unwrap_or_else(|_| DEFAULT_RESOURCE_ID.to_string());
        let endpoint =
            std::env::var(ENV_ENDPOINT).unwrap_or_else(|_| DEFAULT_ENDPOINT.to_string());
        let voice = std::env::var(ENV_VOICE).unwrap_or_else(|_| DEFAULT_VOICE.to_string());
        let encoding =
            std::env::var(ENV_ENCODING).unwrap_or_else(|_| DEFAULT_ENCODING.to_string());
        Ok(Self {
            api_key,
            resource_id,
            endpoint,
            voice,
            encoding,
        })
    }
}

#[async_trait]
impl Tts for VolcengineTts {
    async fn synthesize(&self, text: &str) -> anyhow::Result<AudioBlob> {
        timeout(TOTAL_TIMEOUT, self.synthesize_inner(text))
            .await
            .map_err(|_| anyhow::anyhow!("volcengine TTS timed out after {TOTAL_TIMEOUT:?}"))?
    }
}

impl VolcengineTts {
    async fn synthesize_inner(&self, text: &str) -> anyhow::Result<AudioBlob> {
        let connect_id = Uuid::now_v7().to_string();
        let session_id = Uuid::now_v7().to_string();

        let mut request = self.endpoint.as_str().into_client_request()?;
        let headers = request.headers_mut();
        headers.insert("X-Api-Key", HeaderValue::from_str(&self.api_key)?);
        headers.insert("X-Api-Resource-Id", HeaderValue::from_str(&self.resource_id)?);
        headers.insert("X-Api-Connect-Id", HeaderValue::from_str(&connect_id)?);

        let (ws, response) = tokio_tungstenite::connect_async(request)
            .await
            .map_err(|e| anyhow::anyhow!("volcengine TTS WS connect failed: {e}"))?;
        tracing::debug!(
            status = %response.status(),
            connect_id = %connect_id,
            "volcengine TTS WS connected"
        );
        let (mut tx, mut rx) = ws.split();

        // 1. StartConnection — empty JSON payload, no session id.
        tx.send(Message::Binary(frame(
            MSG_TYPE_FULL_CLIENT,
            SER_JSON,
            EV_START_CONNECTION,
            None,
            b"{}",
        )))
        .await?;

        // 2. StartSession — voice + audio params.
        let audio_params = json!({
            "format": self.encoding,
            "sample_rate": DEFAULT_SAMPLE_RATE,
        });
        let start_payload = json!({
            "user": { "uid": "hi-agent" },
            "namespace": NAMESPACE,
            "event": EV_START_SESSION,
            "req_params": {
                "speaker": self.voice,
                "audio_params": audio_params,
            },
        });
        tx.send(Message::Binary(frame(
            MSG_TYPE_FULL_CLIENT,
            SER_JSON,
            EV_START_SESSION,
            Some(&session_id),
            &serde_json::to_vec(&start_payload)?,
        )))
        .await?;

        // 3. TaskRequest — the text to synthesize.
        let task_payload = json!({
            "user": { "uid": "hi-agent" },
            "namespace": NAMESPACE,
            "event": EV_TASK_REQUEST,
            "req_params": {
                "speaker": self.voice,
                "audio_params": audio_params,
                "text": text,
            },
        });
        tx.send(Message::Binary(frame(
            MSG_TYPE_FULL_CLIENT,
            SER_JSON,
            EV_TASK_REQUEST,
            Some(&session_id),
            &serde_json::to_vec(&task_payload)?,
        )))
        .await?;

        // 4. FinishSession — no more text coming.
        tx.send(Message::Binary(frame(
            MSG_TYPE_FULL_CLIENT,
            SER_JSON,
            EV_FINISH_SESSION,
            Some(&session_id),
            b"{}",
        )))
        .await?;

        // 5. Drain audio frames until the session ends.
        let mut audio = Vec::new();
        let mut done = false;
        while let Some(msg) = rx.next().await {
            let msg = msg.map_err(|e| anyhow::anyhow!("volcengine TTS WS recv: {e}"))?;
            let bytes = match msg {
                Message::Binary(b) => b,
                Message::Close(_) => break,
                _ => continue,
            };
            let Some(parsed) = parse_frame(&bytes) else {
                continue;
            };
            match parsed.msg_type {
                MSG_TYPE_AUDIO_SERVER => audio.extend_from_slice(parsed.payload),
                MSG_TYPE_FULL_SERVER => match parsed.event {
                    Some(EV_TTS_ENDED) | Some(EV_SESSION_FINISHED) => {
                        done = true;
                        break;
                    }
                    Some(EV_CONNECTION_FAILED) | Some(EV_SESSION_FAILED) => {
                        anyhow::bail!(
                            "volcengine TTS failed: {}",
                            String::from_utf8_lossy(parsed.payload)
                        );
                    }
                    _ => {}
                },
                MSG_TYPE_ERROR => {
                    anyhow::bail!(
                        "volcengine TTS server error: {}",
                        String::from_utf8_lossy(parsed.payload)
                    );
                }
                _ => {}
            }
        }

        // 6. Best-effort teardown; ignore failures, the audio is already in hand.
        let _ = tx
            .send(Message::Binary(frame(
                MSG_TYPE_FULL_CLIENT,
                SER_JSON,
                EV_FINISH_CONNECTION,
                None,
                b"{}",
            )))
            .await;
        let _ = tx.send(Message::Close(None)).await;

        if audio.is_empty() {
            anyhow::bail!(
                "volcengine TTS returned no audio (session {})",
                if done { "ended" } else { "interrupted" }
            );
        }
        Ok(AudioBlob {
            bytes: Bytes::from(audio),
            mime: encoding_to_mime(&self.encoding).to_string(),
            ext: encoding_to_ext(&self.encoding),
        })
    }
}

/// Build one WithEvent frame: 4-byte header + event + optional session id +
/// payload size + payload. `serialization` is JSON for control frames.
fn frame(
    msg_type: u8,
    serialization: u8,
    event: i32,
    session_id: Option<&str>,
    payload: &[u8],
) -> Vec<u8> {
    let mut out = Vec::with_capacity(16 + payload.len());
    out.push(PROTO_HEADER_BYTE0);
    out.push((msg_type << 4) | FLAG_WITH_EVENT);
    out.push(serialization << 4); // compression nibble = 0 (none)
    out.push(0);
    out.extend_from_slice(&event.to_be_bytes());
    if !is_connection_event(event) {
        let sid = session_id.unwrap_or("");
        out.extend_from_slice(&(sid.len() as u32).to_be_bytes());
        out.extend_from_slice(sid.as_bytes());
    }
    out.extend_from_slice(&(payload.len() as u32).to_be_bytes());
    out.extend_from_slice(payload);
    out
}

struct ParsedFrame<'a> {
    msg_type: u8,
    event: Option<i32>,
    payload: &'a [u8],
}

/// Parse a server frame, tolerating the optional event/session-id fields. The
/// remainder of the frame after a u32 size prefix is the payload (raw audio for
/// AudioOnlyServer, JSON otherwise). Returns `None` on a malformed frame.
fn parse_frame(bytes: &[u8]) -> Option<ParsedFrame<'_>> {
    if bytes.len() < 4 {
        return None;
    }
    let msg_type = (bytes[1] >> 4) & 0x0F;
    let flags = bytes[1] & 0x0F;
    let mut idx = 4usize;

    let mut event = None;
    if flags == FLAG_WITH_EVENT {
        let ev = i32::from_be_bytes(bytes.get(idx..idx + 4)?.try_into().ok()?);
        idx += 4;
        event = Some(ev);
        if !is_connection_event(ev) {
            let sid_len =
                u32::from_be_bytes(bytes.get(idx..idx + 4)?.try_into().ok()?) as usize;
            idx += 4 + sid_len;
        }
    }

    // Size-prefixed payload. Clamp to the frame in case the prefix is absent or
    // lies, so we still surface whatever bytes remain.
    let payload = if let Some(len_bytes) = bytes.get(idx..idx + 4) {
        let len = u32::from_be_bytes(len_bytes.try_into().ok()?) as usize;
        idx += 4;
        let end = (idx + len).min(bytes.len());
        &bytes[idx..end]
    } else {
        &bytes[bytes.len()..]
    };
    Some(ParsedFrame {
        msg_type,
        event,
        payload,
    })
}

fn encoding_to_mime(enc: &str) -> &'static str {
    match enc {
        "mp3" => "audio/mpeg",
        "wav" => "audio/wav",
        "ogg_opus" | "ogg" => "audio/ogg",
        "pcm" => "audio/L16",
        _ => "application/octet-stream",
    }
}

fn encoding_to_ext(enc: &str) -> &'static str {
    match enc {
        "mp3" => "mp3",
        "wav" => "wav",
        "ogg_opus" | "ogg" => "ogg",
        "pcm" => "pcm",
        _ => "bin",
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn frame_roundtrips_through_parse() {
        // A non-connection event carries the session id between header and payload.
        let f = frame(MSG_TYPE_FULL_CLIENT, SER_JSON, EV_TASK_REQUEST, Some("sess"), b"{\"a\":1}");
        let p = parse_frame(&f).expect("parse");
        assert_eq!(p.msg_type, MSG_TYPE_FULL_CLIENT);
        assert_eq!(p.event, Some(EV_TASK_REQUEST));
        assert_eq!(p.payload, b"{\"a\":1}");
    }

    #[test]
    fn connection_event_omits_session_id() {
        let f = frame(MSG_TYPE_FULL_CLIENT, SER_JSON, EV_START_CONNECTION, None, b"{}");
        let p = parse_frame(&f).expect("parse");
        assert_eq!(p.event, Some(EV_START_CONNECTION));
        assert_eq!(p.payload, b"{}");
    }

    #[test]
    fn parses_raw_audio_server_frame() {
        let f = frame(MSG_TYPE_AUDIO_SERVER, 0, EV_TASK_REQUEST, Some("s"), &[1, 2, 3, 4]);
        let p = parse_frame(&f).expect("parse");
        assert_eq!(p.msg_type, MSG_TYPE_AUDIO_SERVER);
        assert_eq!(p.payload, &[1, 2, 3, 4]);
    }
}
