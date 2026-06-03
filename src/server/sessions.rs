//! Session visibility endpoints — the operator's window into ACP sessions.
//!
//! - `GET /api/sessions` — JSON live snapshot of every scene (process, reactor
//!   session, workers, context budget, pending alarms, last turn).
//! - `GET /api/sessions/events` — Server-Sent Events: every lifecycle event,
//!   replayed from the ring on connect then streamed live. One stream, all scenes.
//!
//! These read the [`Observatory`](crate::observatory::Observatory) mirror and
//! feed; they never mutate it. The inspect SPA (`/inspect/sessions`) consumes both.
//! Intended for the developer/operator, not the end-user face — sessions stay
//! an internal concept on the channel side.

use std::convert::Infallible;
use std::sync::Arc;

use axum::extract::State;
use axum::response::IntoResponse;
use axum::response::sse::{Event, KeepAlive, Sse};
use futures::stream::{self, Stream, StreamExt};

use crate::observatory::{SessionEvent, event_stream};
use crate::server::AppState;

/// `GET /api/sessions` — the live per-scene snapshot as JSON.
pub async fn get_sessions(State(state): State<Arc<AppState>>) -> impl IntoResponse {
    axum::Json(state.observatory.snapshot().await)
}

/// `GET /api/sessions/events` — SSE of every lifecycle event. On connect, the
/// buffered history replays first (so a late-joining dashboard sees recent
/// context), then live events stream as they happen. Replay and live are cut
/// atomically by [`Observatory::subscribe`](crate::observatory::Observatory::subscribe),
/// so no event is dropped or duplicated across the seam.
pub async fn get_sessions_events(
    State(state): State<Arc<AppState>>,
) -> Sse<impl Stream<Item = Result<Event, Infallible>>> {
    let (replay, rx) = state.observatory.subscribe().await;
    let replay = stream::iter(replay.into_iter().map(Ok::<SessionEvent, Infallible>));
    let live = event_stream(rx);
    let events = replay.chain(live).map(|res| {
        // Both halves are `Result<_, Infallible>`, so this never errors.
        let ev = res.unwrap_or_else(|never| match never {});
        Ok(to_sse_event(&ev))
    });
    Sse::new(events).keep_alive(KeepAlive::default())
}

fn to_sse_event(ev: &SessionEvent) -> Event {
    Event::default()
        .event("session")
        .json_data(ev)
        .unwrap_or_else(|_| Event::default().comment("serialize error"))
}
