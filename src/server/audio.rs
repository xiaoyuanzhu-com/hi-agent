//! The audio channel: inbound speech and outbound voice.
//!
//! "Audio is audio." The audio *input* channel carries audio bytes — observable
//! and playable the same way vision frames are. The transcript the agent reasons
//! over is a *derived* representation, so STT output is dispatched onto the **text**
//! channel (exactly like `POST /api/in/text`); the agent consumes text, while
//! `GET /api/in/audio` lets any client hear the raw audio.
//!
//! Inbound clip (`POST /api/in/audio`): the body bytes are audio; we save them
//! as a co-located `audio-<id>.<ext>` blob beside the scene's day-log, publish
//! them on the inbound-audio broadcast (so `GET /api/in/audio` can play the
//! clip), transcribe via the configured STT capability
//! ([`crate::capabilities::stt`]), and feed the transcript into the same
//! per-scene path that `POST /api/in/text` uses. The journal records a
//! `SignalIn { channel: Text, body: <transcript>, media: Some(..) }` — the
//! agent reads text, while the media reference (sharing the blob's id) links
//! back to the audio this transcript was derived from.
//!
//! Inbound stream (`WS /api/in/audio/stream`): the client streams raw 16 kHz mono
//! 16-bit PCM as binary frames for the whole time the mic is open; the upstream
//! STT does the endpointing. There is no client-side VAD and nothing is sent back
//! on the socket — it is upload-only. Each frame is republished on the
//! inbound-audio broadcast (so `GET /api/in/audio` plays the live mic), and each
//! finalized sentence is dispatched as a text `SignalIn`. The agent sees no live
//! partials — a sentence reaches it once, settled — but rolling partials *are*
//! echoed to scene observers (`GET /api/in/text`, `final:false`): they're the
//! barge-in trigger, letting a client duck its playback the instant speech is
//! recognized.
//!
//! Observe (`GET /api/in/audio`): the live audio bytes for the scene, one source
//! (mic stream or posted clip) per chunked response — the inbound mirror of
//! `GET /api/out/audio`. The `Start` event's mime tells the client how to decode
//! (`audio/pcm;rate=16000;channels=1` for the mic, the clip's own type for a POST).
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
use std::sync::atomic::Ordering;
use std::time::{Duration, Instant};

use axum::body::{Body, Bytes};
use axum::extract::ws::{Message as WsMessage, WebSocketUpgrade};
use axum::extract::{Query, State};
use axum::http::header::CONTENT_TYPE;
use axum::http::{HeaderMap, HeaderValue, StatusCode};
use axum::response::IntoResponse;
use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};
use tokio::sync::broadcast::error::RecvError;
use tokio::sync::mpsc;

use crate::capabilities::stt::{self, Transcript};
use crate::memory::media;
use crate::server::headers::{AuthBearer, RequiredScene, SceneHeader, StreamHeader};
use crate::server::{AppState, AudioEvent, AudioInEvent};
use crate::segment::{Segmenter, Speech};
use crate::types::{Channel, JournalEntry, Media, Origin, Scene, Signal};
use uuid::Uuid;

const DEFAULT_MIME: &str = "audio/wav";

/// Format of the live mic stream: raw 16 kHz mono signed 16-bit little-endian PCM.
/// Carried on the inbound-audio `Start` so a listener knows how to decode it.
const PCM_MIME: &str = "audio/pcm;rate=16000;channels=1";

#[derive(Debug, Serialize)]
struct PostAudioAck {
    transcript: String,
    media_path: String,
}

pub async fn post_audio(
    State(state): State<Arc<AppState>>,
    SceneHeader(scene): SceneHeader,
    StreamHeader(stream): StreamHeader,
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

    // The signal's id (uuidv7) also names its co-located blob; `ts` places both
    // in the same day-folder. Generate them before storing so the two agree.
    let ts = Utc::now();
    let id = Uuid::now_v7().to_string();

    // 1. Persist the raw bytes so we can replay/audit and so the log has a
    //    stable reference. We do this before STT so a transcription failure
    //    still leaves the audio on disk.
    let media_path = match media::store_blob(&state.data_dir, &scene, ts, Channel::Audio, &id, ext, &body).await {
        Ok(f) => f,
        Err(err) => {
            tracing::error!(error = %err, "failed to persist incoming audio");
            return (StatusCode::INTERNAL_SERVER_ERROR, "audio store failed\n").into_response();
        }
    };

    // 2. Publish the clip on the inbound-audio channel as one source, so any
    //    `GET /api/in/audio` listener can play it. Bytes are refcounted, so with
    //    no listener this is a cheap drop.
    let turn = state.audio_in_turn.fetch_add(1, Ordering::Relaxed);
    let _ = state.audio_in.send(AudioInEvent::Start {
        scene: Some(scene.clone()),
        turn,
        mime: mime.clone(),
    });
    let _ = state.audio_in.send(AudioInEvent::Frame {
        scene: Some(scene.clone()),
        turn,
        bytes: body.clone(),
    });
    let _ = state.audio_in.send(AudioInEvent::End { scene: Some(scene.clone()), turn });

    // 3. Transcribe. Errors surface as 502 — the upstream provider failed.
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

    // 4. The transcript is text: dispatch it onto the text channel exactly like a
    //    typed line. The agent reads text; the audio stays on the audio channel.
    //    The clip's (ts, id, media) ride along so the journal entry links back to
    //    the stored blob by the shared id.
    let media = Media {
        file: media_path.clone(),
        mime: mime.clone(),
        duration_ms: None,
        width: None,
        height: None,
    };
    if !deliver_transcript(&state, &scene, stream, &transcript, Some((ts, id, media))).await {
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
    /// The named stream within the scene, same role as `X-HI-Stream` on the POST
    /// path; absent/empty → the default stream. Rides the query string for the
    /// same handshake reason as `scene`.
    stream: Option<String>,
}

/// `GET /api/in/audio/stream` — continuous inbound speech over a WebSocket.
///
/// Upload-only: the client streams raw 16 kHz mono 16-bit PCM as binary frames
/// for the whole time the mic is open; the upstream STT does the endpointing.
/// There is no client-side VAD and nothing is sent back on the socket. Each frame
/// is republished on the inbound-audio broadcast so `GET /api/in/audio` plays the
/// live mic; each finalized sentence is dispatched on the text channel (the path
/// `POST /api/in/audio` uses for its transcript).
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
    let stream = params.stream.map(|s| s.trim().to_owned()).filter(|s| !s.is_empty());
    tracing::info!(scene = %scene, stream = ?stream, "WS /api/in/audio/stream opened");
    ws.on_upgrade(move |socket| stream_audio_in(state, scene, stream, socket))
}

async fn stream_audio_in(
    state: Arc<AppState>,
    scene: Scene,
    stream: Option<String>,
    mut socket: axum::extract::ws::WebSocket,
) {
    // PCM client → STT; Transcripts STT → dispatch. Bounded so a stalled
    // upstream exerts backpressure rather than buffering unboundedly.
    let (audio_tx, audio_rx) = mpsc::channel::<Bytes>(64);
    let (tr_tx, mut tr_rx) = mpsc::channel::<Transcript>(64);

    let stt_task = tokio::spawn(async move { stt::transcribe_streaming(audio_rx, tr_tx).await });

    // An explicit Segmenter — not the upstream's silence flag — decides where the
    // continuous word-stream is cut into sentences for the agent. A periodic tick
    // drives the time-based cut rules when the speaker has gone quiet. Each
    // finalized sentence is delivered on the text channel; there are no partials.
    let relay_state = state.clone();
    let relay_scene = scene.clone();
    let relay_stream = stream.clone();
    let out_task = tokio::spawn(async move {
        let mut seg = Segmenter::new(Speech::default(), Instant::now());
        let mut ticker = tokio::time::interval(Duration::from_millis(150));
        ticker.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Skip);
        loop {
            let cuts = tokio::select! {
                msg = tr_rx.recv() => match msg {
                    Some(t) => {
                        // Echo every rolling partial to the scene's observers
                        // (`final:false`). This is the duck trigger: the client
                        // stops its own playback the moment speech is
                        // recognized, hundreds of ms before a sentence settles.
                        // The same moment is reported to the barge-in registry,
                        // whose own clock decides whether the agent's voice was
                        // probably still sounding (→ "what went unheard" note).
                        if !t.is_final && !t.text.trim().is_empty() {
                            relay_state.echo_input(&relay_scene, Channel::Text, &t.text, false);
                            relay_state.interrupts.note_speech(&relay_scene, tokio::time::Instant::now()).await;
                        }
                        seg.observe(&t.text, t.is_final, Instant::now())
                    }
                    None => break, // STT session ended
                },
                _ = ticker.tick() => seg.tick(Instant::now()),
            };
            for sentence in cuts {
                deliver_transcript(&relay_state, &relay_scene, relay_stream.clone(), &sentence, None).await;
            }
        }
        // Flush any trailing words as a final sentence when the session ends.
        if let Some(sentence) = seg.flush() {
            deliver_transcript(&relay_state, &relay_scene, relay_stream.clone(), &sentence, None).await;
        }
    });

    // One WS connection is one inbound-audio source: its frames carry a shared
    // `turn` so a `GET /api/in/audio` listener stays bound to this mic alone.
    let turn = state.audio_in_turn.fetch_add(1, Ordering::Relaxed);
    let mut started = false;

    // Pump inbound PCM until the client closes or the STT session ends (a send
    // error means `audio_rx` was dropped because `transcribe_streaming` returned).
    while let Some(msg) = socket.recv().await {
        match msg {
            Ok(WsMessage::Binary(b)) => {
                // Republish the raw PCM for `GET /api/in/audio` listeners. The
                // `Start` (carrying the format) precedes the first frame.
                if !started {
                    started = true;
                    let _ = state.audio_in.send(AudioInEvent::Start {
                        scene: Some(scene.clone()),
                        turn,
                        mime: PCM_MIME.to_owned(),
                    });
                }
                let _ = state.audio_in.send(AudioInEvent::Frame {
                    scene: Some(scene.clone()),
                    turn,
                    bytes: b.clone(),
                });
                if audio_tx.send(b).await.is_err() {
                    break;
                }
            }
            Ok(WsMessage::Close(_)) | Err(_) => break,
            Ok(_) => {}
        }
    }

    // Close the inbound-audio source so listeners end their current response.
    if started {
        let _ = state.audio_in.send(AudioInEvent::End { scene: Some(scene.clone()), turn });
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

/// Deliver one finalized transcript onto the **text** channel — journal it, echo
/// it to scene observers (settled), and hand it to the reactor — exactly as a
/// typed `POST /api/in/text` line. Returns `false` if the inbound channel is
/// closed. The agent consumes text either way; for a posted clip, `clip` carries
/// the `(ts, id, media)` of the stored audio blob so the journal entry shares the
/// blob's id and day-folder. The mic stream passes `None` — streaming utterances
/// aren't persisted as discrete media files, so the journal records no `media`.
async fn deliver_transcript(
    state: &AppState,
    scene: &Scene,
    stream: Option<String>,
    text: &str,
    clip: Option<(DateTime<Utc>, String, Media)>,
) -> bool {
    let (ts, id, media) = match clip {
        Some((ts, id, media)) => (ts, id, Some(media)),
        None => (Utc::now(), Uuid::now_v7().to_string(), None),
    };
    let signal = Signal {
        channel: Channel::Text,
        scene: scene.clone(),
        body: text.to_owned(),
        stream: stream.clone(),
        ts,
    };
    crate::channel_log::inbound(Channel::Text, scene, text);
    let entry = JournalEntry::SignalIn {
        id,
        ts,
        channel: Channel::Text,
        scene: scene.clone(),
        body: text.to_owned(),
        stream,
        media,
        origin: Some(Origin::Human),
    };
    if let Err(err) = state.memory.journal.append(entry).await {
        tracing::error!(error = %err, "journal append failed; accepting signal anyway");
    }
    // Echo before dispatching inward, so the line shows on every client the same
    // way a typed line does.
    state.echo_input(scene, Channel::Text, text, true);
    if let Err(err) = state.inbound.send(signal).await {
        tracing::error!(error = %err, "inbound channel closed");
        return false;
    }
    true
}

/// Whether an event routed to `target` should reach this `scene` subscriber.
fn routed(target: &Option<Scene>, scene: &Scene) -> bool {
    match target {
        None => true,
        Some(t) => t == scene,
    }
}

/// `GET /api/in/audio` — the live audio bytes on this scene, one source per
/// long-poll. The inbound mirror of [`get_out_audio`].
pub async fn get_in_audio(
    State(state): State<Arc<AppState>>,
    RequiredScene(scene): RequiredScene,
    AuthBearer(auth): AuthBearer,
) -> impl IntoResponse {
    let mut rx = state.audio_in.subscribe();

    tracing::info!(scene = %scene, auth = ?auth, "GET /api/in/audio long-poll opened");

    // Block until a source for this subscriber starts. `Start` carries the mime,
    // which must be set before any body byte; Frame/End seen before a Start (we
    // subscribed mid-source) are skipped — the client re-polls and catches the
    // next source cleanly.
    let (turn, mime) = loop {
        match rx.recv().await {
            Ok(event) => {
                if !routed(event.scene(), &scene) {
                    continue;
                }
                if let AudioInEvent::Start { turn, mime, .. } = event {
                    break (turn, mime);
                }
            }
            Err(RecvError::Lagged(n)) => {
                tracing::warn!(missed = n, "inbound-audio subscriber lagged");
                continue;
            }
            Err(RecvError::Closed) => {
                return (StatusCode::SERVICE_UNAVAILABLE, "broadcast closed\n").into_response();
            }
        }
    };

    // Stream this source's frames as a chunked body until its `End`. Frames from
    // any other source or scene are filtered out, so a response stays bound to the
    // single source it opened on.
    let stream = futures::stream::unfold((rx, scene, turn), |(mut rx, scene, turn)| async move {
        loop {
            match rx.recv().await {
                Ok(event) => {
                    if !routed(event.scene(), &scene) || event.turn() != turn {
                        continue;
                    }
                    match event {
                        AudioInEvent::Frame { bytes, .. } => {
                            return Some((
                                Ok::<Bytes, std::convert::Infallible>(bytes),
                                (rx, scene, turn),
                            ));
                        }
                        AudioInEvent::End { .. } => return None,
                        AudioInEvent::Start { .. } => continue,
                    }
                }
                Err(RecvError::Lagged(n)) => {
                    tracing::warn!(missed = n, "inbound-audio subscriber lagged mid-source");
                    continue;
                }
                Err(RecvError::Closed) => return None,
            }
        }
    });

    let mut response = Body::from_stream(stream).into_response();
    if let Ok(val) = HeaderValue::from_str(&mime) {
        response.headers_mut().insert(CONTENT_TYPE, val);
    }
    response
}

/// `GET /api/out/audio` — the agent's voice, one turn per long-poll.
pub async fn get_out_audio(
    State(state): State<Arc<AppState>>,
    RequiredScene(scene): RequiredScene,
    AuthBearer(auth): AuthBearer,
) -> impl IntoResponse {
    let mut rx = state.audio_out.subscribe();

    tracing::info!(scene = %scene, auth = ?auth, "GET /api/out/audio long-poll opened");

    // Opening this long-poll is a scene-presence signal: warm the scene up so its
    // process + session + upstream cache are hot before the first utterance.
    state.warm_scene(&scene);

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
