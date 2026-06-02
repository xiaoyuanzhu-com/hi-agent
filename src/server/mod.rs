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

use crate::memory::Memory;
use crate::reactor::OutboundSignal;
use crate::types::{PeerId, Signal, SurfaceEnvelope};
use crate::voice::Stt;

pub mod audio;
pub mod binder;
pub mod headers;
pub mod segmenter;
pub mod stubs;
pub mod surface;
pub mod thought;
pub mod thought_bus;
pub mod vision;

pub use thought_bus::ThoughtBus;

/// Outbound synthesized-audio event. One turn's speech is a continuous stream:
/// a `Start` (carrying the mime so GET /audio can set `Content-Type` before the
/// first byte), then a run of `Frame`s as the brain synthesizes them, then an
/// `End`. The GET /audio handler turns one such run into one chunked HTTP
/// response — the client just appends bytes and plays, no per-clip reassembly.
///
/// `to` routes to a peer (or broadcast when `None`); `turn` is the monotonic
/// cognition turn, used to keep a handler's response bound to a single turn so
/// frames from a later turn never bleed into an earlier response.
#[derive(Debug, Clone)]
pub enum AudioEvent {
    Start { to: Option<PeerId>, turn: u64, mime: String },
    Frame { to: Option<PeerId>, turn: u64, bytes: Bytes },
    End { to: Option<PeerId>, turn: u64 },
}

impl AudioEvent {
    /// The routing target, common to every variant.
    pub fn to(&self) -> &Option<PeerId> {
        match self {
            AudioEvent::Start { to, .. }
            | AudioEvent::Frame { to, .. }
            | AudioEvent::End { to, .. } => to,
        }
    }

    /// The cognition turn this event belongs to.
    pub fn turn(&self) -> u64 {
        match self {
            AudioEvent::Start { turn, .. }
            | AudioEvent::Frame { turn, .. }
            | AudioEvent::End { turn, .. } => *turn,
        }
    }
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

    /// Where blob media lives. POST /api/audio and POST /api/vision write
    /// incoming bytes here before journaling the reference.
    pub data_dir: PathBuf,

    /// Speech-to-text capability. `None` → POST /api/audio returns 501.
    pub stt: Option<Arc<dyn Stt>>,
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

    // The reactor's single transport-free outbound seam. A binder task fans each
    // `OutboundSignal` out to the HTTP-shaped carriers above — assigning
    // Content-Type, framing one utterance into one response, closing the body at
    // an utterance boundary. The reactor knows none of that.
    let (out_tx, out_rx) = mpsc::channel::<OutboundSignal>(1024);
    tokio::spawn(binder::bind_outbound(
        out_rx,
        thought_bus.clone(),
        audio_tx.clone(),
        surface_tx.clone(),
    ));

    let state = Arc::new(AppState {
        inbound: inbound_tx,
        thought_bus: thought_bus.clone(),
        audio_out: audio_tx.clone(),
        surface_out: surface_tx.clone(),
        memory,
        data_dir,
        stt,
    });

    // Every channel lives under `/api/*`; the appearance router owns the rest
    // (SPA shell + static assets).
    let router = Router::new()
        .route("/api/thought", post(thought::post_thought).get(thought::get_thought))
        .route("/api/audio", post(audio::post_audio).get(audio::get_audio))
        .route("/api/audio/in", get(audio::get_audio_in))
        .route("/api/surface", get(surface::get_surface))
        .route("/api/vision", post(vision::post_vision))
        .route("/api/touch", post(stubs::post_touch))
        .route("/api/smell", post(stubs::post_smell))
        .route("/api/taste", post(stubs::post_taste))
        .with_state(state)
        .merge(crate::appearance::router())
        .fallback(not_found)
        .layer(TraceLayer::new_for_http());

    let seams = ServerSeams {
        inbound_rx,
        thought_bus,
        out_tx,
    };

    (router, seams)
}

/// What `build` hands back to wire the reactor to the HTTP front. `inbound_rx`
/// is the channel POSTs feed; `out_tx` is the reactor's single transport-free
/// outbound seam (the binder spawned in `build` carries it to the wire). The
/// `thought_bus` is exposed only so integration tests can drive utterances
/// directly without standing up a reactor.
pub struct ServerSeams {
    pub inbound_rx: mpsc::Receiver<Signal>,
    pub thought_bus: ThoughtBus,
    pub out_tx: mpsc::Sender<OutboundSignal>,
}

async fn not_found() -> (StatusCode, &'static str) {
    (StatusCode::NOT_FOUND, "not found\n")
}
