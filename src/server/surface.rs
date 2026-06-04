//! GET /api/out/surface — long-poll for outbound rich-content envelopes.
//!
//! Mirrors GET /api/out/audio: subscribe to the reactor's `surface_out` broadcast
//! and return one envelope per request as JSON; the browser re-subscribes for the
//! next. The reactor produces these when the agent emits a `[[surface:…]]`
//! block in its reply.

use std::sync::Arc;

use axum::extract::State;
use axum::http::StatusCode;
use axum::response::IntoResponse;
use tokio::sync::broadcast::error::RecvError;

use crate::server::AppState;
use crate::server::headers::{AuthBearer, RequiredScene};

pub async fn get_out_surface(
    State(state): State<Arc<AppState>>,
    RequiredScene(scene): RequiredScene,
    AuthBearer(auth): AuthBearer,
) -> impl IntoResponse {
    let mut rx = state.surface_out.subscribe();

    tracing::info!(scene = %scene, auth = ?auth, "GET /api/out/surface long-poll opened");

    // Opening this long-poll is a scene-presence signal: warm the scene up so its
    // process + session + upstream cache are hot before the first utterance.
    state.warm_scene(&scene);

    loop {
        match rx.recv().await {
            Ok(event) => {
                let deliver = match &event.scene {
                    None => true,
                    Some(target) => target == &scene,
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
