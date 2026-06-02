//! The audio channel: inbound speech and outbound voice.
//!
//! Inbound clip (`POST /api/in/audio`): the body bytes are audio; we save them
//! under `data/media/audio/in/<uuid>.<ext>`, transcribe via the configured STT
//! capability ([`crate::capabilities::stt`]), and feed the transcript into the
//! same per-scene path that `POST /api/in/text` uses. The journal records a
//! `SignalIn { channel: Audio, body: <transcript>, media_path: Some(path) }`
//! so the reactor's snapshot can show that this signal arrived as speech
//! while the body remains text-searchable. The transcript is also echoed to
//! scene observers (`GET /api/in/audio`).
//!
//! Inbound stream (`WS /api/in/audio/stream`): the client streams raw 16 kHz mono
//! 16-bit PCM as binary frames for the whole time the mic is open; the upstream
//! STT does the endpointing. There is no client-side VAD and nothing is sent back
//! on the socket — it is upload-only. Recognized results (partials + finalized
//! sentences) are published to the per-scene input-echo bus, so the uploading
//! client sees its own words the same way every other client does: via
//! `GET /api/in/audio`. Each finalized sentence is also dispatched as a `SignalIn`.
//!
//! Observe (`GET /api/in/audio`): a live NDJSON stream of recognized speech for
//! the scene (see [`crate::server::observe`]). Drives both display and the
//! scene-wide barge-in reflex (duck the agent's voice the moment a mic goes hot).
//!
//! Outbound (`GET /api/out/audio`): subscriber to the reactor's `audio_out`
//! broadcast. A turn's speech arrives as a `Start`/`Frame`*/`End` run; this
//! handler blocks until a `Start` for the subscriber, then streams that turn's
//! frames as one chunked HTTP response until the matching `End`. The client
//! appends the bytes to a single sink and plays — one continuous utterance per
//! response, no per-clip reassembly. After the response closes the client re-GETs
//! for the next turn (same loop shape as the other channels).
//!
//! Capability gating: missing STT → 501 on POST/stream. Missing TTS → no audio
//! events are ever broadcast; GET /api/out/audio blocks forever (same long-poll
//! semantics as the other channels — the request is fine, the agent just never
//! speaks).

use std::sync::Arc;
use std::time::{Duration, Instant};

use axum::body::{Body, Bytes};
use axum::extract::ws::{Message as WsMessage, WebSocketUpgrade};
use axum::extract::{Query, State};
use axum::http::header::CONTENT_TYPE;
use axum::http::{HeaderMap, HeaderValue, StatusCode};
use axum::response::{IntoResponse, Response};
use chrono::Utc;
use serde::{Deserialize, Serialize};
use tokio::sync::broadcast::error::RecvError;
use tokio::sync::mpsc;

use crate::capabilities::stt::{self, Transcript};
use crate::memory::media::{self, Direction};
use crate::server::headers::{AuthBearer, RequiredScene, SceneHeader};
use crate::server::{observe, AppState, AudioEvent};
use crate::segment::{Segmenter, Speech};
use crate::types::{Channel, JournalEntry, Scene, Signal};

const DEFAULT_MIME: &str = "audio/wav";

#[derive(Debug, Serialize)]
struct PostAudioAck {
    transcript: String,
    media_path: String,
}

pub async fn post_audio(
    State(state): State<Arc<AppState>>,
    SceneHeader(scene): SceneHeader,
    AuthBearer(auth): AuthBearer,
    headers: HeaderMap,
    body: Bytes,
) -> impl IntoResponse {
    if !stt::available() {
        return (
            StatusCode::NOT_IMPLEMENTED,
            "audio capability not configured (set STT_PROVIDER)\n",
        )
            .into_response();
    }

    if body.is_empty() {
        return (StatusCode::BAD_REQUEST, "audio body is empty\n").into_response();
    }

    let mime = headers
        .get(CONTENT_TYPE)
        .and_then(|v| v.to_str().ok())
        .map(str::to_owned)
        .unwrap_or_else(|| DEFAULT_MIME.to_string());
    let ext = mime_to_ext(&mime);

    tracing::info!(
        scene = %scene,
        auth = ?auth,
        mime = %mime,
        bytes = body.len(),
        "POST /audio"
    );

    // 1. Persist the raw bytes so we can replay/audit and so the journal has
    //    a stable reference. We do this before STT so a transcription failure
    //    still leaves the audio on disk.
    let media_path = match media::store_audio(&state.data_dir, Direction::In, ext, &body).await {
        Ok(p) => p,
        Err(err) => {
            tracing::error!(error = %err, "failed to persist incoming audio");
            return (StatusCode::INTERNAL_SERVER_ERROR, "audio store failed\n").into_response();
        }
    };

    // 2. Transcribe. Errors surface as 502 — the upstream provider failed.
    let transcript = match stt::transcribe(body, &mime).await {
        Ok(t) => t,
        Err(err) => {
            tracing::warn!(error = %err, media_path = %media_path, "STT transcribe failed");
            return (
                StatusCode::BAD_GATEWAY,
                format!("transcription failed: {err}\n"),
            )
                .into_response();
        }
    };

    // Empty transcript = the clip held no recognizable speech. The upstream
    // cannot distinguish silence from un-transcribable sound, so we don't try:
    // there's nothing to journal or dispatch. Return a benign ack (the raw
    // audio is already persisted for audit) — the SPA reads the empty
    // transcript and drops back to idle rather than treating it as a failure.
    if transcript.trim().is_empty() {
        tracing::info!(scene = %scene, media_path = %media_path, "audio clip held no speech");
        let ack = PostAudioAck { transcript: String::new(), media_path };
        return (StatusCode::ACCEPTED, axum::Json(ack)).into_response();
    }

    // 3. Journal + dispatch — identical to thought.rs from this point, except
    //    the channel is Audio and we carry the media_path.
    let ts = Utc::now();
    let signal = Signal {
        channel: Channel::Audio,
        scene: scene.clone(),
        body: transcript.clone(),
        ts,
    };
    crate::channel_log::inbound(Channel::Audio, &scene, &transcript);
    let entry = JournalEntry::SignalIn {
        ts,
        channel: Channel::Audio,
        scene: scene.clone(),
        body: transcript.clone(),
        media_path: Some(media_path.clone()),
    };
    if let Err(err) = state.memory.journal.append(entry).await {
        tracing::error!(error = %err, "journal append failed; accepting signal anyway");
    }
    // Echo the recognized clip to scene observers (a settled utterance).
    state.echo_input(&scene, Channel::Audio, &transcript, true);
    if let Err(err) = state.inbound.send(signal).await {
        tracing::error!(error = %err, "inbound channel closed");
        return (StatusCode::SERVICE_UNAVAILABLE, "inbound channel closed\n").into_response();
    }

    let ack = PostAudioAck { transcript, media_path };
    (StatusCode::ACCEPTED, axum::Json(ack)).into_response()
}

#[derive(Debug, Deserialize)]
pub struct StreamParams {
    /// The streaming scene. Browsers can't set `X-HI-Scene` on a WebSocket
    /// handshake, so the scene rides in the query string instead.
    scene: Option<String>,
}

/// `GET /api/in/audio/stream` — continuous inbound speech over a WebSocket.
///
/// Upload-only: the client streams raw 16 kHz mono 16-bit PCM as binary frames
/// for the whole time the mic is open; the upstream STT does the endpointing.
/// There is no client-side VAD and nothing is sent back on the socket. Recognized
/// results (partials + finalized sentences) are published to the per-scene
/// input-echo bus, so the uploader observes its own words via `GET /api/in/audio`
/// exactly like every other client — that stream also drives barge-in. Each
/// finalized sentence is dispatched as a `SignalIn` (the path `POST /api/in/audio`
/// uses).
pub async fn get_audio_stream(
    State(state): State<Arc<AppState>>,
    Query(params): Query<StreamParams>,
    ws: WebSocketUpgrade,
) -> impl IntoResponse {
    if !stt::available() {
        return (
            StatusCode::NOT_IMPLEMENTED,
            "audio capability not configured (set STT_PROVIDER)\n",
        )
            .into_response();
    }
    let scene = Scene(
        params
            .scene
            .filter(|s| !s.is_empty())
            .unwrap_or_else(|| "anonymous".to_string()),
    );
    tracing::info!(scene = %scene, "WS /api/in/audio/stream opened");
    ws.on_upgrade(move |socket| stream_audio_in(state, scene, socket))
}

async fn stream_audio_in(
    state: Arc<AppState>,
    scene: Scene,
    mut socket: axum::extract::ws::WebSocket,
) {
    // PCM client → STT; Transcripts STT → echo/dispatch. Bounded so a stalled
    // upstream exerts backpressure rather than buffering unboundedly.
    let (audio_tx, audio_rx) = mpsc::channel::<Bytes>(64);
    let (tr_tx, mut tr_rx) = mpsc::channel::<Transcript>(64);

    let stt_task = tokio::spawn(async move { stt::transcribe_streaming(audio_rx, tr_tx).await });

    // Publish raw STT results to scene observers (partials drive display +
    // barge-in everywhere) while an explicit Segmenter — not the upstream's
    // silence flag — decides where the continuous word-stream is cut into
    // sentences for the agent. A periodic tick drives the time-based cut rules
    // when the speaker has gone quiet.
    let relay_state = state.clone();
    let relay_scene = scene.clone();
    let out_task = tokio::spawn(async move {
        let mut seg = Segmenter::new(Speech::default(), Instant::now());
        let mut ticker = tokio::time::interval(Duration::from_millis(150));
        ticker.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Skip);
        loop {
            let cuts = tokio::select! {
                msg = tr_rx.recv() => match msg {
                    Some(t) => {
                        // Rolling partial → scene observers (display + barge-in);
                        // the agent hears the segmenter's cut below, never this.
                        if !t.text.trim().is_empty() {
                            relay_state.echo_input(&relay_scene, Channel::Audio, &t.text, false);
                        }
                        seg.observe(&t.text, t.is_final, Instant::now())
                    }
                    None => break, // STT session ended
                },
                _ = ticker.tick() => seg.tick(Instant::now()),
            };
            for sentence in cuts {
                // A completed sentence: echo it as settled and hand it to the agent.
                relay_state.echo_input(&relay_scene, Channel::Audio, &sentence, true);
                dispatch_utterance(&relay_state, &relay_scene, &sentence).await;
            }
        }
        // Flush any trailing words as a final sentence when the session ends.
        if let Some(sentence) = seg.flush() {
            relay_state.echo_input(&relay_scene, Channel::Audio, &sentence, true);
            dispatch_utterance(&relay_state, &relay_scene, &sentence).await;
        }
    });

    // Pump inbound PCM until the client closes or the STT session ends (a send
    // error means `audio_rx` was dropped because `transcribe_streaming` returned).
    while let Some(msg) = socket.recv().await {
        match msg {
            Ok(WsMessage::Binary(b)) => {
                if audio_tx.send(b).await.is_err() {
                    break;
                }
            }
            Ok(WsMessage::Close(_)) | Err(_) => break,
            Ok(_) => {}
        }
    }

    // Closing the audio side lets the STT session flush its last utterance.
    drop(audio_tx);
    match tokio::time::timeout(Duration::from_secs(5), stt_task).await {
        Ok(Ok(Err(err))) => tracing::warn!(scene = %scene, error = %err, "audio stream STT ended"),
        Err(_) => tracing::warn!(scene = %scene, "audio stream STT did not finalize in time"),
        _ => {}
    }
    out_task.abort();
    tracing::info!(scene = %scene, "WS /api/in/audio/stream closed");
}

/// Journal + dispatch one finalized utterance — the streaming counterpart of the
/// tail of `post_audio`. Streaming utterances aren't persisted as discrete media
/// files (no per-clip blob); the journal records the transcript with no
/// `media_path`.
async fn dispatch_utterance(state: &AppState, scene: &Scene, text: &str) {
    let ts = Utc::now();
    let signal = Signal {
        channel: Channel::Audio,
        scene: scene.clone(),
        body: text.to_owned(),
        ts,
    };
    crate::channel_log::inbound(Channel::Audio, scene, text);
    let entry = JournalEntry::SignalIn {
        ts,
        channel: Channel::Audio,
        scene: scene.clone(),
        body: text.to_owned(),
        media_path: None,
    };
    if let Err(err) = state.memory.journal.append(entry).await {
        tracing::error!(error = %err, "journal append failed; accepting signal anyway");
    }
    if let Err(err) = state.inbound.send(signal).await {
        tracing::error!(error = %err, "inbound channel closed");
    }
}

/// Whether an event routed to `target` should reach this `scene` subscriber.
fn routed(target: &Option<Scene>, scene: &Scene) -> bool {
    match target {
        None => true,
        Some(t) => t == scene,
    }
}

/// `GET /api/in/audio` — observe recognized speech on this scene, live (NDJSON).
pub async fn get_in_audio(
    State(state): State<Arc<AppState>>,
    RequiredScene(scene): RequiredScene,
    AuthBearer(auth): AuthBearer,
) -> Response {
    tracing::info!(scene = %scene, auth = ?auth, "GET /api/in/audio observe opened");
    observe::stream_input(state, scene, Channel::Audio)
}

/// `GET /api/out/audio` — the agent's voice, one turn per long-poll.
pub async fn get_out_audio(
    State(state): State<Arc<AppState>>,
    RequiredScene(scene): RequiredScene,
    AuthBearer(auth): AuthBearer,
) -> impl IntoResponse {
    let mut rx = state.audio_out.subscribe();

    tracing::info!(scene = %scene, auth = ?auth, "GET /api/out/audio long-poll opened");

    // Block until a turn for this subscriber starts. `Start` carries the mime,
    // which must be set before any body byte; Frame/End seen before a Start
    // (we subscribed mid-turn) are skipped — the client re-polls and catches
    // the next turn cleanly.
    let (turn, mime) = loop {
        match rx.recv().await {
            Ok(event) => {
                if !routed(event.scene(), &scene) {
                    continue;
                }
                if let AudioEvent::Start { turn, mime, .. } = event {
                    break (turn, mime);
                }
            }
            Err(RecvError::Lagged(n)) => {
                tracing::warn!(missed = n, "audio subscriber lagged");
                continue;
            }
            Err(RecvError::Closed) => {
                return (StatusCode::SERVICE_UNAVAILABLE, "broadcast closed\n").into_response();
            }
        }
    };

    // Stream this turn's frames as a chunked body until its `End`. Frames from
    // any other turn or scene are filtered out, so a response stays bound to the
    // single turn it opened on.
    let stream = futures::stream::unfold(
        (rx, scene, turn, false),
        |(mut rx, scene, turn, done)| async move {
            if done {
                return None;
            }
            loop {
                match rx.recv().await {
                    Ok(event) => {
                        if !routed(event.scene(), &scene) || event.turn() != turn {
                            continue;
                        }
                        match event {
                            AudioEvent::Frame { bytes, .. } => {
                                return Some((
                                    Ok::<Bytes, std::convert::Infallible>(bytes),
                                    (rx, scene, turn, false),
                                ));
                            }
                            AudioEvent::End { .. } => return None,
                            AudioEvent::Start { .. } => continue,
                        }
                    }
                    Err(RecvError::Lagged(n)) => {
                        tracing::warn!(missed = n, "audio subscriber lagged mid-turn");
                        continue;
                    }
                    Err(RecvError::Closed) => return None,
                }
            }
        },
    );

    let mut response = Body::from_stream(stream).into_response();
    if let Ok(val) = HeaderValue::from_str(&mime) {
        response.headers_mut().insert(CONTENT_TYPE, val);
    }
    response
}

fn mime_to_ext(mime: &str) -> &'static str {
    match mime.split(';').next().unwrap_or("").trim().to_ascii_lowercase().as_str() {
        "audio/wav" | "audio/wave" | "audio/x-wav" => "wav",
        "audio/mpeg" | "audio/mp3" => "mp3",
        "audio/ogg" | "audio/opus" => "ogg",
        "audio/flac" => "flac",
        "audio/aac" | "audio/x-aac" => "aac",
        "audio/m4a" | "audio/x-m4a" | "audio/mp4" => "m4a",
        _ => "bin",
    }
}
