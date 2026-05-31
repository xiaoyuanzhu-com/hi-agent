//! HTTP front — axum router and shared application state.

use std::path::PathBuf;
use std::sync::Arc;

use axum::Router;
use axum::http::StatusCode;
use axum::routing::{get, post};
use bytes::Bytes;
use chrono::{DateTime, Utc};
use tokio::sync::{broadcast, mpsc};
use tower_http::trace::TraceLayer;

use crate::floor::FloorState;
use crate::memory::Memory;
use crate::types::{PeerId, Signal, SurfaceEnvelope};
use crate::voice::Stt;

pub mod audio;
pub mod headers;
pub mod stt_stream;
pub mod stubs;
pub mod surface;
pub mod thought;
pub mod thought_bus;

pub use thought_bus::ThoughtBus;

/// Outbound synthesized-audio event. Carries the bytes plus the mime type the
/// GET /audio long-poll should serve.
#[derive(Debug, Clone)]
pub struct AudioEvent {
    pub to: Option<PeerId>,
    pub mime: String,
    pub bytes: Bytes,
    /// Monotonic id of the cognition turn that produced this clip. Internal to
    /// the mind now — it tags the channel logs so a reply is traceable across
    /// the thought + audio streams. The client never sees it: turn-taking is
    /// decided server-side (commit-after-quiet), so by the time a clip is on the
    /// wire it's already the committed reply, and the client just plays it.
    pub turn: u64,
    pub ts: DateTime<Utc>,
}

/// Outbound rich-content event. Carries the envelope plus the routing target
/// that the GET /surface long-poll filters on.
#[derive(Debug, Clone)]
pub struct SurfaceEvent {
    pub to: Option<PeerId>,
    pub envelope: SurfaceEnvelope,
    pub ts: DateTime<Utc>,
}

/// Shared state passed to every handler via `axum::extract::State`.
pub struct AppState {
    /// Inbound signals from every channel POST. The reactor consumes these.
    pub inbound: mpsc::Sender<Signal>,

    /// Outbound thought buffer. GET /thought readers drain it per peer. Unlike
    /// a broadcast, a reply produced while no reader is connected is retained
    /// for the next GET instead of being dropped.
    pub thought_bus: ThoughtBus,

    /// Outbound audio broadcast. GET /audio subscribers receive from this; the
    /// reactor produces TTS clips here when a TTS provider is configured.
    pub audio_out: broadcast::Sender<AudioEvent>,

    /// Outbound rich-content broadcast. GET /surface subscribers receive from
    /// this; the reactor produces envelopes when the agent emits a surface block.
    pub surface_out: broadcast::Sender<SurfaceEvent>,

    /// Memory substrate — journal. Cloneable handle.
    pub memory: Memory,

    /// Where blob media lives. POST /audio writes incoming bytes here before
    /// dispatching the transcript through the journal.
    pub data_dir: PathBuf,

    /// Speech-to-text capability. `None` → POST /audio returns 501.
    pub stt: Option<Arc<dyn Stt>>,

    /// Live per-peer floor signal. The streaming-STT handler marks a peer
    /// "speaking" for each mic socket's lifetime; the reactor reads it to wait
    /// for a settled silence before replying. See [`crate::floor`].
    pub floor: FloorState,
}

pub fn build(
    memory: Memory,
    data_dir: PathBuf,
    stt: Option<Arc<dyn Stt>>,
) -> (Router, ServerSeams) {
    let (inbound_tx, inbound_rx) = mpsc::channel::<Signal>(1024);
    let thought_bus = ThoughtBus::new();
    let (audio_tx, _) = broadcast::channel::<AudioEvent>(64);
    let (surface_tx, _) = broadcast::channel::<SurfaceEvent>(64);
    let floor = FloorState::new();

    let state = Arc::new(AppState {
        inbound: inbound_tx,
        thought_bus: thought_bus.clone(),
        audio_out: audio_tx.clone(),
        surface_out: surface_tx.clone(),
        memory,
        data_dir,
        stt,
        floor: floor.clone(),
    });

    let router = Router::new()
        .route("/thought", post(thought::post_thought).get(thought::get_thought))
        .route("/audio", post(audio::post_audio).get(audio::get_audio))
        .route("/stt/stream", get(stt_stream::get_stt_stream))
        .route("/surface", get(surface::get_surface))
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
        thought_bus,
        audio_out: audio_tx,
        surface_out: surface_tx,
        floor,
    };

    (router, seams)
}

pub struct ServerSeams {
    pub inbound_rx: mpsc::Receiver<Signal>,
    pub thought_bus: ThoughtBus,
    pub audio_out: broadcast::Sender<AudioEvent>,
    pub surface_out: broadcast::Sender<SurfaceEvent>,
    pub floor: FloorState,
}

async fn not_found() -> (StatusCode, &'static str) {
    (StatusCode::NOT_FOUND, "not found\n")
}
