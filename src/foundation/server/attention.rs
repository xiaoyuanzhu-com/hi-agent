//! The attention lane — the web face reports its *own* window coming forward.
//!
//! `POST /api/in/attention` is a first-party heartbeat the page sends when it
//! becomes visible or regains focus (its `visibilitychange` / `focus` events).
//! It's the one signal that tells presence a window was *activated* (brought to
//! front), not merely left open — the strongest "they're checking on you" cue for
//! the expectation axis. Strictly first-party: the page reports only about its own
//! visibility, never anything about other apps or the wider system.
//!
//! Body-less and cheap; it just pokes [`crate::body::presence::Presence`].

use std::sync::Arc;

use axum::extract::State;
use axum::http::StatusCode;

use crate::foundation::server::headers::SceneHeader;
use crate::foundation::server::AppState;

/// Record that this scene's window was activated (became visible / focused).
pub async fn post_attention(
    State(state): State<Arc<AppState>>,
    SceneHeader(scene): SceneHeader,
) -> StatusCode {
    state.presence.note_activation(&scene);
    StatusCode::ACCEPTED
}
