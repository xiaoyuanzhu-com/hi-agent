//! `GET /api/out/attention` — the desktop attention overlay's event stream.
//!
//! The ⌘ attention gestures (double-tap → glance, press-hold → mic) drive a
//! native menu-bar overlay: a transparent, always-on-top `WKWebView` that loads
//! `/attention` and subscribes here for the events that animate it — a glance
//! blink, the listen-start "hi" fly-split, the live transcript + streaming reply,
//! and the listen-stop rejoin.
//!
//! Unlike the per-scene `observe` streams, attention is the desktop's single
//! attention surface — there is one ⌘ session at a time — so this stream is **not
//! scene-scoped**; the producer (the gesture bridge in [`crate::body::gesture`])
//! emits already-resolved events. Presence, not a log: broadcast, lossy, no
//! replay (matches [`crate::foundation::server::InputEcho`]).

use std::sync::Arc;

use axum::body::{Body, Bytes};
use axum::extract::State;
use axum::http::{StatusCode, header};
use axum::response::Response;
use futures::stream::unfold;
use tokio::sync::broadcast::error::RecvError;

use crate::foundation::server::AppState;

/// One event animating the attention overlay, serialized as a tagged object —
/// e.g. `{"kind":"transcript","text":"…","final":false}`.
#[derive(Debug, Clone, serde::Serialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum AttentionEvent {
    /// Double-tap ⌘ — a quick glance at the screen. The overlay blooms + blinks
    /// the "hi" mark, no expansion.
    Glance,
    /// A press-hold crossed the hold threshold — the mic is live. The overlay
    /// splits the "hi": the red `h` flies left (the human), the blue `i` stays
    /// right (the AI), divided by the notch.
    ListenStart,
    /// A rolling (`final == false`) or settled (`final == true`) transcript of
    /// the user's speech during a hold — fills the user (left) side.
    Transcript {
        text: String,
        #[serde(rename = "final")]
        is_final: bool,
    },
    /// A chunk of the agent's streaming reply during a hold — fills the agent
    /// (right) side.
    Reply { text: String },
    /// The hold ended (mic closed) — the overlay rejoins the "hi" and collapses.
    ListenStop,
}

/// `GET /api/out/attention` — stream attention events as NDJSON (one JSON object
/// per line) for as long as the client holds the connection. A lossy live feed:
/// a reader that falls behind resumes from the live edge (events are presence we
/// don't replay), and a fresh GET simply re-subscribes.
pub async fn get_out_attention(State(state): State<Arc<AppState>>) -> Response {
    let rx = state.attention_out.subscribe();

    let stream = unfold(rx, |mut rx| async move {
        loop {
            match rx.recv().await {
                Ok(ev) => {
                    // One JSON object per line. Serialization can't fail for this
                    // shape; on the off chance it does, skip the frame.
                    let Ok(mut line) = serde_json::to_vec(&ev) else {
                        continue;
                    };
                    line.push(b'\n');
                    return Some((Ok::<Bytes, std::convert::Infallible>(Bytes::from(line)), rx));
                }
                Err(RecvError::Lagged(n)) => {
                    tracing::warn!(missed = n, "attention observer lagged");
                    continue;
                }
                Err(RecvError::Closed) => return None,
            }
        }
    });

    Response::builder()
        .status(StatusCode::OK)
        .header(header::CONTENT_TYPE, "application/x-ndjson; charset=utf-8")
        .body(Body::from_stream(stream))
        .unwrap()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn event_serialization_shapes() {
        let s = |e: &AttentionEvent| serde_json::to_string(e).unwrap();
        assert_eq!(s(&AttentionEvent::Glance), r#"{"kind":"glance"}"#);
        assert_eq!(s(&AttentionEvent::ListenStart), r#"{"kind":"listen_start"}"#);
        assert_eq!(s(&AttentionEvent::ListenStop), r#"{"kind":"listen_stop"}"#);
        assert_eq!(
            s(&AttentionEvent::Transcript { text: "你好".into(), is_final: false }),
            r#"{"kind":"transcript","text":"你好","final":false}"#
        );
        assert_eq!(
            s(&AttentionEvent::Reply { text: "好的".into() }),
            r#"{"kind":"reply","text":"好的"}"#
        );
    }
}
