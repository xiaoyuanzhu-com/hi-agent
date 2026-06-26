//! The vision input channel: a live camera, streamed as video.
//!
//! "Vision is video." The camera streams continuously — the client does not
//! pre-sample frames — so the *backend* is the control point for how much it
//! actually looks. Each minute is persisted as a standalone file; understanding it
//! is the agent's call (the `see`/`watch` tools), not an eager caption here. When
//! the face capability is configured one keyframe is decoded out of the minute (via
//! `ffmpeg`) and run through face recognition — a receive-time reflex that lets the
//! camera surface "someone's here" the same way a posted still does. The live
//! `video_in` broadcast itself is not yet frame-sampled in real time.
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
use std::collections::HashSet;
use std::time::{Duration, Instant};

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

use crate::body::capabilities::face;
use crate::foundation::vendors::ffmpeg_frame;
use crate::mind::memory::layout::{MediaSlot, day_key};
use crate::mind::memory::media;
use crate::mind::memory::people_vectors::{self, Modality};
use crate::foundation::server::headers::{AuthBearer, RequiredScene, SceneHeader};
use crate::foundation::server::{AppState, FacePresence, PartialMinute, VideoInEvent, VideoSource};
use crate::types::{Channel, JournalEntry, Media, Origin, Scene, Signal};

const DEFAULT_IMAGE_MIME: &str = "image/jpeg";
const DEFAULT_VIDEO_MIME: &str = "video/webm";

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

    // A one-off still: persist the bytes, then raise a minimal signal pointing at
    // them (a `see`-able ref) — understanding is the agent's call, not an eager
    // caption. Perception runs in the background so the POST returns promptly; the
    // journaled signal lands a moment later.
    let ts = Utc::now();
    let rel = match media::store_blob(&state.data_dir, &scene, Channel::Vision, ts, MediaSlot::InputOneOff, ext, &body).await {
        Ok(rel) => rel,
        Err(err) => {
            tracing::error!(error = %err, "failed to persist vision frame");
            return (StatusCode::INTERNAL_SERVER_ERROR, "vision store failed\n").into_response();
        }
    };
    // Keep the raw bytes for the receive-time face reflex; nothing is captioned.
    let recognise = FaceSource::Image(body);
    spawn_perceive(state.clone(), scene.clone(), Perceived::Still, rel, mime, ts, None, Some(recognise));
    StatusCode::ACCEPTED.into_response()
}

/// The stream label presence signals carry, so they render as `vision#presence` in
/// the transcript — set apart from the minute-grid `vision` signals.
const PRESENCE_STREAM: &str = "presence";

/// Internal label for an unrecognized face. The empty string can never be a real
/// subject (enrollment requires a non-empty slug), so it is a collision-proof key
/// for the "someone I don't know" bucket; all strangers collapse onto it.
const STRANGER: &str = "";

/// How long a known label may go unseen before the presence lane calls it gone.
/// With a ~2.5s still cadence this absorbs a few dropped detections (a turned head,
/// a missed frame) so presence doesn't flap appear/leave on flicker.
const PRESENCE_LEAVE_GRACE_SECS: i64 = 8;

/// `POST /api/in/vision/presence` — one low-res camera still from the always-on
/// presence lane. Recognize faces on the *local* models and, only when *who is
/// present* changes, journal a perception signal and wake the reactor — so the live
/// mind learns in real time that someone is on camera (and who, when known) instead
/// of waiting on the minute grid. Cheaply a no-op (202) without the face capability,
/// on an undecodable frame, or when nothing changed. Stills are never persisted —
/// this lane is a reflex, not a record; the full video stream remains the archive.
pub async fn post_presence(
    State(state): State<Arc<AppState>>,
    SceneHeader(scene): SceneHeader,
    body: Bytes,
) -> StatusCode {
    // No models or no body → accept and drop without spending a decode. The client
    // keeps posting harmlessly; we just don't look.
    if body.is_empty() || !face::available() {
        return StatusCode::ACCEPTED;
    }

    let faces = match face::detect_and_embed(body).await {
        Ok(f) => f,
        Err(err) => {
            tracing::debug!(scene = %scene, error = %err, "presence: face detect failed");
            return StatusCode::ACCEPTED;
        }
    };

    // Map each salient face to a stable identity label: a recognized cluster's
    // subject, or the stranger bucket. Strangers all collapse onto one key — "there
    // is someone here I don't know" is the useful signal; telling two unknowns apart
    // needs clustering, which reflection does later.
    let mut seen_now: HashSet<String> = HashSet::new();
    for f in faces.iter().filter(|f| presence_salient(f)) {
        let label = people_vectors::nearest(&state.data_dir, Modality::Face, &f.embedding, 1)
            .await
            .ok()
            .and_then(|c| c.into_iter().next())
            .filter(|c| c.similarity >= RECOGNISE_MIN)
            .map(|c| c.subject)
            .unwrap_or_else(|| STRANGER.to_string());
        seen_now.insert(label);
    }

    let now = Utc::now();
    let grace = chrono::Duration::seconds(PRESENCE_LEAVE_GRACE_SECS);
    // Diff under the lock (sync, no await held across it), then journal/wake outside.
    let (appeared, left) = {
        let mut map = state.face_presence.lock().expect("face_presence mutex poisoned");
        let entry = map.entry(scene.clone()).or_default();
        presence_delta(entry, &seen_now, now, grace)
    };

    if appeared.is_empty() && left.is_empty() {
        return StatusCode::ACCEPTED;
    }

    let body_text = presence_body(&appeared, &left);
    crate::foundation::channel_log::inbound(Channel::Vision, &scene, &body_text);

    let entry = JournalEntry::SignalIn {
        id: Uuid::now_v7().to_string(),
        ts: now,
        channel: Channel::Vision,
        scene: scene.clone(),
        body: body_text.clone(),
        stream: Some(PRESENCE_STREAM.to_string()),
        media: None,
        origin: Some(Origin::Human),
    };
    if let Err(err) = state.memory.journal.append(entry).await {
        tracing::warn!(scene = %scene, error = %err, "presence: journal append failed");
    }

    // The wake: journaling alone updates disk; the reactor only re-reads on a nudge.
    let signal = Signal {
        channel: Channel::Vision,
        scene: scene.clone(),
        body: body_text,
        stream: Some(PRESENCE_STREAM.to_string()),
        ts: now,
    };
    if let Err(err) = state.inbound.send(signal).await {
        tracing::error!(scene = %scene, error = %err, "presence: inbound channel closed");
    }

    StatusCode::ACCEPTED
}

/// A salient presence face: confident enough, and big enough in the (downscaled)
/// still to embed reliably. The box floor is looser than the reflection clusterer's
/// because the presence still is ~640px wide, so a face is fewer pixels.
fn presence_salient(f: &face::DetectedFace) -> bool {
    let w = (f.bbox[2] - f.bbox[0]).max(0.0);
    let h = (f.bbox[3] - f.bbox[1]).max(0.0);
    f.score >= 0.6 && w >= 36.0 && h >= 36.0
}

/// Fold this frame's observed labels into a scene's presence state and return the
/// `(appeared, left)` labels that changed. Hysteresis: a label counts as present
/// from when first seen until `grace` elapses with no sighting, so a dropped
/// detection doesn't flap a leave. `announced` is the set currently treated as
/// on-camera, so each transition fires exactly once. Pure (no IO); the caller holds
/// the lock and journals the result.
fn presence_delta(
    state: &mut FacePresence,
    seen_now: &HashSet<String>,
    now: DateTime<Utc>,
    grace: chrono::Duration,
) -> (Vec<String>, Vec<String>) {
    for l in seen_now {
        state.last_seen.insert(l.clone(), now);
    }
    let mut appeared: Vec<String> = seen_now
        .iter()
        .filter(|l| !state.announced.contains(*l))
        .cloned()
        .collect();
    for l in &appeared {
        state.announced.insert(l.clone());
    }
    // Snapshot the announced set first (owned), so the staleness filter borrows
    // only `last_seen` — no overlapping borrow of two fields of `state`.
    let announced: Vec<String> = state.announced.iter().cloned().collect();
    let mut left: Vec<String> = announced
        .into_iter()
        .filter(|l| {
            state
                .last_seen
                .get(l)
                .is_none_or(|&t| now.signed_duration_since(t) > grace)
        })
        .collect();
    for l in &left {
        state.announced.remove(l);
        state.last_seen.remove(l);
    }
    appeared.sort();
    left.sort();
    (appeared, left)
}

/// One natural-language line for a presence change, e.g. "赵力 appeared on camera."
/// or "someone you don't recognize appeared on camera; 老王 left the camera." Soft
/// evidence for the mind, not a verdict.
fn presence_body(appeared: &[String], left: &[String]) -> String {
    let render = |ls: &[String]| -> String {
        human_join(&ls.iter().map(|l| presence_display(l)).collect::<Vec<_>>())
    };
    let mut parts: Vec<String> = Vec::new();
    if !appeared.is_empty() {
        parts.push(format!("{} appeared on camera", render(appeared)));
    }
    if !left.is_empty() {
        parts.push(format!("{} left the camera", render(left)));
    }
    format!("{}.", parts.join("; "))
}

/// Render an identity label for the presence body. A real name shows as-is; a raw
/// cluster id (8 base-36 chars minted by [`people_vectors::mint_id`], a face seen
/// before but not yet named) reads as a generic "a familiar face" so an opaque id
/// never leaks into the agent's perception; the stranger bucket reads as "someone
/// you don't recognize".
fn presence_display(label: &str) -> String {
    if label == STRANGER {
        "someone you don't recognize".to_string()
    } else if looks_like_cluster_id(label) {
        "a familiar face".to_string()
    } else {
        label.to_string()
    }
}

/// Whether `s` looks like a freshly-minted, still-unnamed cluster id from
/// [`people_vectors::mint_id`] — 8 base-36 lowercase chars with at least one digit.
/// Random ids almost always carry a digit; an all-letters name like "samantha"
/// doesn't, so it isn't mistaken for an id.
fn looks_like_cluster_id(s: &str) -> bool {
    s.len() == 8
        && s.bytes().all(|b| b.is_ascii_digit() || b.is_ascii_lowercase())
        && s.bytes().any(|b| b.is_ascii_digit())
}

/// Join labels into a clause: "a", "a and b", "a, b, and c".
fn human_join(items: &[String]) -> String {
    match items {
        [] => String::new(),
        [a] => a.clone(),
        [a, b] => format!("{a} and {b}"),
        [rest @ .., last] => format!("{}, and {last}", rest.join(", ")),
    }
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

/// What kind of vision media was perceived — shapes the signal we raise.
enum Perceived {
    /// A one-off still the person handed in (POST /api/in/vision): always worth a
    /// signal, carrying a `see`-able ref.
    Still,
    /// A wall-clock minute of the live camera. Ambient: a signal is raised only when
    /// someone is on camera (the face reflex); a quiet minute is kept silently.
    CameraMinute,
}

/// Persist-and-perceive's journaling half: raise a minimal `Vision` signal pointing
/// at the stored blob — NOT an eager caption. Understanding is the agent's call (the
/// `see`/`watch` tools), guided by prose; here we only earn its attention. Face
/// recognition stays a receive-time reflex (soft evidence) folded into the signal.
/// Runs detached so capture never blocks on the (possibly slow) keyframe decode.
fn spawn_perceive(
    state: Arc<AppState>,
    scene: Scene,
    kind: Perceived,
    blob_rel: String,
    mime: String,
    ts: DateTime<Utc>,
    duration_ms: Option<u64>,
    recognise: Option<FaceSource>,
) {
    tokio::spawn(async move {
        // Receive-time face reflex (soft evidence): best-effort, never blocks/fails.
        // Works for a still and for a camera minute (one keyframe decoded out).
        let face = if face::available()
            && let Some(src) = recognise
            && let Some(img) = src.into_image().await
        {
            face_note(img, &state.data_dir).await
        } else {
            None
        };

        let body = match kind {
            Perceived::Still => {
                let mut b = format!("📷 photo arrived ⟨ref: {}/{}⟩", day_key(ts), blob_rel);
                if let Some(note) = &face {
                    b.push_str(note);
                }
                b
            }
            // Ambient camera: only surface a minute when someone's on it. A quiet
            // minute stays on disk but raises no signal (no per-minute spam).
            Perceived::CameraMinute => match &face {
                Some(note) => format!("👁 someone's on camera{note}"),
                None => return,
            },
        };

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

/// Persist one wall-clock minute of camera media as
/// `vision/<date>/<HH>/<MM>.<ext>` (ext follows the stream's container —
/// `mp4`/`webm`), prefixed with the init segment so the file decodes standalone,
/// then perceive it (a face-gated ambient signal; no caption). Best-effort: a store
/// failure is logged and perception is skipped.
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
    // on — the camera's twin of the still-image path, and the gate that decides
    // whether this ambient minute is worth a signal. Decoding happens inside the
    // detached perceive task, so the flush itself stays cheap.
    let recognise = face::available().then(move || FaceSource::Video(video));
    spawn_perceive(state.clone(), scene.clone(), Perceived::CameraMinute, rel, mime.to_string(), ts, None, recognise);
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
    // Throttle for refreshing the shared partial-minute snapshot (`watch`'s freshness
    // window): cloning the growing minute buffer every chunk would be wasteful.
    let mut last_partial = Instant::now();

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
                    // Refresh the freshness snapshot the `watch` tool reads — init +
                    // buf is an independently-decodable clip. Throttled so we don't
                    // clone the growing minute buffer on every chunk.
                    if let Some(init) = cap_init.as_ref()
                        && last_partial.elapsed() >= Duration::from_secs(1)
                    {
                        last_partial = Instant::now();
                        state.video_in_partial.lock().unwrap().insert(
                            scene.clone(),
                            PartialMinute {
                                turn,
                                mime: mime.clone(),
                                init: init.clone(),
                                buf: Bytes::copy_from_slice(&cap_buf),
                            },
                        );
                    }
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
        // Drop the freshness snapshot too, if we're still its owner.
        let mut partial = state.video_in_partial.lock().unwrap();
        if partial.get(&scene).map(|p| p.turn) == Some(turn) {
            partial.remove(&scene);
        }
        drop(partial);
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
/// long-poll. The visual twin of [`crate::foundation::server::audio::get_in_audio`], with a
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

    fn seen(labels: &[&str]) -> HashSet<String> {
        labels.iter().map(|s| s.to_string()).collect()
    }

    #[test]
    fn presence_first_sighting_appears_once_then_quiet() {
        let mut st = FacePresence::default();
        let now = Utc::now();
        let g = chrono::Duration::seconds(8);
        let (app, left) = presence_delta(&mut st, &seen(&["赵力"]), now, g);
        assert_eq!(app, vec!["赵力".to_string()]);
        assert!(left.is_empty());
        // Same face two seconds later → no repeat event.
        let (app2, left2) = presence_delta(&mut st, &seen(&["赵力"]), now + chrono::Duration::seconds(2), g);
        assert!(app2.is_empty() && left2.is_empty());
    }

    #[test]
    fn presence_flicker_within_grace_does_not_leave() {
        let mut st = FacePresence::default();
        let now = Utc::now();
        let g = chrono::Duration::seconds(8);
        presence_delta(&mut st, &seen(&["a"]), now, g);
        // A single missed frame 3s later, still within grace → no leave.
        let (app, left) = presence_delta(&mut st, &HashSet::new(), now + chrono::Duration::seconds(3), g);
        assert!(app.is_empty() && left.is_empty());
    }

    #[test]
    fn presence_leaves_after_grace_then_reappears_fresh() {
        let mut st = FacePresence::default();
        let now = Utc::now();
        let g = chrono::Duration::seconds(8);
        presence_delta(&mut st, &seen(&["a"]), now, g);
        let (app, left) = presence_delta(&mut st, &HashSet::new(), now + chrono::Duration::seconds(20), g);
        assert!(app.is_empty());
        assert_eq!(left, vec!["a".to_string()]);
        // Forgotten after leaving — a later sighting is a fresh appear.
        let (app2, _) = presence_delta(&mut st, &seen(&["a"]), now + chrono::Duration::seconds(25), g);
        assert_eq!(app2, vec!["a".to_string()]);
    }

    #[test]
    fn presence_body_reads_naturally() {
        assert_eq!(presence_body(&["赵力".to_string()], &[]), "赵力 appeared on camera.");
        assert_eq!(
            presence_body(&[STRANGER.to_string()], &[]),
            "someone you don't recognize appeared on camera."
        );
        assert_eq!(
            presence_body(&["ff32ce3w".to_string()], &["赵力".to_string()]),
            "a familiar face appeared on camera; 赵力 left the camera."
        );
    }

    #[test]
    fn cluster_id_vs_name_detection() {
        assert!(looks_like_cluster_id("ff32ce3w")); // minted id (has digits)
        assert!(!looks_like_cluster_id("samantha")); // 8 letters, no digit → a name
        assert!(!looks_like_cluster_id("赵力"));
        assert!(!looks_like_cluster_id("alice"));
        assert!(!looks_like_cluster_id(STRANGER));
    }

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
