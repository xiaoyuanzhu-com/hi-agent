//! HTTP front — axum router and shared application state.
//!
//! Step 1 wires up:
//! - `GET /` — placeholder HTML (Step 9 swaps in the embedded SPA).
//! - `POST /thought`, `GET /thought` — the only fully-implemented channel.
//! - `POST /approval`, `GET /approval` — Step 7 bridge to ACP request_permission.
//! - 501 stubs for vision/audio/touch/smell/taste.
//!
//! `AppState` exposes seams the reactor consumes:
//! - `inbound` — mpsc receiver of every accepted Signal.
//! - `thought_out` — broadcast that routers/workers will publish on.
//! - `approval_out` — broadcast for approval requests.
//! - `approval_decisions` — mpsc of POST /approval decisions back to the reactor.

use std::sync::Arc;

use axum::Router;
use axum::http::StatusCode;
use axum::routing::{get, post};
use tokio::sync::{broadcast, mpsc};
use tower_http::trace::TraceLayer;

use crate::memory::Memory;
use crate::types::Signal;

pub mod approval;
pub mod headers;
pub mod stubs;
pub mod thought;

use approval::{ApprovalDecision, ApprovalEvent};

/// Shared state passed to every handler via `axum::extract::State`.
///
/// The reactor owns `inbound_rx`, `approval_decisions_rx`, and the broadcast
/// senders' publishing side.
pub struct AppState {
    /// Inbound signals from every channel POST. Step 3 consumes from this.
    pub inbound: mpsc::Sender<Signal>,

    /// Outbound thought broadcast. GET /thought subscribers receive from this.
    pub thought_out: broadcast::Sender<Signal>,

    /// Outbound approval broadcast. GET /approval subscribers receive.
    pub approval_out: broadcast::Sender<ApprovalEvent>,

    /// Approval decisions coming back from POST /approval. The reactor reads
    /// from the matching receiver and relays into the parked oneshot for the
    /// originating ACP request-permission handler.
    pub approval_decisions: mpsc::Sender<ApprovalDecision>,

    /// Memory substrate — journal + intent store. Cloneable handle.
    pub memory: Memory,
}

/// Build the axum app and the matching inbound receiver / outbound senders
/// the reactor will eventually own.
pub fn build(memory: Memory) -> (Router, ServerSeams) {
    let (inbound_tx, inbound_rx) = mpsc::channel::<Signal>(1024);
    let (thought_tx, _) = broadcast::channel::<Signal>(256);
    let (approval_tx, _) = broadcast::channel::<ApprovalEvent>(64);
    let (approval_decisions_tx, approval_decisions_rx) =
        mpsc::channel::<ApprovalDecision>(64);

    let state = Arc::new(AppState {
        inbound: inbound_tx,
        thought_out: thought_tx.clone(),
        approval_out: approval_tx.clone(),
        approval_decisions: approval_decisions_tx,
        memory,
    });

    let router = Router::new()
        .route("/thought", post(thought::post_thought).get(thought::get_thought))
        .route("/approval", post(approval::post_approval).get(approval::get_approval))
        .route("/vision", post(stubs::post_vision))
        .route("/audio", post(stubs::post_audio).get(stubs::get_audio))
        .route("/touch", post(stubs::post_touch))
        .route("/smell", post(stubs::post_smell))
        .route("/taste", post(stubs::post_taste))
        .with_state(state)
        .merge(crate::appearance::router())
        .fallback(not_found)
        .layer(TraceLayer::new_for_http());

    let seams = ServerSeams {
        inbound_rx,
        thought_out: thought_tx,
        approval_out: approval_tx,
        approval_decisions_rx,
    };

    (router, seams)
}

/// Handles the reactor will pick up in Step 3.
pub struct ServerSeams {
    pub inbound_rx: mpsc::Receiver<Signal>,
    pub thought_out: broadcast::Sender<Signal>,
    pub approval_out: broadcast::Sender<ApprovalEvent>,
    pub approval_decisions_rx: mpsc::Receiver<ApprovalDecision>,
}

async fn not_found() -> (StatusCode, &'static str) {
    (StatusCode::NOT_FOUND, "not found\n")
}
