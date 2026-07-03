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
//!   → TaskRequest(200)     (text to synthesize — sent MANY times per session)
//!   → FinishSession(102)
//!   ← AudioOnlyServer(0xB) frames (raw audio) … until TTSEnded(359)/SessionFinished(152)
//!   → FinishConnection(2)  ← ConnectionFinished(52)
//!
//! We expose the provider-agnostic streaming [`start`]: one WS session per
//! turn. [`start`] performs the connection + session handshake and returns a
//! [`TtsStream`]; a background driver task then feeds each pushed text chunk as
//! its own `TaskRequest` into the *same open session* and forwards every
//! `AudioOnlyServer` frame to the stream's receiver as it arrives. Dropping the
//! text sender sends `FinishSession`; the task tears the connection down once
//! the session ends. The audio is thus one continuous stream for the whole
//! turn, never a sequence of per-sentence clips.

use std::time::Duration;

use bytes::Bytes;
use futures::{SinkExt, StreamExt};
use serde_json::json;
use tokio::sync::mpsc;
use tokio::time::timeout;
use tokio_tungstenite::tungstenite::client::IntoClientRequest;
use tokio_tungstenite::tungstenite::http::HeaderValue;
use tokio_tungstenite::tungstenite::Message;
use uuid::Uuid;

use crate::body::capabilities::tts::TtsStream;

const DEFAULT_ENDPOINT: &str = "wss://openspeech.bytedance.com/api/v3/tts/bidirection";
const DEFAULT_RESOURCE_ID: &str = "seed-tts-2.0";
const DEFAULT_VOICE: &str = "zh_female_vv_uranus_bigtts";
const DEFAULT_ENCODING: &str = "mp3";
const DEFAULT_SAMPLE_RATE: u32 = 24_000;

const NAMESPACE: &str = "BidirectionalTTS";
/// Max quiet gap to wait for the server to flush audio *after* we've finished
/// sending text (FinishSession), before assuming the session hung. It does NOT
/// apply while text input is still open: a live turn may go quiet for a long
/// stretch while the agent searches or thinks, and tearing the session down
/// then would drop the rest of the turn's speech into a dead socket.
const IDLE_TIMEOUT: Duration = Duration::from_secs(30);
/// Channel depth for both text-in and frames-out; bounded so a slow consumer
/// applies backpressure rather than letting audio pile up unbounded.
const CHAN_DEPTH: usize = 64;

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

pub struct Config {
    api_key: String,
    resource_id: String,
    endpoint: String,
    voice: String,
    encoding: String,
}

impl Config {
    /// Resolve config from the credential store. `key` is the vendor API key
    /// (required — the caller builds a config only when a key is present); `base_url`
    /// host-rebases the endpoint onto the gateway (songguo) when the broker supplies
    /// one. Voice, encoding, and resource id use built-in defaults — no env.
    pub fn from_store(key: Option<&str>, base_url: Option<&str>) -> anyhow::Result<Self> {
        let api_key = key
            .map(str::trim)
            .filter(|k| !k.is_empty())
            .ok_or_else(|| anyhow::anyhow!("TTS (volcengine) requires an API key"))?
            .to_string();
        // The gateway (songguo) supplies the full `wss://` endpoint; use it verbatim.
        // With no base_url (BYOK) fall back to the vendor's own endpoint.
        let endpoint = match base_url.map(str::trim).filter(|b| !b.is_empty()) {
            Some(base) => base.trim_end_matches('/').to_string(),
            None => DEFAULT_ENDPOINT.to_string(),
        };
        Ok(Self {
            api_key,
            resource_id: DEFAULT_RESOURCE_ID.to_string(),
            endpoint,
            voice: DEFAULT_VOICE.to_string(),
            encoding: DEFAULT_ENCODING.to_string(),
        })
    }
}

pub async fn start(cfg: &Config) -> anyhow::Result<TtsStream> {
    let connect_id = Uuid::now_v7().to_string();
    let session_id = Uuid::now_v7().to_string();

    let mut request = cfg.endpoint.as_str().into_client_request()?;
    let headers = request.headers_mut();
    headers.insert("X-Api-Key", HeaderValue::from_str(&cfg.api_key)?);
    headers.insert("X-Api-Resource-Id", HeaderValue::from_str(&cfg.resource_id)?);
    headers.insert("X-Api-Connect-Id", HeaderValue::from_str(&connect_id)?);

    let (ws, response) = tokio_tungstenite::connect_async(request)
        .await
        .map_err(|e| anyhow::anyhow!("volcengine TTS WS connect failed: {e}"))?;
    tracing::debug!(
        status = %response.status(),
        connect_id = %connect_id,
        "volcengine TTS WS connected"
    );
    let (mut tx, rx) = ws.split();

    let audio_params = json!({
        "format": cfg.encoding,
        "sample_rate": DEFAULT_SAMPLE_RATE,
    });

    // Handshake before returning, so `start()` only resolves once the
    // session can accept text and a connect failure surfaces to the caller.
    tx.send(Message::Binary(frame(
        MSG_TYPE_FULL_CLIENT,
        SER_JSON,
        EV_START_CONNECTION,
        None,
        b"{}",
    )))
    .await?;
    let start_payload = json!({
        "user": { "uid": "hi-agent" },
        "namespace": NAMESPACE,
        "event": EV_START_SESSION,
        "req_params": {
            "speaker": cfg.voice,
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

    let (text_tx, text_rx) = mpsc::channel::<String>(CHAN_DEPTH);
    let (frame_tx, frame_rx) = mpsc::channel::<Bytes>(CHAN_DEPTH);

    let voice = cfg.voice.clone();
    tokio::spawn(drive_session(
        tx,
        rx,
        session_id,
        voice,
        audio_params,
        text_rx,
        frame_tx,
    ));

    Ok(TtsStream {
        mime: encoding_to_mime(&cfg.encoding).to_string(),
        text: text_tx,
        frames: frame_rx,
    })
}

/// Whether the driver loop should keep running after handling an event.
enum Flow {
    Continue,
    Break,
}

/// Background driver: feed each pushed text chunk as a `TaskRequest` into the
/// open session and forward every audio frame to `frame_tx` as it arrives.
/// Ends when the session finishes, the text sender is dropped *and* the server
/// flushes, the frame receiver is gone (barge-in), or the session goes idle.
async fn drive_session<Tx, Rx>(
    mut tx: Tx,
    mut rx: Rx,
    session_id: String,
    voice: String,
    audio_params: serde_json::Value,
    mut text_rx: mpsc::Receiver<String>,
    frame_tx: mpsc::Sender<Bytes>,
) where
    Tx: SinkExt<Message> + Unpin,
    Rx: StreamExt<Item = Result<Message, tokio_tungstenite::tungstenite::Error>> + Unpin,
{
    let mut text_done = false;
    loop {
        // While text input is still open (the turn is live), a quiet gap just
        // means the agent is thinking — wait indefinitely and keep the session
        // warm. Only once input is finished (FinishSession sent) do we guard the
        // server's flush with an idle deadline, to catch a hung session.
        let guard_idle = text_done;
        let step = async {
            tokio::select! {
                maybe_msg = rx.next() => handle_server_msg(maybe_msg, &frame_tx).await,
                text = text_rx.recv(), if !text_done => {
                    match text {
                        Some(t) => {
                            let task_payload = json!({
                                "user": { "uid": "hi-agent" },
                                "namespace": NAMESPACE,
                                "event": EV_TASK_REQUEST,
                                "req_params": {
                                    "speaker": voice,
                                    "audio_params": audio_params,
                                    "text": t,
                                },
                            });
                            match serde_json::to_vec(&task_payload) {
                                Ok(buf) => {
                                    let f = frame(MSG_TYPE_FULL_CLIENT, SER_JSON, EV_TASK_REQUEST, Some(&session_id), &buf);
                                    if tx.send(Message::Binary(f)).await.is_err() {
                                        return Flow::Break;
                                    }
                                }
                                Err(err) => tracing::warn!(error = %err, "volcengine TTS task payload encode failed"),
                            }
                            Flow::Continue
                        }
                        None => {
                            // End of input — flush and wait for the server to finish.
                            text_done = true;
                            let f = frame(MSG_TYPE_FULL_CLIENT, SER_JSON, EV_FINISH_SESSION, Some(&session_id), b"{}");
                            let _ = tx.send(Message::Binary(f)).await;
                            Flow::Continue
                        }
                    }
                }
            }
        };

        let flow = if guard_idle {
            match timeout(IDLE_TIMEOUT, step).await {
                Ok(flow) => flow,
                Err(_) => {
                    tracing::warn!(
                        "volcengine TTS idle {IDLE_TIMEOUT:?} after finish; tearing down"
                    );
                    break;
                }
            }
        } else {
            step.await
        };

        match flow {
            Flow::Continue => {}
            Flow::Break => break,
        }
    }

    // Best-effort teardown; ignore failures, the audio is already delivered.
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
}

/// Handle one server message: forward audio, detect end/error. Returns whether
/// the driver loop should continue.
async fn handle_server_msg(
    maybe_msg: Option<Result<Message, tokio_tungstenite::tungstenite::Error>>,
    frame_tx: &mpsc::Sender<Bytes>,
) -> Flow {
    let bytes = match maybe_msg {
        Some(Ok(Message::Binary(b))) => b,
        Some(Ok(Message::Close(_))) | None => return Flow::Break,
        Some(Ok(_)) => return Flow::Continue,
        Some(Err(e)) => {
            tracing::warn!(error = %e, "volcengine TTS WS recv error");
            return Flow::Break;
        }
    };
    let Some(parsed) = parse_frame(&bytes) else {
        return Flow::Continue;
    };
    match parsed.msg_type {
        MSG_TYPE_AUDIO_SERVER => {
            // Receiver gone (barge-in / client left) → stop the session.
            if frame_tx.send(Bytes::copy_from_slice(parsed.payload)).await.is_err() {
                return Flow::Break;
            }
            Flow::Continue
        }
        MSG_TYPE_FULL_SERVER => match parsed.event {
            Some(EV_TTS_ENDED) | Some(EV_SESSION_FINISHED) => Flow::Break,
            Some(EV_CONNECTION_FAILED) | Some(EV_SESSION_FAILED) => {
                tracing::warn!(
                    payload = %String::from_utf8_lossy(parsed.payload),
                    "volcengine TTS session failed"
                );
                Flow::Break
            }
            _ => Flow::Continue,
        },
        MSG_TYPE_ERROR => {
            tracing::warn!(
                payload = %String::from_utf8_lossy(parsed.payload),
                "volcengine TTS server error"
            );
            Flow::Break
        }
        _ => Flow::Continue,
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
