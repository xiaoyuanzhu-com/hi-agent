//! POST /thought and GET /thought.
//!
//! GET /thought is a long-poll. The handler subscribes to the agent's outbound
//! event stream, holds the connection open until the first matching `Chunk`
//! arrives, then streams every chunk into the response body until the matching
//! `EndOfUtterance` fires. Closing the body is the spec's "end of utterance".
//!
//! Per the spec, a fresh GET re-subscribes for the next utterance.

use std::sync::Arc;

use axum::body::{Body, Bytes};
use axum::extract::State;
use axum::http::{StatusCode, header};
use axum::response::{IntoResponse, Response};
use chrono::Utc;
use futures::stream::unfold;
use serde::{Deserialize, Serialize};
use tokio::sync::broadcast::error::RecvError;

use crate::server::AppState;
use crate::server::headers::{AuthBearer, PeerHeader, ToHeader};
use crate::types::{Channel, JournalEntry, PeerId, Signal};

/// One event on the agent's outbound `/thought` stream.
///
/// The reactor broadcasts `Chunk` for each ACP text delta and `EndOfUtterance`
/// when a routing turn finishes (clean or cancelled). The GET handler turns
/// this into HTTP chunked body + close.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub enum ThoughtEvent {
    Chunk {
        to: Option<PeerId>,
        from: PeerId,
        text: String,
    },
    EndOfUtterance {
        to: Option<PeerId>,
    },
}

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
    let rx = state.thought_out.subscribe();
    tracing::info!(subscriber = ?subscriber, auth = ?auth, "GET /thought long-poll opened");

    let stream = build_utterance_stream(rx, subscriber);

    Response::builder()
        .status(StatusCode::OK)
        .header(header::CONTENT_TYPE, "text/plain; charset=utf-8")
        .body(Body::from_stream(stream))
        .unwrap()
}

/// Convert the broadcast subscription into a `Stream<Result<Bytes, _>>` that
/// yields the bytes of one utterance and closes when that utterance ends.
fn build_utterance_stream(
    rx: tokio::sync::broadcast::Receiver<ThoughtEvent>,
    subscriber: Option<PeerId>,
) -> impl futures::Stream<Item = Result<Bytes, std::convert::Infallible>> {
    // Once `started` is true, an EndOfUtterance for our filter closes the stream.
    struct S {
        rx: tokio::sync::broadcast::Receiver<ThoughtEvent>,
        subscriber: Option<PeerId>,
        started: bool,
    }

    unfold(
        S {
            rx,
            subscriber,
            started: false,
        },
        |mut s| async move {
            loop {
                match s.rx.recv().await {
                    Ok(ThoughtEvent::Chunk { to, text, .. }) => {
                        if !matches_filter(&to, &s.subscriber) {
                            continue;
                        }
                        s.started = true;
                        return Some((Ok(Bytes::from(text)), s));
                    }
                    Ok(ThoughtEvent::EndOfUtterance { to }) => {
                        if !s.started {
                            continue;
                        }
                        if matches_filter(&to, &s.subscriber) {
                            return None;
                        }
                    }
                    Err(RecvError::Lagged(n)) => {
                        tracing::warn!(missed = n, "thought subscriber lagged");
                        continue;
                    }
                    Err(RecvError::Closed) => return None,
                }
            }
        },
    )
}

fn matches_filter(to: &Option<PeerId>, subscriber: &Option<PeerId>) -> bool {
    match (to, subscriber) {
        (None, _) => true,
        (Some(target), Some(sub)) => target == sub,
        (Some(_), None) => true,
    }
}
