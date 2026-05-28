//! POST /thought and GET /thought.
//!
//! Long-poll model for GET: subscribe to the outbound broadcast and wait for
//! one matching signal. The response body is the signal body; closing it is
//! the spec's end-of-utterance marker. Step 1 has no producer, so GETs hang
//! until the request is cancelled.

use std::sync::Arc;

use axum::body::Bytes;
use axum::extract::State;
use axum::http::StatusCode;
use axum::response::IntoResponse;
use chrono::Utc;
use tokio::sync::broadcast::error::RecvError;

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

    // Journal-before-dispatch (impl.md: "Reactor writes to journal before
    // invoking the routing session"). A journal write failure is logged but
    // does not 500 the peer — accepting the signal and being slightly behind
    // on disk is preferable to forcing a retry storm.
    let entry = JournalEntry::SignalIn {
        ts: signal.ts,
        channel: signal.channel,
        from: signal.from.clone(),
        body: signal.body.clone(),
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
) -> impl IntoResponse {
    let mut rx = state.thought_out.subscribe();

    tracing::info!(subscriber = ?subscriber, auth = ?auth, "GET /thought long-poll opened");

    loop {
        match rx.recv().await {
            Ok(signal) => {
                // Filter: deliver if the signal is broadcast (no `to`), or
                // addressed to this subscriber, or if the subscriber didn't
                // identify themselves (legacy fan-out).
                let deliver = match (&signal.to, &subscriber) {
                    (None, _) => true,
                    (Some(target), Some(sub)) => target == sub,
                    (Some(_), None) => true,
                };
                if !deliver {
                    continue;
                }
                return (StatusCode::OK, signal.body).into_response();
            }
            Err(RecvError::Lagged(n)) => {
                tracing::warn!(missed = n, "thought subscriber lagged");
                continue;
            }
            Err(RecvError::Closed) => {
                return (StatusCode::SERVICE_UNAVAILABLE, "broadcast closed").into_response();
            }
        }
    }
}
