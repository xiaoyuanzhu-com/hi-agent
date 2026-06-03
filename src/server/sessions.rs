//! Session visibility endpoints — the operator's window into ACP sessions.
//!
//! - `GET /api/sessions` — JSON live snapshot of every scene (process, reactor
//!   session, workers, context budget, pending alarms, last turn).
//! - `GET /api/sessions/events` — Server-Sent Events carrying two frame types:
//!   `session` lifecycle events (replayed from the ring on connect, then live)
//!   and periodic `snapshot` frames (the full per-scene mirror). The dashboard
//!   reads scene state from the snapshot frames here rather than polling
//!   `/api/sessions` — one less long-lived endpoint competing for the browser's
//!   ~6 per-origin HTTP/1.1 connections (the channel streams already claim
//!   several), and the snapshots also carry the two mirror-only fields the event
//!   stream never emits: the context budget and worker transcript tails.
//!
//! These read the [`Observatory`](crate::observatory::Observatory) mirror and
//! feed; they never mutate it. The inspect SPA (`/inspect/sessions`) consumes both.
//! Intended for the developer/operator, not the end-user face — sessions stay
//! an internal concept on the channel side.

use std::convert::Infallible;
use std::sync::Arc;
use std::time::Duration;

use axum::extract::State;
use axum::response::IntoResponse;
use axum::response::sse::{Event, KeepAlive, Sse};
use futures::stream::{self, Stream, StreamExt};
use tokio::time::interval;

use crate::observatory::{SceneView, SessionEvent, event_stream};
use crate::server::AppState;

/// How often the events SSE pushes a fresh full snapshot. Matches the old client
/// poll cadence; the frame is ~1 KB so this is negligible on the wire.
const SNAPSHOT_INTERVAL: Duration = Duration::from_millis(1500);

/// `GET /api/sessions` — the live per-scene snapshot as JSON.
pub async fn get_sessions(State(state): State<Arc<AppState>>) -> impl IntoResponse {
    axum::Json(state.observatory.snapshot().await)
}

/// `GET /api/sessions/events` — SSE merging two frame types on one connection:
///
/// - `session` — every lifecycle event. On connect the buffered history replays
///   first (so a late-joining dashboard sees recent context), then live events
///   stream as they happen. Replay and live are cut atomically by
///   [`Observatory::subscribe`](crate::observatory::Observatory::subscribe), so
///   no event is dropped or duplicated across the seam.
/// - `snapshot` — the full per-scene mirror, pushed every [`SNAPSHOT_INTERVAL`].
///   The first frame fires immediately, so a fresh subscriber has complete scene
///   state at once without a separate poll.
pub async fn get_sessions_events(
    State(state): State<Arc<AppState>>,
) -> Sse<impl Stream<Item = Result<Event, Infallible>>> {
    let (replay, rx) = state.observatory.subscribe().await;
    let replay = stream::iter(replay.into_iter().map(Ok::<SessionEvent, Infallible>));
    let live = event_stream(rx);
    let events = replay.chain(live).map(|res| {
        // Both halves are `Result<_, Infallible>`, so this never errors.
        let ev = res.unwrap_or_else(|never| match never {});
        session_event(&ev)
    });

    // Periodic full-snapshot frames on the same connection. `interval`'s first
    // tick is immediate, so the subscriber gets scene state on connect.
    let obs = state.observatory.clone();
    let snapshots = stream::unfold((obs, interval(SNAPSHOT_INTERVAL)), |(obs, mut tick)| async move {
        tick.tick().await;
        let snap = obs.snapshot().await;
        Some((snapshot_event(&snap), (obs, tick)))
    });

    let merged = stream::select(events, snapshots).map(Ok::<Event, Infallible>);
    Sse::new(merged).keep_alive(KeepAlive::default())
}

fn session_event(ev: &SessionEvent) -> Event {
    Event::default()
        .event("session")
        .json_data(ev)
        .unwrap_or_else(|_| Event::default().comment("serialize error"))
}

fn snapshot_event(snap: &[SceneView]) -> Event {
    Event::default()
        .event("snapshot")
        .json_data(snap)
        .unwrap_or_else(|_| Event::default().comment("serialize error"))
}
