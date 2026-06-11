//! The text channel: `POST /api/in/text`, `GET /api/in/text`, `GET /api/out/text`.
//!
//! `POST /api/in/text` is the typed-input path: the body is dispatched to the
//! mind (journalled + queued on `inbound`) and echoed on the per-scene
//! input-echo bus so every client observing `GET /api/in/text` renders the same
//! line — the human's words fan out the way the agent's do.
//!
//! `GET /api/out/text` is a long-poll for the agent's reply. The handler binds
//! to the next buffered utterance for the subscriber's scene, holds the
//! connection open, and streams each chunk into the response body until the
//! utterance completes. Closing the body is the spec's "end of utterance". A
//! fresh GET re-subscribes for the next utterance; because the
//! [`TextBus`](crate::server::TextBus) buffers per scene, a reply produced
//! between polls (or before the first poll) is retained rather than lost — see
//! that module for the race this fixes.
//!
//! `GET /api/in/text` is a live observe stream (see [`crate::server::observe`]):
//! no buffering, just the inputs as they cross the boundary.

use std::sync::Arc;

use axum::body::{Body, Bytes};
use axum::extract::State;
use axum::http::{StatusCode, header};
use axum::response::{IntoResponse, Response};
use chrono::Utc;
use uuid::Uuid;

use futures::StreamExt as _;

use crate::server::observe;
use crate::server::headers::{AuthBearer, RequiredScene, SceneHeader, StreamHeader};
use crate::server::AppState;
use crate::types::{Channel, JournalEntry, Origin, Signal};

pub async fn post_text(
    State(state): State<Arc<AppState>>,
    SceneHeader(scene): SceneHeader,
    StreamHeader(stream): StreamHeader,
    AuthBearer(auth): AuthBearer,
    body: Bytes,
) -> impl IntoResponse {
    let body_str = match std::str::from_utf8(&body) {
        Ok(s) => s.to_owned(),
        Err(_) => {
            return (StatusCode::BAD_REQUEST, "text body must be utf-8").into_response();
        }
    };

    let signal = Signal {
        channel: Channel::Text,
        scene: scene.clone(),
        body: body_str,
        stream,
        ts: Utc::now(),
    };

    tracing::info!(
        scene = %scene,
        auth = ?auth,
        len = signal.body.len(),
        "POST /api/in/text"
    );
    crate::channel_log::inbound(Channel::Text, &scene, &signal.body);

    let entry = JournalEntry::SignalIn {
        id: Uuid::now_v7().to_string(),
        ts: signal.ts,
        channel: signal.channel,
        scene: signal.scene.clone(),
        body: signal.body.clone(),
        stream: signal.stream.clone(),
        media: None,
        origin: Some(Origin::Human),
    };
    if let Err(err) = state.memory.journal.append(entry).await {
        tracing::error!(error = %err, "journal append failed; accepting signal anyway");
    }

    // Echo to scene observers (live, no buffer) before dispatching inward, so a
    // typed line shows on every client just like recognized speech does.
    state.echo_input(&scene, Channel::Text, &signal.body, true);

    if let Err(err) = state.inbound.send(signal).await {
        tracing::error!(error = %err, "inbound channel closed");
        return (StatusCode::SERVICE_UNAVAILABLE, "inbound channel closed").into_response();
    }

    StatusCode::ACCEPTED.into_response()
}

/// `GET /api/out/text` — the agent's reply, one utterance per long-poll.
pub async fn get_out_text(
    State(state): State<Arc<AppState>>,
    RequiredScene(scene): RequiredScene,
    AuthBearer(auth): AuthBearer,
) -> Response {
    tracing::info!(scene = %scene, auth = ?auth, "GET /api/out/text long-poll opened");

    // Opening this long-poll is a scene-presence signal: warm the scene up so its
    // process + session + upstream cache are hot before the first utterance.
    state.warm_scene(&scene);

    // Count this reader as live presence for as long as its body stream exists.
    let presence = state.presence.connect(&scene, crate::presence::OutChannel::Text);
    let stream = state.text_bus.subscribe(scene).map(move |item| {
        let _held = &presence;
        item
    });

    Response::builder()
        .status(StatusCode::OK)
        .header(header::CONTENT_TYPE, "text/plain; charset=utf-8")
        .body(Body::from_stream(stream))
        .unwrap()
}

/// `GET /api/in/text` — observe typed inputs on this scene, live (NDJSON).
pub async fn get_in_text(
    State(state): State<Arc<AppState>>,
    RequiredScene(scene): RequiredScene,
    AuthBearer(auth): AuthBearer,
) -> Response {
    tracing::info!(scene = %scene, auth = ?auth, "GET /api/in/text observe opened");
    observe::stream_input(state, scene, Channel::Text)
}
