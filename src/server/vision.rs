//! POST /api/vision — inbound vision channel; GET /api/vision — its read side.
//!
//! Vision is a *continuous* input channel: the client captures frames from the
//! camera and POSTs them here, the same way the mic feeds /api/audio. There is
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
//!    party, not just the reactor. `GET /api/vision` mirrors `GET /api/surface`:
//!    one frame per scene-filtered long-poll response. A detector working
//!    session polls it, runs CV on the raw bytes, and drives the overlay channel
//!    — the perception is *its* job, not the host's.

use std::sync::Arc;

use axum::body::Bytes;
use axum::extract::State;
use axum::http::header::CONTENT_TYPE;
use axum::http::{HeaderMap, HeaderValue, StatusCode};
use axum::response::IntoResponse;
use chrono::Utc;
use tokio::sync::broadcast::error::RecvError;

use crate::memory::media::{self, Direction};
use crate::server::headers::{AuthBearer, RequiredScene, SceneHeader};
use crate::server::{AppState, VisionFrameEvent};

const DEFAULT_MIME: &str = "image/jpeg";

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
        .unwrap_or_else(|| DEFAULT_MIME.to_string());
    let ext = mime_to_ext(&mime);

    tracing::debug!(scene = %scene, mime = %mime, bytes = body.len(), "POST /api/vision");

    // Publish the live frame to any subscriber (the read side of the channel).
    // A send error just means nobody is watching — fine, mirrors audio out.
    let _ = state.vision_out.send(VisionFrameEvent {
        scene: Some(scene.clone()),
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

/// `GET /api/vision` — long-poll for the next live frame in this scene.
///
/// Mirrors [`crate::server::surface::get_surface`]: subscribe to `vision_out`,
/// skip frames routed to other scenes, and return the next matching frame's
/// bytes with its `Content-Type`. The subscriber re-GETs for the frame after.
pub async fn get_vision(
    State(state): State<Arc<AppState>>,
    RequiredScene(scene): RequiredScene,
    AuthBearer(auth): AuthBearer,
) -> impl IntoResponse {
    let mut rx = state.vision_out.subscribe();

    tracing::info!(scene = %scene, auth = ?auth, "GET /vision long-poll opened");

    loop {
        match rx.recv().await {
            Ok(event) => {
                let deliver = match &event.scene {
                    None => true,
                    Some(target) => target == &scene,
                };
                if !deliver {
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
