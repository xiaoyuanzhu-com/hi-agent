//! The vision input channel: a live camera, streamed as video.
//!
//! "Vision is video." The camera streams continuously — the client does not
//! pre-sample frames — so the *backend* is the control point for how much it
//! actually looks. Today nothing decodes or samples the stream (the agent is
//! text-only); the bytes simply flow and are observable, the same way the audio
//! input channel carries raw audio. When a perception path lands, it subscribes
//! to `video_in` and applies its own sample rate.
//!
//! Stream (`WS /api/in/vision/stream`): the client runs a `MediaRecorder` and
//! ships encoded chunks as binary frames for the whole time the camera is open;
//! upload-only, nothing comes back on the socket. The container is negotiated
//! client-side — fragmented MP4 (hardware HEVC/H.264) where available, else WebM
//! (software VP8/VP9) — and rides through as the source mime. Each chunk is
//! republished on the `video_in` broadcast. Such a stream is only decodable from
//! its first chunk (the initialization segment), so that chunk is cached per
//! scene ([`VideoSource`]) to let an observer join mid-stream.
//!
//! Observe (`GET /api/in/vision`): the live video for the scene, one camera
//! session per chunked response — the visual twin of `GET /api/in/audio`. If a
//! camera is already live, the response opens with the cached init segment so
//! MediaSource can decode immediately; otherwise it blocks for the next session.
//! The `Content-Type` is the source's `video/webm;codecs=…`.
//!
//! Clip (`POST /api/in/vision`): a one-off still image, persisted to disk for
//! audit. It is not broadcast — the live channel is video.

use std::sync::Arc;
use std::sync::atomic::Ordering;

use axum::body::{Body, Bytes};
use axum::extract::ws::{Message as WsMessage, WebSocketUpgrade};
use axum::extract::{Query, State};
use axum::http::header::CONTENT_TYPE;
use axum::http::{HeaderMap, HeaderValue, StatusCode};
use axum::response::IntoResponse;
use futures::StreamExt;
use serde::Deserialize;
use tokio::sync::broadcast::error::RecvError;

use chrono::{DateTime, Utc};
use uuid::Uuid;

use crate::capabilities::vision::{self as vision_cap, VisualMedia};
use crate::memory::layout::MediaSlot;
use crate::memory::media;
use crate::server::headers::{AuthBearer, RequiredScene, SceneHeader};
use crate::server::{AppState, VideoInEvent, VideoSource};
use crate::types::{Channel, JournalEntry, Media, Origin, Scene};

const DEFAULT_IMAGE_MIME: &str = "image/jpeg";
const DEFAULT_VIDEO_MIME: &str = "video/webm";

/// The instruction the placeholder perception passes to the vision capability.
const VISION_PROMPT: &str = "Describe what you see, briefly.";

pub async fn post_vision(
    State(state): State<Arc<AppState>>,
    SceneHeader(scene): SceneHeader,
    headers: HeaderMap,
    body: Bytes,
) -> impl IntoResponse {
    if body.is_empty() {
        return (StatusCode::BAD_REQUEST, "vision body is empty\n").into_response();
    }

    let mime = headers
        .get(CONTENT_TYPE)
        .and_then(|v| v.to_str().ok())
        .map(str::to_owned)
        .unwrap_or_else(|| DEFAULT_IMAGE_MIME.to_string());
    let ext = mime_to_ext(&mime);

    tracing::debug!(scene = %scene, mime = %mime, bytes = body.len(), "POST /api/in/vision");

    // A one-off still: persist the bytes, then perceive it — a caption (the
    // vision capability, or a placeholder when none is configured) becomes the
    // signal's text surface. Perception runs in the background so the POST
    // returns promptly; the journaled signal lands a moment later.
    let ts = Utc::now();
    let rel = match media::store_blob(&state.data_dir, &scene, Channel::Vision, ts, MediaSlot::InputOneOff, ext, &body).await {
        Ok(rel) => rel,
        Err(err) => {
            tracing::error!(error = %err, "failed to persist vision frame");
            return (StatusCode::INTERNAL_SERVER_ERROR, "vision store failed\n").into_response();
        }
    };
    let visual = VisualMedia::image_bytes(body, mime.clone());
    spawn_perceive(state.clone(), scene.clone(), visual, rel, mime, ts, None);
    StatusCode::ACCEPTED.into_response()
}

/// Spawn the perception of one piece of visual media: caption it (via the vision
/// capability, or a placeholder when unconfigured) and journal a `Vision` signal
/// whose `body` is that caption and whose `media` points at the stored blob.
/// Runs detached so capture never blocks on the (possibly remote) understanding.
fn spawn_perceive(
    state: Arc<AppState>,
    scene: Scene,
    media: VisualMedia,
    blob_rel: String,
    mime: String,
    ts: DateTime<Utc>,
    duration_ms: Option<u64>,
) {
    tokio::spawn(async move {
        let body = caption(media, &mime).await;
        let entry = JournalEntry::SignalIn {
            id: Uuid::now_v7().to_string(),
            ts,
            channel: Channel::Vision,
            scene: scene.clone(),
            body,
            stream: None,
            media: Some(Media { file: blob_rel, mime, duration_ms, width: None, height: None }),
            origin: Some(Origin::Human),
        };
        if let Err(err) = state.memory.journal.append(entry).await {
            tracing::warn!(scene = %scene, error = %err, "journal append failed for vision perception");
        }
    });
}

/// Caption a piece of visual media. Uses the configured vision capability when
/// available; otherwise returns a placeholder so the signal still carries a text
/// surface (the bytes are persisted regardless).
async fn caption(media: VisualMedia, mime: &str) -> String {
    if vision_cap::available() {
        match vision_cap::understand(media, VISION_PROMPT).await {
            Ok(text) if !text.trim().is_empty() => text,
            Ok(_) => format!("[vision: empty understanding ({mime})]"),
            Err(err) => format!("[vision: understanding failed: {err}]"),
        }
    } else {
        format!("[vision capture ({mime}); understanding not configured]")
    }
}

/// Persist one wall-clock minute of camera media as
/// `vision/<date>/<HH>/<MM>.<ext>` (ext follows the stream's container —
/// `mp4`/`webm`), prefixed with the init segment so the file decodes standalone,
/// then perceive it (video → caption). Best-effort: a store failure is logged
/// and perception is skipped.
async fn flush_video_minute(
    state: &Arc<AppState>,
    scene: &Scene,
    mime: &str,
    init: Option<Bytes>,
    media_chunks: &[u8],
    ts: DateTime<Utc>,
) {
    let mut bytes = Vec::with_capacity(init.as_ref().map_or(0, |i| i.len()) + media_chunks.len());
    if let Some(i) = &init {
        bytes.extend_from_slice(i);
    }
    bytes.extend_from_slice(media_chunks);
    let ext = video_mime_to_ext(mime);
    let rel = match media::store_blob(&state.data_dir, scene, Channel::Vision, ts, MediaSlot::InputStream, ext, &bytes).await {
        Ok(rel) => rel,
        Err(err) => {
            tracing::warn!(scene = %scene, error = %err, "persisting camera minute failed");
            return;
        }
    };
    let visual = VisualMedia::video_bytes(Bytes::from(bytes), mime);
    spawn_perceive(state.clone(), scene.clone(), visual, rel, mime.to_string(), ts, None);
}

#[derive(Debug, Deserialize)]
pub struct StreamParams {
    /// The streaming scene. Browsers can't set `X-HI-Scene` on a WebSocket
    /// handshake, so the scene rides in the query string instead.
    scene: Option<String>,
    /// The exact `MediaRecorder` mime (`video/webm;codecs=vp8`) — an observer
    /// needs it verbatim to open a matching MediaSource buffer. Rides the query
    /// string for the same handshake reason as `scene`.
    mime: Option<String>,
}

/// `WS /api/in/vision/stream` — continuous inbound video over a WebSocket.
///
/// Upload-only: the client streams WebM chunks as binary frames for the whole
/// time the camera is open. Each chunk is republished on `video_in`; the first
/// chunk (the init segment) is cached so observers can join mid-stream.
pub async fn get_vision_stream(
    State(state): State<Arc<AppState>>,
    Query(params): Query<StreamParams>,
    ws: WebSocketUpgrade,
) -> impl IntoResponse {
    let scene = Scene(
        params
            .scene
            .filter(|s| !s.is_empty())
            .unwrap_or_else(|| "anonymous".to_string()),
    );
    let mime = params
        .mime
        .filter(|m| !m.trim().is_empty())
        .unwrap_or_else(|| DEFAULT_VIDEO_MIME.to_string());
    tracing::info!(scene = %scene, mime = %mime, "WS /api/in/vision/stream opened");
    ws.on_upgrade(move |socket| stream_video_in(state, scene, mime, socket))
}

async fn stream_video_in(
    state: Arc<AppState>,
    scene: Scene,
    mime: String,
    mut socket: axum::extract::ws::WebSocket,
) {
    // One WS connection is one inbound-video source; its chunks share a `turn` so
    // a `GET /api/in/vision` observer stays bound to this camera alone.
    let turn = state.video_in_turn.fetch_add(1, Ordering::Relaxed);
    let mut started = false;

    // Persist the camera on a wall-clock-minute grid: media chunks accumulate per
    // minute and flush to `vision/<date>/<HH>/<MM>.<ext>` at each rollover (and at
    // close). The init segment (the first chunk) prefixes every minute file so
    // each is independently decodable. Each flushed minute is also perceived.
    let mut cap_init: Option<Bytes> = None;
    let mut cap_minute: Option<String> = None;
    let mut cap_ts = Utc::now();
    let mut cap_buf: Vec<u8> = Vec::new();

    while let Some(msg) = socket.recv().await {
        match msg {
            Ok(WsMessage::Binary(b)) => {
                let now = Utc::now();
                if !started {
                    // The first chunk is the init segment: cache it for
                    // late-joining observers and for prefixing each minute file,
                    // then announce the source. It is not buffered as media.
                    started = true;
                    cap_init = Some(b.clone());
                    cap_minute = Some(now.format("%Y-%m-%dT%H:%M").to_string());
                    cap_ts = now;
                    state.video_in_live.lock().unwrap().insert(
                        scene.clone(),
                        VideoSource { turn, mime: mime.clone(), init: b.clone() },
                    );
                    let _ = state.video_in.send(VideoInEvent::Start {
                        scene: Some(scene.clone()),
                        turn,
                        mime: mime.clone(),
                    });
                } else {
                    let minute = now.format("%Y-%m-%dT%H:%M").to_string();
                    if cap_minute.as_deref() != Some(minute.as_str()) {
                        if !cap_buf.is_empty() {
                            flush_video_minute(&state, &scene, &mime, cap_init.clone(), &cap_buf, cap_ts).await;
                        }
                        cap_buf.clear();
                        cap_minute = Some(minute);
                        cap_ts = now;
                    }
                    cap_buf.extend_from_slice(&b);
                }
                let _ = state.video_in.send(VideoInEvent::Frame {
                    scene: Some(scene.clone()),
                    turn,
                    bytes: b,
                });
            }
            Ok(WsMessage::Close(_)) | Err(_) => break,
            Ok(_) => {}
        }
    }

    // Flush the final, partial minute of camera media.
    if !cap_buf.is_empty() {
        flush_video_minute(&state, &scene, &mime, cap_init.clone(), &cap_buf, cap_ts).await;
    }

    if started {
        // Clear the cache only if we're still the active source (a newer camera
        // may have replaced us), then close the source for observers.
        let mut live = state.video_in_live.lock().unwrap();
        if live.get(&scene).map(|s| s.turn) == Some(turn) {
            live.remove(&scene);
        }
        drop(live);
        let _ = state.video_in.send(VideoInEvent::End { scene: Some(scene.clone()), turn });
    }
    tracing::info!(scene = %scene, "WS /api/in/vision/stream closed");
}

/// Whether an event routed to `target` should reach this `scene` subscriber.
fn routed(target: &Option<Scene>, scene: &Scene) -> bool {
    match target {
        None => true,
        Some(t) => t == scene,
    }
}

/// `GET /api/in/vision` — the live camera video on this scene, one session per
/// long-poll. The visual twin of [`crate::server::audio::get_in_audio`], with a
/// cached-init prelude so an observer can join a camera that's already running.
pub async fn get_vision(
    State(state): State<Arc<AppState>>,
    RequiredScene(scene): RequiredScene,
    AuthBearer(auth): AuthBearer,
) -> impl IntoResponse {
    let mut rx = state.video_in.subscribe();

    tracing::info!(scene = %scene, auth = ?auth, "GET /api/in/vision long-poll opened");

    // Subscribe first, then look for an already-active camera: any frame that
    // lands between these two steps is still caught on `rx`.
    let active = state.video_in_live.lock().unwrap().get(&scene).cloned();

    let (turn, mime, init): (u64, String, Option<Bytes>) = match active {
        // A camera is already live: open with its cached init segment, then play
        // the live frames that follow.
        Some(src) => (src.turn, src.mime, Some(src.init)),
        // No camera yet: block until one starts. Its init segment arrives as the
        // first `Frame` on `rx`, so nothing to prepend.
        None => loop {
            match rx.recv().await {
                Ok(event) => {
                    if !routed(event.scene(), &scene) {
                        continue;
                    }
                    if let VideoInEvent::Start { turn, mime, .. } = event {
                        break (turn, mime, None);
                    }
                }
                Err(RecvError::Lagged(n)) => {
                    tracing::warn!(missed = n, "inbound-video subscriber lagged");
                    continue;
                }
                Err(RecvError::Closed) => {
                    return (StatusCode::SERVICE_UNAVAILABLE, "broadcast closed\n").into_response();
                }
            }
        },
    };

    // Stream this source's frames until its `End`. Frames from any other source
    // or scene are filtered out, so the response stays bound to one camera.
    let frames = futures::stream::unfold((rx, scene, turn), |(mut rx, scene, turn)| async move {
        loop {
            match rx.recv().await {
                Ok(event) => {
                    if !routed(event.scene(), &scene) || event.turn() != turn {
                        continue;
                    }
                    match event {
                        VideoInEvent::Frame { bytes, .. } => {
                            return Some((
                                Ok::<Bytes, std::convert::Infallible>(bytes),
                                (rx, scene, turn),
                            ));
                        }
                        VideoInEvent::End { .. } => return None,
                        VideoInEvent::Start { .. } => continue,
                    }
                }
                Err(RecvError::Lagged(n)) => {
                    tracing::warn!(missed = n, "inbound-video subscriber lagged mid-source");
                    continue;
                }
                Err(RecvError::Closed) => return None,
            }
        }
    });

    // Prepend the cached init segment (for a late join) ahead of the live frames.
    let init_stream = futures::stream::iter(init.map(Ok::<Bytes, std::convert::Infallible>));
    let body = init_stream.chain(frames);

    let mut response = Body::from_stream(body).into_response();
    if let Ok(val) = HeaderValue::from_str(&mime) {
        response.headers_mut().insert(CONTENT_TYPE, val);
    }
    response
}

fn mime_to_ext(mime: &str) -> &'static str {
    match mime.split(';').next().unwrap_or("").trim().to_ascii_lowercase().as_str() {
        "image/jpeg" | "image/jpg" => "jpg",
        "image/png" => "png",
        "image/webp" => "webp",
        "image/gif" => "gif",
        _ => "bin",
    }
}

/// Map a streaming video mime to a file extension for the persisted minute file.
/// The client negotiates the container at runtime (fragmented MP4 for hardware
/// HEVC/H.264, else WebM), so the extension must follow the actual stream.
fn video_mime_to_ext(mime: &str) -> &'static str {
    match mime.split(';').next().unwrap_or("").trim().to_ascii_lowercase().as_str() {
        "video/mp4" => "mp4",
        "video/webm" => "webm",
        _ => "bin",
    }
}
