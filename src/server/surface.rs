//! GET /surface — long-poll for outbound rich-content envelopes.
//!
//! Mirrors GET /audio: subscribe to the reactor's `surface_out` broadcast and
//! return one envelope per request as JSON; the browser re-subscribes for the
//! next. The reactor produces these when the agent emits a `[[surface:…]]`
//! block in its reply.

use std::sync::Arc;

use axum::extract::State;
use axum::http::StatusCode;
use axum::response::IntoResponse;
use tokio::sync::broadcast::error::RecvError;

use crate::server::AppState;
use crate::server::headers::{AuthBearer, ToHeader};

pub async fn get_surface(
    State(state): State<Arc<AppState>>,
    ToHeader(subscriber): ToHeader,
    AuthBearer(auth): AuthBearer,
) -> impl IntoResponse {
    let mut rx = state.surface_out.subscribe();

    tracing::info!(subscriber = ?subscriber, auth = ?auth, "GET /surface long-poll opened");

    loop {
        match rx.recv().await {
            Ok(event) => {
                let deliver = match (&event.to, &subscriber) {
                    (None, _) => true,
                    (Some(target), Some(sub)) => target == sub,
                    (Some(_), None) => true,
                };
                if !deliver {
                    continue;
                }
                return (StatusCode::OK, axum::Json(event.envelope)).into_response();
            }
            Err(RecvError::Lagged(n)) => {
                tracing::warn!(missed = n, "surface subscriber lagged");
                continue;
            }
            Err(RecvError::Closed) => {
                return (StatusCode::SERVICE_UNAVAILABLE, "broadcast closed\n").into_response();
            }
        }
    }
}
