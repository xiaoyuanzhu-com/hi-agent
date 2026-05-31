//! POST /api/vision — inbound vision channel.
//!
//! Vision is a *continuous* input channel: the client captures frames from the
//! camera and POSTs them here, the same way the mic feeds /api/audio. There is
//! no commit on the client — it just streams the signal; what (if anything) to
//! do with it is the mind's call.
//!
//! For now the mind has no way to *perceive* an image (cognition is text-only —
//! audio works because STT turns it into text). So we deliberately do the
//! minimum that makes the channel real: persist the frame to disk and ack. We
//! do **not** journal or dispatch it — journaling a frame every few seconds
//! would flood the per-peer snapshot ([`crate::memory::snapshot`], a 30-min /
//! 200-entry window) and crowd out the actual conversation, all for bytes the
//! agent can't read yet. When a perception path exists (captioning or a
//! multimodal prompt), this is where it slots in.

use std::sync::Arc;

use axum::body::Bytes;
use axum::extract::State;
use axum::http::header::CONTENT_TYPE;
use axum::http::{HeaderMap, StatusCode};
use axum::response::IntoResponse;

use crate::memory::media::{self, Direction};
use crate::server::AppState;
use crate::server::headers::{PeerHeader, ToHeader};

const DEFAULT_MIME: &str = "image/jpeg";

pub async fn post_vision(
    State(state): State<Arc<AppState>>,
    PeerHeader(from): PeerHeader,
    ToHeader(to): ToHeader,
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

    tracing::debug!(from = %from, to = ?to, mime = %mime, bytes = body.len(), "POST /api/vision");

    // Persist only — no journal, no dispatch (see module docs).
    match media::store_image(&state.data_dir, Direction::In, ext, &body).await {
        Ok(_path) => StatusCode::ACCEPTED.into_response(),
        Err(err) => {
            tracing::error!(error = %err, "failed to persist vision frame");
            (StatusCode::INTERNAL_SERVER_ERROR, "vision store failed\n").into_response()
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
