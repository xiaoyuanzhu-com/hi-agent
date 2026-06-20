//! The vision input channel: a live camera, streamed as video.
//!
//! "Vision is video." The camera streams continuously — the client does not
//! pre-sample frames — so the *backend* is the control point for how much it
//! actually looks. Each persisted minute-file is captioned, and when the face
//! capability is configured one keyframe is decoded out of it (via `ffmpeg`) and
//! run through face recognition, so the camera recognizes people the same way a
//! posted still does. The live `video_in` broadcast itself is not yet
//! frame-sampled in real time — perception works off the minute grid.
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

use crate::capabilities::face;
use crate::capabilities::vision::{self as vision_cap, VisualMedia};
use crate::vendors::ffmpeg_frame;
use crate::memory::layout::MediaSlot;
use crate::memory::media;
use crate::memory::people_vectors::{self, Modality};
use crate::server::headers::{AuthBearer, RequiredScene, SceneHeader};
use crate::server::{AppState, VideoInEvent, VideoSource};
use crate::types::{Channel, JournalEntry, Media, Origin, Scene};

const DEFAULT_IMAGE_MIME: &str = "image/jpeg";
const DEFAULT_VIDEO_MIME: &str = "video/webm";

/// The instruction the placeholder perception passes to the vision capability.
const VISION_PROMPT: &str = "Describe what you see, briefly.";

/// Cosine floor for naming a recognized face in the evidence note; below it the
/// face is shown as "unfamiliar". Deliberately low — the note is soft evidence
/// the agent weighs, not a verdict (same-person cosine runs ~0.7+, different ~0).
const RECOGNISE_MIN: f32 = 0.4;

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
    // The raw image bytes ride to perception twice: once wrapped for captioning,
    // once kept raw for face recognition (a still — video frame-sampling is later).
    let recognise = FaceSource::Image(body.clone());
    let visual = VisualMedia::image_bytes(body, mime.clone());
    spawn_perceive(state.clone(), scene.clone(), visual, rel, mime, ts, None, Some(recognise));
    StatusCode::ACCEPTED.into_response()
}

/// The media a perceived signal can recognize faces from: a still image is ready
/// for the face pipeline as-is; a video clip needs one keyframe decoded out first
/// ([`ffmpeg_frame::first_frame`]). Resolved inside the detached perceive task so
/// the (possibly slow) ffmpeg call never blocks capture.
enum FaceSource {
    Image(Bytes),
    Video(Bytes),
}

impl FaceSource {
    /// Reduce to an encoded still image the face pipeline accepts: an image is
    /// itself; a video yields its first frame. `None` (logged) on a decode failure.
    async fn into_image(self) -> Option<Bytes> {
        match self {
            FaceSource::Image(b) => Some(b),
            FaceSource::Video(b) => match ffmpeg_frame::first_frame(b).await {
                Ok(frame) => Some(frame),
                Err(err) => {
                    tracing::warn!(error = %err, "vision: keyframe extraction failed");
                    None
                }
            },
        }
    }
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
    recognise: Option<FaceSource>,
) {
    tokio::spawn(async move {
        let mut body = caption(media, &mime).await;
        // Fold any recognized faces into the caption as receive-time evidence —
        // the body is already a derived surface, so this matches it. Works for a
        // still image and for a camera-stream minute (one keyframe decoded out);
        // best-effort (never blocks or fails the signal).
        if face::available()
            && let Some(src) = recognise
            && let Some(img) = src.into_image().await
            && let Some(note) = face_note(img, &state.data_dir).await
        {
            body.push_str(&note);
        }
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

/// Recognize the faces in a still image and render them as one compact evidence
/// note to append to the caption, e.g. ` ⟨faces: 老王 ~0.83; unfamiliar⟩`. Returns
/// `None` when no face is found or detection fails — best-effort, the signal
/// stands either way. Each face is matched against the people store; a match
/// below [`RECOGNISE_MIN`] reads as "unfamiliar".
async fn face_note(bytes: Bytes, data_dir: &std::path::Path) -> Option<String> {
    let faces = match face::detect_and_embed(bytes).await {
        Ok(f) => f,
        Err(err) => {
            tracing::warn!(error = %err, "face recognition failed");
            return None;
        }
    };
    if faces.is_empty() {
        return None;
    }
    let mut parts = Vec::with_capacity(faces.len());
    for f in &faces {
        let top = people_vectors::nearest(data_dir, Modality::Face, &f.embedding, 1)
            .await
            .unwrap_or_default()
            .into_iter()
            .next();
        match top {
            Some(c) if c.similarity >= RECOGNISE_MIN => {
                parts.push(format!("{} ~{:.2}", c.subject, c.similarity))
            }
            _ => parts.push("unfamiliar".to_string()),
        }
    }
    Some(format!(" ⟨faces: {}⟩", parts.join("; ")))
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
    let video = Bytes::from(bytes);
    // Recognize faces from one keyframe of the minute when the face capability is
    // on — the camera's twin of the still-image path. Decoding happens inside the
    // detached perceive task, so the flush itself stays cheap.
    let recognise = face::available().then(|| FaceSource::Video(video.clone()));
    let visual = VisualMedia::video_bytes(video, mime);
    spawn_perceive(state.clone(), scene.clone(), visual, rel, mime.to_string(), ts, None, recognise);
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
    // close). The init segment prefixes every minute file so each is independently
    // decodable. Each flushed minute is also perceived.
    //
    // We can't assume "first WS chunk == the full init segment": `MediaRecorder`
    // splits a fragmented-MP4 init (`ftyp`+`moov`) across the first *two* chunks,
    // so caching only chunk 0 (the `ftyp`) drops the `moov` (track/codec config)
    // from every minute file but the first — they then fail to decode. So we
    // accumulate leading bytes in `init_acc` until the container delimits the init
    // segment ([`init_segment_len`]), cache that, and treat the remainder as the
    // first media bytes.
    let mut cap_init: Option<Bytes> = None;
    let mut init_acc: Vec<u8> = Vec::new();
    let mut cap_minute: Option<String> = None;
    let mut cap_ts = Utc::now();
    let mut cap_buf: Vec<u8> = Vec::new();

    while let Some(msg) = socket.recv().await {
        match msg {
            Ok(WsMessage::Binary(b)) => {
                let now = Utc::now();
                if !started {
                    // First chunk: announce the source now so observers connected
                    // from the start receive the init segment in order as live
                    // frames. The late-joiner cache holds this provisional init
                    // until the full segment is assembled (refined below).
                    started = true;
                    cap_ts = now;
                    cap_minute = Some(now.format("%Y-%m-%dT%H:%M").to_string());
                    state.video_in_live.lock().unwrap().insert(
                        scene.clone(),
                        VideoSource { turn, mime: mime.clone(), init: b.clone() },
                    );
                    let _ = state.video_in.send(VideoInEvent::Start {
                        scene: Some(scene.clone()),
                        turn,
                        mime: mime.clone(),
                    });
                }

                if cap_init.is_none() {
                    // Still delimiting the init segment. Accumulate; once the full
                    // segment is present, split it off — the leading bytes are the
                    // cached init, the remainder is the opening minute's first media.
                    init_acc.extend_from_slice(&b);
                    if let Some(n) = init_segment_len(&mime, &init_acc) {
                        let init = Bytes::copy_from_slice(&init_acc[..n]);
                        cap_buf.extend_from_slice(&init_acc[n..]);
                        init_acc = Vec::new();
                        // Refine the late-joiner cache to the *full* init segment.
                        if let Some(src) = state.video_in_live.lock().unwrap().get_mut(&scene)
                            && src.turn == turn
                        {
                            src.init = init.clone();
                        }
                        cap_init = Some(init);
                    }
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

/// Byte length of the initialization segment at the front of `buf`, or `None`
/// if `buf` doesn't yet hold the whole segment (the caller accumulates more).
///
/// The init segment is the decoder/track config that must prefix every persisted
/// minute file (and every late-joining observer's stream) for it to decode. It
/// can straddle WS-chunk boundaries, so we delimit it by container structure
/// rather than by chunk granularity — the bug that left every fragmented-MP4
/// minute file but the first missing its `moov`.
fn init_segment_len(mime: &str, buf: &[u8]) -> Option<usize> {
    match mime.split(';').next().unwrap_or("").trim().to_ascii_lowercase().as_str() {
        // Fragmented MP4: init is `ftyp`+`moov` — everything before the first
        // media fragment (`moof`).
        "video/mp4" => mp4_init_len(buf),
        // WebM (the default, and the fallback for anything else): init is the
        // EBML/Segment header — everything before the first `Cluster`.
        _ => webm_init_len(buf),
    }
}

/// Offset of the first `moof` box in a fragmented-MP4 byte stream, i.e. the
/// length of the `ftyp`+`moov` init segment. `None` until enough bytes are
/// buffered to reach the `moof` (or on a malformed box).
fn mp4_init_len(buf: &[u8]) -> Option<usize> {
    let mut i = 0usize;
    while i + 8 <= buf.len() {
        let size32 = u32::from_be_bytes([buf[i], buf[i + 1], buf[i + 2], buf[i + 3]]) as usize;
        if &buf[i + 4..i + 8] == b"moof" {
            return Some(i);
        }
        let size = match size32 {
            // 64-bit `largesize` in the 8 bytes after the type.
            1 => {
                if i + 16 > buf.len() {
                    return None;
                }
                u64::from_be_bytes(buf[i + 8..i + 16].try_into().ok()?) as usize
            }
            // 0 means "to end of file" — an init box never uses it; bail.
            0 => return None,
            n => n,
        };
        if size < 8 {
            return None; // malformed box header
        }
        i = i.checked_add(size)?;
    }
    None
}

/// Offset of the first `Cluster` element (id `1F 43 B6 75`) in a WebM byte
/// stream, i.e. the length of the EBML/Segment init header. `None` until that
/// id appears in `buf`.
fn webm_init_len(buf: &[u8]) -> Option<usize> {
    buf.windows(4).position(|w| w == [0x1F, 0x43, 0xB6, 0x75])
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

#[cfg(test)]
mod tests {
    use super::*;

    /// Build a minimal MP4 box: 4-byte big-endian size + 4-byte type + body.
    fn box_bytes(typ: &[u8; 4], body: &[u8]) -> Vec<u8> {
        let size = (8 + body.len()) as u32;
        let mut v = size.to_be_bytes().to_vec();
        v.extend_from_slice(typ);
        v.extend_from_slice(body);
        v
    }

    #[test]
    fn mp4_init_ends_at_first_moof() {
        let ftyp = box_bytes(b"ftyp", b"isomhvc1");
        let moov = box_bytes(b"moov", &[0u8; 200]);
        let moof = box_bytes(b"moof", &[0u8; 50]);
        let mut stream = Vec::new();
        stream.extend_from_slice(&ftyp);
        stream.extend_from_slice(&moov);
        stream.extend_from_slice(&moof);
        let want = ftyp.len() + moov.len();
        assert_eq!(init_segment_len("video/mp4", &stream), Some(want));
    }

    #[test]
    fn mp4_init_incomplete_when_moov_not_yet_buffered() {
        // ftyp present but moov box only partially received → can't reach moof.
        let ftyp = box_bytes(b"ftyp", b"isomhvc1");
        let mut moov = box_bytes(b"moov", &[0u8; 200]);
        moov.truncate(moov.len() - 50);
        let mut stream = ftyp.clone();
        stream.extend_from_slice(&moov);
        assert_eq!(init_segment_len("video/mp4", &stream), None);
    }

    #[test]
    fn mp4_init_spanning_two_chunks_resolves_after_concat() {
        // Mirrors the MediaRecorder split: chunk0 = ftyp, chunk1 = moov + moof.
        let ftyp = box_bytes(b"ftyp", b"isomhvc1");
        let moov = box_bytes(b"moov", &[0u8; 120]);
        let moof = box_bytes(b"moof", &[0u8; 30]);
        let chunk0 = ftyp.clone();
        let mut chunk1 = moov.clone();
        chunk1.extend_from_slice(&moof);
        assert_eq!(init_segment_len("video/mp4", &chunk0), None);
        let mut acc = chunk0.clone();
        acc.extend_from_slice(&chunk1);
        assert_eq!(init_segment_len("video/mp4", &acc), Some(ftyp.len() + moov.len()));
    }

    #[test]
    fn webm_init_ends_at_first_cluster() {
        let mut stream = vec![0x1A, 0x45, 0xDF, 0xA3]; // EBML header id
        stream.extend_from_slice(&[0u8; 40]); // header/segment/tracks payload
        let head = stream.len();
        stream.extend_from_slice(&[0x1F, 0x43, 0xB6, 0x75]); // Cluster id
        stream.extend_from_slice(&[0u8; 20]);
        assert_eq!(init_segment_len("video/webm;codecs=vp8", &stream), Some(head));
    }

    #[test]
    fn webm_init_incomplete_without_cluster() {
        let stream = vec![0x1A, 0x45, 0xDF, 0xA3, 0, 0, 0, 0];
        assert_eq!(init_segment_len("video/webm", &stream), None);
    }
}
