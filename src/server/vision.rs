//! POST /api/in/vision — inbound vision channel; GET /api/in/vision — its read side.
//!
//! Vision is a *continuous* input channel: the client captures frames from the
//! camera and POSTs them here, the same way the mic feeds /api/in/audio. There is
//! no commit on the client — it just streams the signal; what (if anything) to
//! do with it is the mind's call.
//!
//! Two things happen to a posted frame, both deliberately *outside* the
//! cognition turn loop:
//!
//! 1. **Persist** to disk and ack. We do **not** journal or dispatch it into
//!    cognition — journaling a frame every few seconds would flood the per-scene
//!    snapshot ([`crate::memory::snapshot`], a 30-min / 200-entry window) and
//!    crowd out the actual conversation, all for bytes the text-only mind can't
//!    read yet. When a perception path exists (captioning or a multimodal
//!    prompt), this is where it slots in.
//! 2. **Broadcast** on `vision_out` so the channel is readable by any local
//!    party, not just the reactor. `GET /api/in/vision` mirrors `GET /api/out/surface`:
//!    one frame per scene-filtered long-poll response. A detector working
//!    session polls it, runs CV on the raw bytes, and drives the overlay channel
//!    — the perception is *its* job, not the host's.

use std::sync::Arc;

use axum::body::Bytes;
use axum::extract::{Query, State};
use axum::http::header::CONTENT_TYPE;
use axum::http::{HeaderMap, HeaderValue, StatusCode};
use axum::response::IntoResponse;
use chrono::Utc;
use serde::Deserialize;
use tokio::sync::broadcast::error::RecvError;

use crate::memory::media::{self, Direction};
use crate::server::headers::{AuthBearer, RequiredScene, SceneHeader, StreamHeader};
use crate::server::{AppState, VisionFrameEvent};

const DEFAULT_MIME: &str = "image/jpeg";

pub async fn post_vision(
    State(state): State<Arc<AppState>>,
    SceneHeader(scene): SceneHeader,
    StreamHeader(stream): StreamHeader,
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
        .unwrap_or_else(|| DEFAULT_MIME.to_string());
    let ext = mime_to_ext(&mime);

    tracing::debug!(scene = %scene, stream = ?stream, mime = %mime, bytes = body.len(), "POST /api/in/vision");

    // Publish the live frame to any subscriber (the read side of the channel).
    // A send error just means nobody is watching — fine, mirrors audio out.
    let _ = state.vision_out.send(VisionFrameEvent {
        scene: Some(scene.clone()),
        stream,
        bytes: body.clone(),
        mime,
        ts: Utc::now(),
    });

    // Persist only — no journal, no dispatch (see module docs).
    match media::store_image(&state.data_dir, Direction::In, ext, &body).await {
        Ok(_path) => StatusCode::ACCEPTED.into_response(),
        Err(err) => {
            tracing::error!(error = %err, "failed to persist vision frame");
            (StatusCode::INTERNAL_SERVER_ERROR, "vision store failed\n").into_response()
        }
    }
}

/// Query selector for `GET /api/in/vision`: which stream within the scene to
/// read. Absent or empty → the default stream (`None`), matching a POST that set
/// no `X-HI-Stream`. A named feed is requested with `?stream=webcam`.
#[derive(Debug, Deserialize)]
pub struct StreamSelect {
    stream: Option<String>,
}

/// `GET /api/in/vision` — long-poll for the next live frame in this scene's
/// selected stream.
///
/// Mirrors [`crate::server::view::get_out_view`]: subscribe to `vision_out`,
/// skip frames routed to other scenes or other streams, and return the next
/// matching frame's bytes with its `Content-Type`. The subscriber re-GETs for the
/// frame after.
pub async fn get_vision(
    State(state): State<Arc<AppState>>,
    RequiredScene(scene): RequiredScene,
    Query(select): Query<StreamSelect>,
    AuthBearer(auth): AuthBearer,
) -> impl IntoResponse {
    let want = select.stream.as_deref().map(str::trim).filter(|s| !s.is_empty());
    let mut rx = state.vision_out.subscribe();

    tracing::info!(scene = %scene, stream = ?want, auth = ?auth, "GET /api/in/vision long-poll opened");

    loop {
        match rx.recv().await {
            Ok(event) => {
                let scene_ok = match &event.scene {
                    None => true,
                    Some(target) => target == &scene,
                };
                // A frame belongs to exactly one stream — plain equality, no
                // wildcard. Default GET (`None`) reads only default frames.
                let stream_ok = event.stream.as_deref() == want;
                if !(scene_ok && stream_ok) {
                    continue;
                }
                let mut response = event.bytes.into_response();
                if let Ok(val) = HeaderValue::from_str(&event.mime) {
                    response.headers_mut().insert(CONTENT_TYPE, val);
                }
                return response;
            }
            Err(RecvError::Lagged(n)) => {
                tracing::warn!(missed = n, "vision subscriber lagged");
                continue;
            }
            Err(RecvError::Closed) => {
                return (StatusCode::SERVICE_UNAVAILABLE, "broadcast closed\n").into_response();
            }
        }
    }
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
