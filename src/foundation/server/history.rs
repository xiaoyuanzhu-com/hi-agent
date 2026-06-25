//! `GET /api/history` — recent conversation for a scene, for the chat surface.
//!
//! Seeds the menu-bar chat popup's message list on open. Reads the scene's raw
//! journal ([`crate::mind::memory::Journal::recent`]) — worded replies, spoken
//! transcripts (audio), vision stills — and projects each entry into the small
//! chat-message shape the web bubble renders. Handed files live under `files/`,
//! which `recent` skips (they're artifacts, not signals): a dropped file shows
//! live/optimistic in the composer but isn't re-served here, while everything else
//! persists across reopens. Media bytes are never inlined — each media-bearing
//! entry carries a [`super::media`] URL the bubble loads on demand.

use std::sync::Arc;

use axum::extract::{Query, State};
use axum::http::StatusCode;
use axum::response::{IntoResponse, Response};
use axum::Json;
use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};

use crate::types::{Channel, JournalEntry, Media, Origin, Scene};

use super::AppState;

/// The popup shows a recent tail; the ceiling keeps a long session loadable without
/// an unbounded scan.
const DEFAULT_LIMIT: usize = 100;
const MAX_LIMIT: usize = 500;

#[derive(Deserialize)]
pub struct HistoryQuery {
    scene: String,
    limit: Option<usize>,
    /// Lower bound (RFC 3339); omitted = since the epoch, so the cap takes the most
    /// recent `limit`.
    since: Option<DateTime<Utc>>,
}

#[derive(Serialize)]
struct Msg {
    id: String,
    ts: DateTime<Utc>,
    /// `in` = user→agent (right bubble); `out` = agent→user (left bubble).
    dir: &'static str,
    channel: &'static str,
    #[serde(skip_serializing_if = "Option::is_none")]
    origin: Option<Origin>,
    body: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    media: Option<MsgMedia>,
}

#[derive(Serialize)]
struct MsgMedia {
    /// A `/api/media` URL that resolves the blob — bytes are not inlined.
    url: String,
    mime: String,
    /// `image` | `audio` | `video` | `file` — picks the bubble renderer.
    kind: &'static str,
    #[serde(skip_serializing_if = "Option::is_none")]
    width: Option<u32>,
    #[serde(skip_serializing_if = "Option::is_none")]
    height: Option<u32>,
    #[serde(skip_serializing_if = "Option::is_none")]
    duration_ms: Option<u64>,
}

pub async fn get_history(
    State(state): State<Arc<AppState>>,
    Query(q): Query<HistoryQuery>,
) -> Response {
    if q.scene.is_empty() {
        return (StatusCode::BAD_REQUEST, "scene is required").into_response();
    }
    let scene = Scene(q.scene);
    let limit = q.limit.unwrap_or(DEFAULT_LIMIT).clamp(1, MAX_LIMIT);
    let since = q
        .since
        .unwrap_or_else(|| DateTime::from_timestamp(0, 0).expect("unix epoch is valid"));

    let entries = match state.memory.journal.recent(Some(&scene), since, limit).await {
        Ok(e) => e,
        Err(err) => {
            tracing::warn!(error = %err, scene = %scene, "history: journal read failed");
            return StatusCode::INTERNAL_SERVER_ERROR.into_response();
        }
    };

    let msgs: Vec<Msg> = entries.iter().map(|e| project(&scene, e)).collect();
    Json(msgs).into_response()
}

/// Project one journal entry into a chat message. `dir` keys the bubble side; any
/// media (audio clip, vision still) becomes a `/api/media` URL the bubble loads.
fn project(scene: &Scene, entry: &JournalEntry) -> Msg {
    match entry {
        JournalEntry::SignalIn { id, ts, channel, body, media, origin, .. } => Msg {
            id: id.clone(),
            ts: *ts,
            dir: "in",
            channel: channel.as_str(),
            origin: *origin,
            body: body.clone(),
            media: media.as_ref().map(|m| project_media(scene, *channel, *ts, m)),
        },
        JournalEntry::SignalOut { id, ts, channel, body, media, origin, .. } => Msg {
            id: id.clone(),
            ts: *ts,
            dir: "out",
            channel: channel.as_str(),
            origin: *origin,
            body: body.clone(),
            media: media.as_ref().map(|m| project_media(scene, *channel, *ts, m)),
        },
    }
}

fn project_media(scene: &Scene, channel: Channel, ts: DateTime<Utc>, m: &Media) -> MsgMedia {
    MsgMedia {
        url: media_url(scene, channel, ts, &m.file, &m.mime),
        mime: m.mime.clone(),
        kind: media_kind(&m.mime),
        width: m.width,
        height: m.height,
        duration_ms: m.duration_ms,
    }
}

fn media_kind(mime: &str) -> &'static str {
    if mime.starts_with("image/") {
        "image"
    } else if mime.starts_with("audio/") {
        "audio"
    } else if mime.starts_with("video/") {
        "video"
    } else {
        "file"
    }
}

/// Build the `/api/media` URL that resolves this blob: `scene` + `channel` + `ts`
/// locate the channel-day folder, `file` is the path within it, `mime` the stored
/// type echoed back as Content-Type. Values are percent-encoded for a query string.
fn media_url(scene: &Scene, channel: Channel, ts: DateTime<Utc>, file: &str, mime: &str) -> String {
    format!(
        "/api/media?scene={}&channel={}&ts={}&file={}&mime={}",
        q(&scene.0),
        channel.as_str(),
        q(&ts.to_rfc3339()),
        q(file),
        q(mime),
    )
}

/// Percent-encode a value for a query string: keep the RFC 3986 unreserved set,
/// `%XX` everything else — so `serde_urlencoded` on the `/api/media` side decodes
/// it back exactly (note space → `%20`, never `+`).
fn q(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    for b in s.bytes() {
        match b {
            b'A'..=b'Z' | b'a'..=b'z' | b'0'..=b'9' | b'-' | b'_' | b'.' | b'~' => {
                out.push(b as char)
            }
            _ => out.push_str(&format!("%{b:02X}")),
        }
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn media_kind_maps_by_mime_prefix() {
        assert_eq!(media_kind("image/png"), "image");
        assert_eq!(media_kind("audio/mpeg"), "audio");
        assert_eq!(media_kind("video/mp4"), "video");
        assert_eq!(media_kind("application/pdf"), "file");
    }

    #[test]
    fn query_encoding_is_urlencoded_safe() {
        // `/`, `:`, `+`, and space must all encode so the media side decodes them back.
        assert_eq!(q("09/16-45.mp3"), "09%2F16-45.mp3");
        assert_eq!(q("2026-06-25T09:16:45+00:00"), "2026-06-25T09%3A16%3A45%2B00%3A00");
        assert_eq!(q("a b"), "a%20b");
        assert_eq!(q("plain.Ext_1~"), "plain.Ext_1~");
    }
}
