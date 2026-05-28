//! HTTP front — axum router and shared application state.
//!
//! Step 1 wires up:
//! - `GET /` — placeholder HTML (Step 9 swaps in the embedded SPA).
//! - `POST /thought`, `GET /thought` — the only fully-implemented channel.
//! - `POST /approval`, `GET /approval` — Step 7 bridge to ACP request_permission.
//! - 501 stubs for vision/touch/smell/taste.
//! - `POST /audio`, `GET /audio` — Step 11 voice channel, gated on STT/TTS.
//!
//! `AppState` exposes seams the reactor consumes:
//! - `inbound` — mpsc receiver of every accepted Signal.
//! - `thought_out` — broadcast that routers/workers will publish on.
//! - `audio_out` — broadcast for outbound synthesized audio.
//! - `approval_out` — broadcast for approval requests.
//! - `approval_decisions` — mpsc of POST /approval decisions back to the reactor.

use std::path::PathBuf;
use std::sync::Arc;

use axum::Router;
use axum::http::StatusCode;
use axum::routing::post;
use bytes::Bytes;
use chrono::{DateTime, Utc};
use tokio::sync::{broadcast, mpsc};
use tower_http::trace::TraceLayer;

use crate::memory::Memory;
use crate::types::{PeerId, Signal};
use crate::voice::Stt;

pub mod approval;
pub mod audio;
pub mod headers;
pub mod stubs;
pub mod thought;

use approval::{ApprovalDecision, ApprovalEvent};

/// Outbound synthesized-audio event. Carries the bytes plus the mime type the
/// GET /audio long-poll should serve. Sits next to `Signal` in spirit but
/// flows in its own broadcast lane — `Signal.body: String` would mangle audio
/// bytes, and journaling raw bytes into JSONL is a non-starter.
#[derive(Debug, Clone)]
pub struct AudioEvent {
    pub to: Option<PeerId>,
    pub mime: String,
    pub bytes: Bytes,
    pub ts: DateTime<Utc>,
}

/// Shared state passed to every handler via `axum::extract::State`.
///
/// The reactor owns `inbound_rx`, `approval_decisions_rx`, and the broadcast
/// senders' publishing side.
pub struct AppState {
    /// Inbound signals from every channel POST. Step 3 consumes from this.
    pub inbound: mpsc::Sender<Signal>,

    /// Outbound thought broadcast. GET /thought subscribers receive from this.
    pub thought_out: broadcast::Sender<Signal>,

    /// Outbound audio broadcast. GET /audio subscribers receive from this.
    pub audio_out: broadcast::Sender<AudioEvent>,

    /// Outbound approval broadcast. GET /approval subscribers receive.
    pub approval_out: broadcast::Sender<ApprovalEvent>,

    /// Approval decisions coming back from POST /approval. The reactor reads
    /// from the matching receiver and relays into the parked oneshot for the
    /// originating ACP request-permission handler.
    pub approval_decisions: mpsc::Sender<ApprovalDecision>,

    /// Memory substrate — journal + intent store. Cloneable handle.
    pub memory: Memory,

    /// Where blob media lives. POST /audio writes incoming bytes here before
    /// dispatching the transcript through the journal.
    pub data_dir: PathBuf,

    /// Speech-to-text capability. `None` → POST /audio returns 501.
    pub stt: Option<Arc<dyn Stt>>,
}

/// Build the axum app and the matching inbound receiver / outbound senders
/// the reactor will eventually own.
pub fn build(
    memory: Memory,
    data_dir: PathBuf,
    stt: Option<Arc<dyn Stt>>,
) -> (Router, ServerSeams) {
    let (inbound_tx, inbound_rx) = mpsc::channel::<Signal>(1024);
    let (thought_tx, _) = broadcast::channel::<Signal>(256);
    let (audio_tx, _) = broadcast::channel::<AudioEvent>(64);
    let (approval_tx, _) = broadcast::channel::<ApprovalEvent>(64);
    let (approval_decisions_tx, approval_decisions_rx) =
        mpsc::channel::<ApprovalDecision>(64);

    let state = Arc::new(AppState {
        inbound: inbound_tx,
        thought_out: thought_tx.clone(),
        audio_out: audio_tx.clone(),
        approval_out: approval_tx.clone(),
        approval_decisions: approval_decisions_tx,
        memory,
        data_dir,
        stt,
    });

    let router = Router::new()
        .route("/thought", post(thought::post_thought).get(thought::get_thought))
        .route("/audio", post(audio::post_audio).get(audio::get_audio))
        .route("/approval", post(approval::post_approval).get(approval::get_approval))
        .route("/vision", post(stubs::post_vision))
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
        audio_out: audio_tx,
        approval_out: approval_tx,
        approval_decisions_rx,
    };

    (router, seams)
}

/// Handles the reactor will pick up in Step 3.
pub struct ServerSeams {
    pub inbound_rx: mpsc::Receiver<Signal>,
    pub thought_out: broadcast::Sender<Signal>,
    pub audio_out: broadcast::Sender<AudioEvent>,
    pub approval_out: broadcast::Sender<ApprovalEvent>,
    pub approval_decisions_rx: mpsc::Receiver<ApprovalDecision>,
}

async fn not_found() -> (StatusCode, &'static str) {
    (StatusCode::NOT_FOUND, "not found\n")
}
