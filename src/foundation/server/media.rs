//! `GET /api/media` — serve a journaled media blob (audio clip, vision still) by
//! reference, for the chat surface's bubbles.
//!
//! The chat history ([`super::history`]) hands each media-bearing message a URL
//! into here instead of inlining bytes. The blob is located the same way every
//! reader does — [`crate::mind::memory::media::resolve`], which returns the
//! original bytes or the nearest keepsake a faded day left, scoped to the scene's
//! channel-day folder. Read-only and best-effort: an unknown ref or a faded-away
//! blob is a 404.

use std::sync::Arc;

use axum::body::Body;
use axum::extract::{Query, State};
use axum::http::header::{CACHE_CONTROL, CONTENT_TYPE};
use axum::http::{HeaderValue, StatusCode};
use axum::response::{IntoResponse, Response};
use chrono::{DateTime, Utc};
use serde::Deserialize;

use crate::mind::memory::media;
use crate::types::{Channel, Scene};

use super::AppState;

#[derive(Deserialize)]
pub struct MediaQuery {
    scene: String,
    channel: String,
    ts: DateTime<Utc>,
    /// The entry's `media.file` — a path relative to the channel-day folder.
    file: String,
    /// The stored mime, echoed back as Content-Type (sanitized below).
    mime: Option<String>,
}

pub async fn get_media(
    State(state): State<Arc<AppState>>,
    Query(q): Query<MediaQuery>,
) -> Response {
    // `file` is folder-relative; reject anything that could climb out of the
    // scene's channel-day folder (`resolve` joins it directly).
    if q.file.is_empty()
        || q.file.starts_with('/')
        || q.file.contains('\\')
        || q.file.split('/').any(|seg| seg == "..")
    {
        return StatusCode::BAD_REQUEST.into_response();
    }
    if q.scene.is_empty() {
        return StatusCode::BAD_REQUEST.into_response();
    }
    let Ok(channel) = q.channel.parse::<Channel>() else {
        return StatusCode::BAD_REQUEST.into_response();
    };
    let scene = Scene(q.scene);

    let Some(path) = media::resolve(&state.data_dir, &scene, channel, q.ts, &q.file).await else {
        return StatusCode::NOT_FOUND.into_response();
    };
    let bytes = match tokio::fs::read(&path).await {
        Ok(b) => b,
        Err(err) => {
            tracing::warn!(error = %err, "media: read failed");
            return StatusCode::NOT_FOUND.into_response();
        }
    };

    let ctype = q
        .mime
        .as_deref()
        .filter(|m| is_token_mime(m))
        .and_then(|m| HeaderValue::from_str(m).ok())
        .unwrap_or_else(|| HeaderValue::from_static("application/octet-stream"));

    let mut resp = Response::new(Body::from(bytes));
    resp.headers_mut().insert(CONTENT_TYPE, ctype);
    resp.headers_mut()
        .insert(CACHE_CONTROL, HeaderValue::from_static("private, max-age=300"));
    resp
}

/// Accept a mime only if it's a plain `type/subtype` of token chars — so a crafted
/// `mime=` can't inject a header or smuggle CRLF.
fn is_token_mime(mime: &str) -> bool {
    !mime.is_empty()
        && mime.len() <= 128
        && mime
            .bytes()
            .all(|b| b.is_ascii_alphanumeric() || matches!(b, b'/' | b'.' | b'+' | b'-'))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn rejects_non_token_mimes() {
        assert!(is_token_mime("image/png"));
        assert!(is_token_mime("audio/mpeg"));
        assert!(!is_token_mime("text/html\r\nX-Evil: 1"));
        assert!(!is_token_mime(""));
        assert!(!is_token_mime("image/ png"));
    }
}
