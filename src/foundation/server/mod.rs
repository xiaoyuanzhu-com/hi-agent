//! HTTP front — axum router and shared application state.

use std::collections::{HashMap, HashSet};
use std::path::PathBuf;
use std::sync::{Arc, Mutex};
use std::sync::atomic::AtomicU64;

use axum::Router;
use axum::extract::DefaultBodyLimit;
use axum::http::StatusCode;
use axum::routing::{get, post};
use bytes::Bytes;
use chrono::{DateTime, Utc};
use tokio::sync::{broadcast, mpsc};
use tower_http::trace::TraceLayer;

use crate::mind::memory::Memory;
use crate::foundation::acp::AcpTap;
use crate::foundation::observatory::Observatory;
use crate::body::reactor::{InterruptRegistry, OutboundSignal, ToolRegistry};
use crate::types::{Channel, Scene, Signal, ViewEnvelope};

pub mod account;
pub mod acp;
pub mod audio;
pub mod binder;
pub mod channels;
pub mod files;
pub mod generated;
pub mod headers;
pub mod mcp;
pub mod observe;
pub mod reflex;
pub mod sessions;
pub mod stubs;
pub mod text;
pub mod text_bus;
pub mod view;
pub mod view_bus;
pub mod vision;

pub use text_bus::TextBus;
pub use view_bus::ViewBus;

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

/// Inbound audio event — the read side of the audio *input* channel, the mirror
/// of [`AudioEvent`]. "Audio is audio": the bytes the world feeds the agent are
/// observable as bytes, not summarized to a transcript. One source (a mic stream
/// or a posted clip) is a `Start`/`Frame`*/`End` run; `GET /api/in/audio` turns
/// one run into one chunked HTTP response a client can play.
///
/// `turn` is a per-source id (one WS connection or one POST), keeping a
/// listener's response bound to a single source so concurrent uploaders in a
/// scene never interleave in one body. `mime` carries the format so a listener
/// can decode — `audio/pcm;rate=16000;channels=1` for the live mic stream, the
/// clip's own type for a posted clip. Like the other channel broadcasts this is
/// lossy presence with no replay; the transcript the agent actually consumes
/// rides the *text* channel.
#[derive(Debug, Clone)]
pub enum AudioInEvent {
    Start { scene: Option<Scene>, turn: u64, mime: String },
    Frame { scene: Option<Scene>, turn: u64, bytes: Bytes },
    End { scene: Option<Scene>, turn: u64 },
}

impl AudioInEvent {
    /// The routing target, common to every variant.
    pub fn scene(&self) -> &Option<Scene> {
        match self {
            AudioInEvent::Start { scene, .. }
            | AudioInEvent::Frame { scene, .. }
            | AudioInEvent::End { scene, .. } => scene,
        }
    }

    /// The source this event belongs to (one mic stream or one posted clip).
    pub fn turn(&self) -> u64 {
        match self {
            AudioInEvent::Start { turn, .. }
            | AudioInEvent::Frame { turn, .. }
            | AudioInEvent::End { turn, .. } => *turn,
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

/// Inbound video event — the read side of the vision *input* channel, the visual
/// twin of [`AudioInEvent`]. "Vision is video": the camera streams continuously
/// and the bytes are observable as bytes (the backend never decodes or samples
/// frames — that's a future perception path's job). One camera session is a
/// `Start`/`Frame`*/`End` run; `GET /api/in/vision` turns one run into one chunked
/// HTTP response a client can play.
///
/// `turn` is a per-source id (one WS connection); `mime` is the container/codec
/// (`video/webm;codecs=…`). Like the other channel broadcasts this is lossy
/// presence with no replay — but unlike audio frames a WebM stream is only
/// decodable from its first chunk (the initialization segment), so the active
/// source's init bytes are cached separately (see [`VideoSource`]) to let a late
/// observer join mid-stream.
#[derive(Debug, Clone)]
pub enum VideoInEvent {
    Start { scene: Option<Scene>, turn: u64, mime: String },
    Frame { scene: Option<Scene>, turn: u64, bytes: Bytes },
    End { scene: Option<Scene>, turn: u64 },
}

impl VideoInEvent {
    /// The routing target, common to every variant.
    pub fn scene(&self) -> &Option<Scene> {
        match self {
            VideoInEvent::Start { scene, .. }
            | VideoInEvent::Frame { scene, .. }
            | VideoInEvent::End { scene, .. } => scene,
        }
    }

    /// The source this event belongs to (one camera WS connection).
    pub fn turn(&self) -> u64 {
        match self {
            VideoInEvent::Start { turn, .. }
            | VideoInEvent::Frame { turn, .. }
            | VideoInEvent::End { turn, .. } => *turn,
        }
    }
}

/// The currently-active inbound-video source for a scene: its turn id, mime, and
/// cached WebM initialization segment (the first chunk). A `GET /api/in/vision`
/// observer that connects after the camera started writes this init before the
/// live frames so MSE can decode the stream; without it the `<video>` stalls.
#[derive(Debug, Clone)]
pub struct VideoSource {
    pub turn: u64,
    pub mime: String,
    pub init: Bytes,
}

/// A snapshot of the in-progress (not-yet-flushed) camera minute for a scene, so a
/// tool can grab "what just happened" without waiting for the minute to roll over and
/// flush to disk. Holds the cached init segment plus the media bytes accumulated so
/// far this minute; `init` followed by `buf` is an independently-decodable clip — the
/// same shape every persisted minute file has. Refreshed as chunks arrive and cleared
/// when the camera closes (see [`vision`]).
#[derive(Debug, Clone)]
pub struct PartialMinute {
    pub turn: u64,
    pub mime: String,
    pub init: Bytes,
    pub buf: Bytes,
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

/// Per-scene face-presence state for the presence lane (`POST /api/in/vision/presence`).
///
/// The presence still-loop posts a low-res camera frame every couple of seconds;
/// the handler recognizes faces locally and edge-triggers a perception signal only
/// when *who is present* changes. `last_seen` times each identity label's most
/// recent sighting (so a momentarily-missed detection doesn't flap a leave), and
/// `announced` is the set we currently treat as on-camera (so each appear/leave
/// fires exactly once per transition). See [`vision::post_presence`].
#[derive(Default)]
pub struct FacePresence {
    pub last_seen: HashMap<String, DateTime<Utc>>,
    pub announced: HashSet<String>,
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

    /// Per-scene retained appearance state. GET /api/out/view serves whole-state
    /// snapshots from this; the binder folds each reactor-emitted envelope in.
    /// Unlike a broadcast, a view shown while no client is connected is retained —
    /// refresh, a second device, or a restart all converge on the same screen.
    pub views: ViewBus,

    /// Outbound view-event broadcast — the non-draining debug tap of the view
    /// channel, observed by the channel inspector. Delivery rides `views`.
    pub view_out: broadcast::Sender<ViewEvent>,

    /// Inbound audio broadcast — the read side of the audio *input* channel.
    /// `POST /api/in/audio` and `WS /api/in/audio/stream` publish the raw audio
    /// bytes here; `GET /api/in/audio` subscribers play them. Written directly by
    /// the ingest handlers, not the binder — it is input data, not the reactor's
    /// voice. The transcript the agent consumes rides the *text* channel instead.
    pub audio_in: broadcast::Sender<AudioInEvent>,

    /// Hands each inbound-audio source (one WS connection or one posted clip) a
    /// distinct `turn` id, so concurrent uploaders never interleave in one
    /// `GET /api/in/audio` listener response.
    pub audio_in_turn: AtomicU64,

    /// Inbound video broadcast — the read side of the vision *input* channel.
    /// `WS /api/in/vision/stream` publishes the camera's WebM chunks here;
    /// `GET /api/in/vision` subscribers play them. Written directly by the ingest
    /// handler, not the binder — it is input data, not the reactor's voice. The
    /// backend never decodes the video; perceiving frames is a future job.
    pub video_in: broadcast::Sender<VideoInEvent>,

    /// Hands each inbound-video source (one camera WS connection) a distinct
    /// `turn` id, keeping a `GET /api/in/vision` observer bound to one camera.
    pub video_in_turn: AtomicU64,

    /// The active inbound-video source per scene, holding its cached WebM init
    /// segment so an observer can join the live stream mid-flight (see
    /// [`VideoSource`]). Inserted on a camera's first chunk, removed on close.
    pub video_in_live: Mutex<HashMap<Scene, VideoSource>>,

    /// The in-progress (not-yet-flushed) camera minute per scene — a freshness window
    /// for the agent's `watch` tool, which otherwise sees only persisted minute files
    /// up to ~60s stale. Refreshed as chunks accumulate, cleared on camera close. See
    /// [`PartialMinute`].
    pub video_in_partial: Mutex<HashMap<Scene, PartialMinute>>,

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

    /// Raw JSON-RPC wire tap — every ACP frame, business-logic agnostic. Served
    /// read-only by `GET /api/acp/frames/events` for the raw session inspector.
    pub acp_tap: AcpTap,

    /// Where blob media lives. POST /api/in/audio and POST /api/in/vision write
    /// incoming bytes here before journaling the reference.
    pub data_dir: PathBuf,

    /// Owner xiaoyuanzhu sign-in (`Some` only when OIDC is configured). Not a gate.
    /// Retained for a future `sub`-tier account link; no request handler reads it
    /// today — the credential/account routes that did were removed when the config
    /// surface moved to the native tray. `None` ⇒ sign-in unavailable (free tier).
    pub auth: Option<Arc<crate::foundation::auth::AuthState>>,

    /// Scene→tool-sink table. The `/mcp` handler looks a scene up here to route a
    /// tool call to its reactor loop; the reactor registers each scene's sink as
    /// it stands the loop up. See [`crate::body::reactor::ToolRegistry`].
    pub tool_registry: ToolRegistry,

    /// Scene→barge-in state, shared with the reactor. The STT relay reports
    /// recognized speech here ([`crate::body::reactor::InterruptRegistry::note_speech`]);
    /// nothing else on the HTTP side touches it — there is no interrupt
    /// endpoint, the mind infers interruptions from its own clock.
    pub interrupts: InterruptRegistry,

    /// Scene→live-subscriber counts, shared with the reactor. Out-channel
    /// handlers hold a [`crate::body::presence::PresenceGuard`] per connection; the
    /// reactor renders the counts into each turn as human-model facts.
    pub presence: crate::body::presence::Presence,

    /// Scene-scoped phone-upload tokens for the file-upload carrier. A QR encodes
    /// `/up/<token>`; the token resolves to the scene so a phone with no
    /// `X-HI-Scene` header lands in the right scene. Short TTL, pruned on access,
    /// in-memory (a restart drops outstanding links). See [`files`].
    pub handoffs: Mutex<HashMap<String, files::Handoff>>,

    /// Per-scene face-presence state for the presence lane. The presence handler
    /// reads and updates this to decide when an appear/leave event is worth a
    /// signal. See [`FacePresence`] and [`vision::post_presence`].
    pub face_presence: Mutex<HashMap<Scene, FacePresence>>,
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

/// Max body for a handed-file upload. Generous enough for photos/scans/PDFs;
/// the rest of the channels keep axum's small default.
const MAX_UPLOAD: usize = 50 * 1024 * 1024;

pub fn build(
    memory: Memory,
    data_dir: PathBuf,
    observatory: Observatory,
    acp_tap: AcpTap,
    tool_registry: ToolRegistry,
    interrupts: InterruptRegistry,
    presence: crate::body::presence::Presence,
    auth: Option<Arc<crate::foundation::auth::AuthState>>,
) -> (Router, ServerSeams) {
    let (inbound_tx, inbound_rx) = mpsc::channel::<Signal>(1024);
    // Scene warm-up requests: a presence GET asks the reactor to stand a scene up
    // ahead of its first utterance (see `AppState::warm`).
    let (warm_tx, warm_rx) = mpsc::channel::<Scene>(1024);
    let text_bus = TextBus::new();
    let (audio_tx, _) = broadcast::channel::<AudioEvent>(64);
    // Inbound audio: small, frequent PCM frames, so a larger ring than the others.
    let (audio_in_tx, _) = broadcast::channel::<AudioInEvent>(256);
    let (view_tx, _) = broadcast::channel::<ViewEvent>(64);
    // Per-scene retained appearance state, reloaded from disk so a scene's
    // screen survives a restart (see `ViewBus`).
    let view_bus = ViewBus::load(&data_dir);
    // Inbound video: continuous WebM chunks, so a larger ring like inbound audio.
    let (video_in_tx, _) = broadcast::channel::<VideoInEvent>(256);
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
        view_bus.clone(),
        view_tx.clone(),
        output_echo_tx.clone(),
    ));

    let state = Arc::new(AppState {
        inbound: inbound_tx,
        warm: warm_tx,
        text_bus: text_bus.clone(),
        audio_out: audio_tx.clone(),
        audio_in: audio_in_tx.clone(),
        audio_in_turn: AtomicU64::new(0),
        views: view_bus,
        view_out: view_tx.clone(),
        video_in: video_in_tx.clone(),
        video_in_turn: AtomicU64::new(0),
        video_in_live: Mutex::new(HashMap::new()),
        video_in_partial: Mutex::new(HashMap::new()),
        input_echo: input_echo_tx.clone(),
        output_echo: output_echo_tx.clone(),
        memory,
        observatory,
        acp_tap,
        data_dir,
        auth: auth.clone(),
        tool_registry,
        interrupts,
        presence,
        handoffs: Mutex::new(HashMap::new()),
        face_presence: Mutex::new(HashMap::new()),
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
        // The view channel — a scene's retained appearance, served as versioned
        // whole-state snapshots (long-poll on `?since=`).
        .route("/api/out/view", get(view::get_out_view).delete(view::clear_out_view))
        // Vision is an input channel that is also observable: the camera streams
        // WebM over the WS, GET plays the live video; POST persists a still frame.
        .route("/api/in/vision", post(vision::post_vision).get(vision::get_vision))
        .route("/api/in/vision/stream", get(vision::get_vision_stream))
        // The presence lane: a cheap local face reflex. The client posts a low-res
        // camera still every couple of seconds; the handler recognizes faces on the
        // local models and emits a perception signal only when who is present
        // changes — real-time "who's here", no remote call. A no-op without the
        // face capability.
        .route("/api/in/vision/presence", post(vision::post_presence))
        // The file channel — handing the agent a file (handed artifact, not a
        // sense). Drag-drop posts to /api/in/file; the phone handoff mints a
        // token (/api/handoff), serves an uploader at /up/<token>, receives at
        // /api/up/<token>, and renders the QR via /api/qr. Uploads get a generous
        // body limit; everything else keeps axum's small default.
        .route("/api/in/file", post(files::post_file).layer(DefaultBodyLimit::max(MAX_UPLOAD)))
        .route("/api/handoff", post(files::post_handoff))
        .route("/up/{token}", get(files::get_up_page))
        .route("/api/up/{token}", post(files::post_up).layer(DefaultBodyLimit::max(MAX_UPLOAD)))
        .route("/api/qr", get(files::get_qr))
        .route("/api/in/touch", post(stubs::post_touch))
        .route("/api/in/smell", post(stubs::post_smell))
        .route("/api/in/taste", post(stubs::post_taste))
        .route("/api/sessions", get(sessions::get_sessions))
        .route("/api/sessions/events", get(sessions::get_sessions_events))
        // The raw ACP wire feed — every JSON-RPC frame, business-logic agnostic.
        // Backs the raw session inspector at `/inspect/sessions`.
        .route("/api/acp/frames/events", get(acp::get_acp_frames_events))
        // The MCP tool endpoint a session's `mcp_servers` attach connects to. The
        // mind drives output and side-effects by calling tools here; routing is by
        // the X-HI-Scene/X-HI-Role headers the attach carries.
        .route("/mcp", post(mcp::post_mcp).get(mcp::get_mcp))
        // Fire a taught quick-action reflex — recognize the current field via the
        // accessibility tree and type the stored value, no model in the loop. The
        // v1 trigger (a later hotkey/gesture would call the same path).
        .route("/api/reflex/invoke", post(reflex::post_invoke))
        // The device account's energy standing + a signed-in upgrade link. Public,
        // like every route here; the out-of-energy card calls both.
        .route("/api/account/energy", get(account::get_energy))
        .route("/api/account/subscribe", get(account::get_subscribe))
        // A scene's channels, observed live as one merged presence stream — the
        // channel inspector's window onto every in/out channel of one scene.
        .route("/api/scenes/{scene}/channels", get(channels::get_scene_channels))
        // The agent's view workshop on disk (under data_dir) — compiled view modules,
        // images, and build-agent artifacts. Served here, not in the appearance
        // router, because that router is embed-only and stateless.
        .route("/views/{*path}", get(generated::views_file))
        .with_state(state.clone())
        .merge(crate::appearance::router())
        .fallback(not_found);

    // Mount the owner sign-in routes (`/auth/*`) when OIDC is configured. There is
    // no access gate — every route is public; sign-in is an opt-in action that only
    // links the owner's xiaoyuanzhu account for a `sub`-tier upgrade.
    let router = match auth {
        Some(auth) => crate::foundation::auth::mount(router, auth),
        None => router,
    };
    let router = router.layer(TraceLayer::new_for_http());

    let seams = ServerSeams {
        inbound_rx,
        warm_rx,
        text_bus,
        out_tx,
        state,
    };

    (router, seams)
}

/// What `build` hands back to wire the reactor to the HTTP front. `inbound_rx`
/// is the channel POSTs feed; `warm_rx` carries scene warm-up requests a presence
/// GET raises; `out_tx` is the reactor's single transport-free outbound seam (the
/// binder spawned in `build` carries it to the wire). The `text_bus` is exposed
/// only so integration tests can drive utterances directly without standing up a
/// reactor. `state` is the shared `AppState` (the same `Arc` the router holds), so
/// a non-HTTP producer — the come-and-see-this gesture — can inject inbound
/// signals through the same path as a channel POST.
pub struct ServerSeams {
    pub inbound_rx: mpsc::Receiver<Signal>,
    pub warm_rx: mpsc::Receiver<Scene>,
    pub text_bus: TextBus,
    pub out_tx: mpsc::Sender<OutboundSignal>,
    pub state: Arc<AppState>,
}

async fn not_found() -> (StatusCode, &'static str) {
    (StatusCode::NOT_FOUND, "not found\n")
}
