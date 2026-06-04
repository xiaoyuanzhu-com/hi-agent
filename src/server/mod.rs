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
use crate::observatory::Observatory;
use crate::reactor::OutboundSignal;
use crate::types::{Channel, Scene, Signal, ViewEnvelope};

pub mod audio;
pub mod binder;
pub mod channels;
pub mod generated;
pub mod headers;
pub mod observe;
pub mod sessions;
pub mod stubs;
pub mod text;
pub mod text_bus;
pub mod view;
pub mod vision;

pub use text_bus::TextBus;

/// Outbound synthesized-audio event. One turn's speech is a continuous stream:
/// a `Start` (carrying the mime so GET /audio can set `Content-Type` before the
/// first byte), then a run of `Frame`s as the brain synthesizes them, then an
/// `End`. The GET /audio handler turns one such run into one chunked HTTP
/// response — the client just appends bytes and plays, no per-clip reassembly.
///
/// `scene` routes to a scene (or broadcast when `None`); `turn` is the monotonic
/// cognition turn, used to keep a handler's response bound to a single turn so
/// frames from a later turn never bleed into an earlier response.
#[derive(Debug, Clone)]
pub enum AudioEvent {
    Start { scene: Option<Scene>, turn: u64, mime: String },
    Frame { scene: Option<Scene>, turn: u64, bytes: Bytes },
    End { scene: Option<Scene>, turn: u64 },
}

impl AudioEvent {
    /// The routing target, common to every variant.
    pub fn scene(&self) -> &Option<Scene> {
        match self {
            AudioEvent::Start { scene, .. }
            | AudioEvent::Frame { scene, .. }
            | AudioEvent::End { scene, .. } => scene,
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

/// Outbound agent-authored view event. Carries the view envelope (compiled
/// module URL + op) plus the routing target the GET /out/view long-poll filters
/// on.
#[derive(Debug, Clone)]
pub struct ViewEvent {
    pub scene: Option<Scene>,
    pub envelope: ViewEnvelope,
    pub ts: DateTime<Utc>,
}

/// A vision frame, broadcast so any local party can *read* the input channel —
/// not just the reactor. `POST /api/in/vision` keeps persisting and additionally
/// publishes the frame here; `GET /api/in/vision` subscribers (e.g. a detector
/// working session) receive it. The bytes are `Bytes` (refcounted), so a frame
/// with no subscriber is a cheap drop. This rides *outside* the cognition turn
/// loop — it never enters the journal or a prompt; perceiving raw frames is the
/// subscriber's job.
#[derive(Debug, Clone)]
pub struct VisionFrameEvent {
    pub scene: Option<Scene>,
    /// Named stream within the scene (`webcam`), or `None` for the default
    /// stream. `GET /api/in/vision` filters on this so concurrent feeds in one
    /// scene stay separable; a subscriber asks for one with `?stream=`.
    pub stream: Option<String>,
    pub bytes: Bytes,
    pub mime: String,
    pub ts: DateTime<Utc>,
}

/// One recognized input, echoed to scene observers on `GET /api/in/<channel>`.
///
/// Inputs (typed text, recognized speech) cross the world→agent boundary on a
/// single POST/WS held by one client, but every client in the scene should see
/// them — the same identical-UI guarantee the outbound channels give. So each
/// input is published here and fanned out live. This is a *presence* signal, not
/// a log: it is broadcast (lossy ring, no replay), matching `audio_out` /
/// `view_out`. A late joiner sees inputs from the moment it connects.
#[derive(Debug, Clone, serde::Serialize)]
pub struct InputEcho {
    pub scene: Scene,
    pub channel: Channel,
    pub text: String,
    /// `false` for a rolling partial (e.g. live STT), `true` once the utterance
    /// is settled. Serialized as `final` for the client.
    #[serde(rename = "final")]
    pub is_final: bool,
    pub ts: DateTime<Utc>,
}

/// One spoken/typed reply, echoed to scene observers — the outbound mirror of
/// [`InputEcho`]. The agent's worded reply is *delivered* through the consuming
/// [`TextBus`], so an operator can't watch it there without stealing it from the
/// real client. The binder publishes a non-draining copy here, letting the
/// channel inspector observe outbound text the same way `InputEcho` exposes
/// inbound text. Presence, not a log: broadcast, lossy, no replay.
#[derive(Debug, Clone, serde::Serialize)]
pub struct OutputEcho {
    pub scene: Scene,
    pub channel: Channel,
    pub text: String,
    /// `false` while a reply is still streaming chunks, `true` at end-of-utterance.
    #[serde(rename = "final")]
    pub is_final: bool,
    pub ts: DateTime<Utc>,
}

/// Shared state passed to every handler via `axum::extract::State`.
pub struct AppState {
    /// Inbound signals from every channel POST. The reactor consumes these.
    pub inbound: mpsc::Sender<Signal>,

    /// Scene warm-up requests. A scene-presence GET (`GET /api/out/*`, the
    /// long-polls a client opens on scene entry) sends the scene here so the
    /// reactor stands it up — spawning the subprocess and opening the ACP session —
    /// before the first utterance lands, keeping that cold-start off the first
    /// reply's critical path. Bounded and best-effort: a full channel only means
    /// warm-ups are already queued, so a dropped request costs at most the
    /// cold-start it would have saved.
    pub warm: mpsc::Sender<Scene>,

    /// Outbound text buffer. GET /api/out/text readers drain it per scene.
    /// Unlike a broadcast, a reply produced while no reader is connected is
    /// retained for the next GET instead of being dropped.
    pub text_bus: TextBus,

    /// Outbound audio broadcast. GET /api/out/audio subscribers receive from
    /// this; the reactor produces TTS clips here when a TTS provider is set.
    pub audio_out: broadcast::Sender<AudioEvent>,

    /// Outbound view-module broadcast. GET /api/out/view subscribers receive from
    /// this; the reactor produces envelopes when the agent emits a `[[view]]` and
    /// the view compiler has built its module.
    pub view_out: broadcast::Sender<ViewEvent>,

    /// Vision-frame broadcast — the read side of the vision *input* channel.
    /// POST /api/in/vision publishes each frame here; GET /api/in/vision
    /// subscribers (a detector working session, …) consume it. Written directly by
    /// the POST handler, not the binder — it is input data, not the reactor's voice.
    pub vision_out: broadcast::Sender<VisionFrameEvent>,

    /// Inbound echo broadcast. GET /api/in/<channel> observers receive recognized
    /// inputs (typed text, recognized speech) from this — live, no replay.
    pub input_echo: broadcast::Sender<InputEcho>,

    /// Outbound text echo broadcast — the non-draining mirror of the agent's
    /// worded reply. The binder publishes here alongside the consuming `TextBus`
    /// so the channel inspector can observe outbound text live.
    pub output_echo: broadcast::Sender<OutputEcho>,

    /// Memory substrate — journal. Cloneable handle.
    pub memory: Memory,

    /// Structured visibility into the ACP session lifecycle. Served read-only by
    /// the `/api/sessions` endpoints.
    pub observatory: Observatory,

    /// Where blob media lives. POST /api/in/audio and POST /api/in/vision write
    /// incoming bytes here before journaling the reference.
    pub data_dir: PathBuf,
}

impl AppState {
    /// Publish one recognized input to the scene's observers. Best-effort and
    /// non-blocking: with no live observer the send is simply dropped (no replay),
    /// matching the live-presence semantics of the outbound broadcasts.
    pub fn echo_input(&self, scene: &Scene, channel: Channel, text: &str, is_final: bool) {
        let _ = self.input_echo.send(InputEcho {
            scene: scene.clone(),
            channel,
            text: text.to_owned(),
            is_final,
            ts: Utc::now(),
        });
    }

    /// Ask the reactor to warm this scene up now — spawn its subprocess and open
    /// its ACP session — triggered when a client opens one of the scene's
    /// `/api/out/*` long-polls. Best-effort and non-blocking: a full queue drops
    /// the request, leaving the scene to cold-start on first use as before.
    /// Idempotent on the reactor side, so repeated GETs are harmless.
    pub fn warm_scene(&self, scene: &Scene) {
        let _ = self.warm.try_send(scene.clone());
    }
}

pub fn build(memory: Memory, data_dir: PathBuf, observatory: Observatory) -> (Router, ServerSeams) {
    let (inbound_tx, inbound_rx) = mpsc::channel::<Signal>(1024);
    // Scene warm-up requests: a presence GET asks the reactor to stand a scene up
    // ahead of its first utterance (see `AppState::warm`).
    let (warm_tx, warm_rx) = mpsc::channel::<Scene>(1024);
    let text_bus = TextBus::new();
    let (audio_tx, _) = broadcast::channel::<AudioEvent>(64);
    let (view_tx, _) = broadcast::channel::<ViewEvent>(64);
    let (vision_tx, _) = broadcast::channel::<VisionFrameEvent>(64);
    // Input echo: live broadcast, lossy ring, no replay (see `InputEcho`).
    let (input_echo_tx, _) = broadcast::channel::<InputEcho>(64);
    // Output text echo: the binder's non-draining mirror (see `OutputEcho`).
    let (output_echo_tx, _) = broadcast::channel::<OutputEcho>(64);

    // The reactor's single transport-free outbound seam. A binder task fans each
    // `OutboundSignal` out to the HTTP-shaped carriers above — assigning
    // Content-Type, framing one utterance into one response, closing the body at
    // an utterance boundary. The reactor knows none of that.
    let (out_tx, out_rx) = mpsc::channel::<OutboundSignal>(1024);
    tokio::spawn(binder::bind_outbound(
        out_rx,
        text_bus.clone(),
        audio_tx.clone(),
        view_tx.clone(),
        output_echo_tx.clone(),
    ));

    let state = Arc::new(AppState {
        inbound: inbound_tx,
        warm: warm_tx,
        text_bus: text_bus.clone(),
        audio_out: audio_tx.clone(),
        view_out: view_tx.clone(),
        vision_out: vision_tx.clone(),
        input_echo: input_echo_tx.clone(),
        output_echo: output_echo_tx.clone(),
        memory,
        observatory,
        data_dir,
    });

    // Channels are namespaced by boundary: `/api/in/*` is the world→agent side
    // (perception), `/api/out/*` is the agent→world side (expression). Each side
    // is observable via GET so every client in a scene renders identical UI.
    // `/api/sessions` is observability, not a channel.
    let router = Router::new()
        .route("/api/in/text", post(text::post_text).get(text::get_in_text))
        .route("/api/out/text", get(text::get_out_text))
        .route("/api/in/audio", post(audio::post_audio).get(audio::get_in_audio))
        .route("/api/in/audio/stream", get(audio::get_audio_stream))
        .route("/api/out/audio", get(audio::get_out_audio))
        // The view channel — agent-authored view modules, turn-paced (one
        // envelope per long-poll response, scene-filtered).
        .route("/api/out/view", get(view::get_out_view))
        // Vision is an input channel that is also observable: POST a frame,
        // GET the live frame stream (a worker session reads it).
        .route("/api/in/vision", post(vision::post_vision).get(vision::get_vision))
        .route("/api/in/touch", post(stubs::post_touch))
        .route("/api/in/smell", post(stubs::post_smell))
        .route("/api/in/taste", post(stubs::post_taste))
        .route("/api/sessions", get(sessions::get_sessions))
        .route("/api/sessions/events", get(sessions::get_sessions_events))
        // A scene's channels, observed live as one merged presence stream — the
        // channel inspector's window onto every in/out channel of one scene.
        .route("/api/scenes/{scene}/channels", get(channels::get_scene_channels))
        // Compiled agent view modules (runtime artifacts on disk under data_dir,
        // not embedded). Served here, not in the appearance router, because that
        // router is embed-only and stateless.
        .route("/generated/views/{file}", get(generated::generated_view))
        .with_state(state)
        .merge(crate::appearance::router())
        .fallback(not_found)
        .layer(TraceLayer::new_for_http());

    let seams = ServerSeams {
        inbound_rx,
        warm_rx,
        text_bus,
        out_tx,
    };

    (router, seams)
}

/// What `build` hands back to wire the reactor to the HTTP front. `inbound_rx`
/// is the channel POSTs feed; `warm_rx` carries scene warm-up requests a presence
/// GET raises; `out_tx` is the reactor's single transport-free outbound seam (the
/// binder spawned in `build` carries it to the wire). The `text_bus` is exposed
/// only so integration tests can drive utterances directly without standing up a
/// reactor.
pub struct ServerSeams {
    pub inbound_rx: mpsc::Receiver<Signal>,
    pub warm_rx: mpsc::Receiver<Scene>,
    pub text_bus: TextBus,
    pub out_tx: mpsc::Sender<OutboundSignal>,
}

async fn not_found() -> (StatusCode, &'static str) {
    (StatusCode::NOT_FOUND, "not found\n")
}
