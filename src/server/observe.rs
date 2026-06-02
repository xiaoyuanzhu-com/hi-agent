//! `GET /api/in/<channel>` — observe a scene's recognized inputs, live.
//!
//! Inputs cross the world→agent boundary on a single client's POST/WS, but every
//! client in the scene should render them identically — the same guarantee the
//! outbound channels give. Each handler here subscribes to the per-scene
//! [`InputEcho`](crate::server::InputEcho) broadcast, keeps only this scene +
//! channel, and streams the matches as newline-delimited JSON for as long as the
//! connection is held.
//!
//! This is *presence*, not history: the broadcast is lossy with no replay (see
//! `InputEcho`), so an observer sees inputs from the moment it connects. A
//! single long-lived response is used rather than the one-item-per-GET long-poll
//! the outbound channels use, because partials (live STT) arrive too fast for a
//! reconnect-per-item loop.

use std::sync::Arc;

use axum::body::{Body, Bytes};
use axum::http::{StatusCode, header};
use axum::response::Response;
use futures::stream::unfold;
use tokio::sync::broadcast::error::RecvError;

use crate::server::AppState;
use crate::types::{Channel, Scene};

/// Stream this scene's inputs on `channel` as NDJSON until the client hangs up.
pub fn stream_input(state: Arc<AppState>, scene: Scene, channel: Channel) -> Response {
    let rx = state.input_echo.subscribe();

    let stream = unfold((rx, scene, channel), |(mut rx, scene, channel)| async move {
        loop {
            match rx.recv().await {
                Ok(echo) => {
                    if echo.channel != channel || echo.scene != scene {
                        continue;
                    }
                    // One JSON object per line. Serialization can't fail for this
                    // shape; on the off chance it does, skip the frame.
                    let Ok(mut line) = serde_json::to_vec(&echo) else {
                        continue;
                    };
                    line.push(b'\n');
                    return Some((
                        Ok::<Bytes, std::convert::Infallible>(Bytes::from(line)),
                        (rx, scene, channel),
                    ));
                }
                // A slow observer that fell behind just resumes from the live
                // edge — dropped inputs are presence we don't replay.
                Err(RecvError::Lagged(n)) => {
                    tracing::warn!(missed = n, "input observer lagged");
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
