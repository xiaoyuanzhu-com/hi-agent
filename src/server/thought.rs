//! POST /thought and GET /thought.
//!
//! GET /thought is a long-poll. The handler binds to the next buffered
//! utterance for the subscriber's peer, holds the connection open, and streams
//! each chunk into the response body until the utterance completes. Closing the
//! body is the spec's "end of utterance".
//!
//! Per the spec, a fresh GET re-subscribes for the next utterance. Because the
//! [`ThoughtBus`](crate::server::ThoughtBus) buffers per peer, a reply produced
//! between polls (or before the first poll) is retained for the next GET rather
//! than lost — see that module for the race this fixes.

use std::sync::Arc;

use axum::body::{Body, Bytes};
use axum::extract::State;
use axum::http::{StatusCode, header};
use axum::response::{IntoResponse, Response};
use chrono::Utc;

use crate::server::AppState;
use crate::server::headers::{AuthBearer, PeerHeader, ToHeader};
use crate::types::{Channel, JournalEntry, Signal};

pub async fn post_thought(
    State(state): State<Arc<AppState>>,
    PeerHeader(from): PeerHeader,
    ToHeader(to): ToHeader,
    AuthBearer(auth): AuthBearer,
    body: Bytes,
) -> impl IntoResponse {
    let body_str = match std::str::from_utf8(&body) {
        Ok(s) => s.to_owned(),
        Err(_) => {
            return (StatusCode::BAD_REQUEST, "thought body must be utf-8").into_response();
        }
    };

    let signal = Signal {
        channel: Channel::Thought,
        from: from.clone(),
        to: to.clone(),
        body: body_str,
        ts: Utc::now(),
    };

    tracing::info!(
        from = %from,
        to = ?to,
        auth = ?auth,
        len = signal.body.len(),
        "POST /thought"
    );
    crate::channel_log::inbound(Channel::Thought, &from, &signal.body);

    let entry = JournalEntry::SignalIn {
        ts: signal.ts,
        channel: signal.channel,
        from: signal.from.clone(),
        body: signal.body.clone(),
        media_path: None,
    };
    if let Err(err) = state.memory.journal.append(entry).await {
        tracing::error!(error = %err, "journal append failed; accepting signal anyway");
    }

    if let Err(err) = state.inbound.send(signal).await {
        tracing::error!(error = %err, "inbound channel closed");
        return (StatusCode::SERVICE_UNAVAILABLE, "inbound channel closed").into_response();
    }

    StatusCode::ACCEPTED.into_response()
}

pub async fn get_thought(
    State(state): State<Arc<AppState>>,
    ToHeader(subscriber): ToHeader,
    AuthBearer(auth): AuthBearer,
) -> Response {
    // A reader drains one peer's mailbox, so it must say who it is. The spec
    // has the subscriber identify themselves via X-HI-To; without it we can't
    // route the buffered reply.
    let Some(peer) = subscriber else {
        return (
            StatusCode::BAD_REQUEST,
            "GET /thought requires X-HI-To to name the subscribing peer\n",
        )
            .into_response();
    };

    tracing::info!(subscriber = %peer, auth = ?auth, "GET /thought long-poll opened");

    let stream = state.thought_bus.subscribe(peer);

    Response::builder()
        .status(StatusCode::OK)
        .header(header::CONTENT_TYPE, "text/plain; charset=utf-8")
        .body(Body::from_stream(stream))
        .unwrap()
}
