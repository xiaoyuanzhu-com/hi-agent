//! HTTP front — axum router and shared application state.

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

pub mod audio;
pub mod headers;
pub mod stubs;
pub mod thought;

pub use thought::ThoughtEvent;

/// Outbound synthesized-audio event. Carries the bytes plus the mime type the
/// GET /audio long-poll should serve.
#[derive(Debug, Clone)]
pub struct AudioEvent {
    pub to: Option<PeerId>,
    pub mime: String,
    pub bytes: Bytes,
    pub ts: DateTime<Utc>,
}

/// Shared state passed to every handler via `axum::extract::State`.
pub struct AppState {
    /// Inbound signals from every channel POST. The reactor consumes these.
    pub inbound: mpsc::Sender<Signal>,

    /// Outbound thought broadcast. GET /thought subscribers receive from this.
    pub thought_out: broadcast::Sender<ThoughtEvent>,

    /// Outbound audio broadcast. GET /audio subscribers receive from this.
    /// No producer wired yet post-MCP; subscribers will hang until the agent
    /// gets an audio-emit path.
    pub audio_out: broadcast::Sender<AudioEvent>,

    /// Memory substrate — journal. Cloneable handle.
    pub memory: Memory,

    /// Where blob media lives. POST /audio writes incoming bytes here before
    /// dispatching the transcript through the journal.
    pub data_dir: PathBuf,

    /// Speech-to-text capability. `None` → POST /audio returns 501.
    pub stt: Option<Arc<dyn Stt>>,
}

pub fn build(
    memory: Memory,
    data_dir: PathBuf,
    stt: Option<Arc<dyn Stt>>,
) -> (Router, ServerSeams) {
    let (inbound_tx, inbound_rx) = mpsc::channel::<Signal>(1024);
    let (thought_tx, _) = broadcast::channel::<ThoughtEvent>(256);
    let (audio_tx, _) = broadcast::channel::<AudioEvent>(64);

    let state = Arc::new(AppState {
        inbound: inbound_tx,
        thought_out: thought_tx.clone(),
        audio_out: audio_tx.clone(),
        memory,
        data_dir,
        stt,
    });

    let router = Router::new()
        .route("/thought", post(thought::post_thought).get(thought::get_thought))
        .route("/audio", post(audio::post_audio).get(audio::get_audio))
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
    };

    (router, seams)
}

pub struct ServerSeams {
    pub inbound_rx: mpsc::Receiver<Signal>,
    pub thought_out: broadcast::Sender<ThoughtEvent>,
    pub audio_out: broadcast::Sender<AudioEvent>,
}

async fn not_found() -> (StatusCode, &'static str) {
    (StatusCode::NOT_FOUND, "not found\n")
}
