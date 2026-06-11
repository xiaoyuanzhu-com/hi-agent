//! GET /api/out/view — long-poll for outbound agent-authored view modules.
//!
//! Mirrors GET /api/out/surface: subscribe to the reactor's `view_out` broadcast
//! and return one envelope per request as JSON; the browser re-subscribes for the
//! next. The reactor produces these when the agent emits a `[[view…]]` block in
//! its reply and the view compiler has turned its source into a module.

use std::sync::Arc;

use axum::extract::State;
use axum::http::StatusCode;
use axum::response::IntoResponse;
use tokio::sync::broadcast::error::RecvError;

use crate::server::AppState;
use crate::server::headers::{AuthBearer, RequiredScene};

pub async fn get_out_view(
    State(state): State<Arc<AppState>>,
    RequiredScene(scene): RequiredScene,
    AuthBearer(auth): AuthBearer,
) -> impl IntoResponse {
    let mut rx = state.view_out.subscribe();
    // A held view long-poll = a screen is attached; counted until this handler returns.
    let _presence = state.presence.connect(&scene, crate::presence::OutChannel::View);

    tracing::info!(scene = %scene, auth = ?auth, "GET /api/out/view long-poll opened");

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
                tracing::warn!(missed = n, "view subscriber lagged");
                continue;
            }
            Err(RecvError::Closed) => {
                return (StatusCode::SERVICE_UNAVAILABLE, "broadcast closed\n").into_response();
            }
        }
    }
}
