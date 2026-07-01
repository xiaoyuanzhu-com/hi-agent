//! Video-generation capability — text prompt → short video clip.
//!
//! Asynchronous task: [`submit`] a request to get a task id back, then [`poll`]
//! until it reaches a terminal [`VideoStatus`]. The split keeps the multi-minute
//! wait honest instead of hiding it behind a single blocking call.
//!
//! The capability is a module of free functions over a process-global,
//! once-initialized config: [`init`] resolves the vendor from the credential
//! store, [`available`] reports whether a provider is configured, and [`submit`] /
//! [`poll`] dispatch to it. The config never appears in a signature.
//!
//! **No caller wires this in yet.** The module is built and unit-tested
//! standalone so a later *emission* path can call it as a purely additive
//! change.

use std::sync::OnceLock;

use bytes::Bytes;

use crate::foundation::vendors::doubao_video_gen;

/// A reference image for image-to-video (e.g. the first frame). Either an
/// already-usable URL passed through untouched, or raw bytes the vendor
/// base64-encodes into a `data:` URL at request time.
#[derive(Debug, Clone)]
pub enum ImageRef {
    Url(String),
    Bytes { bytes: Bytes, mime: String },
}

impl ImageRef {
    pub fn url(url: impl Into<String>) -> Self {
        ImageRef::Url(url.into())
    }

    pub fn bytes(bytes: Bytes, mime: impl Into<String>) -> Self {
        ImageRef::Bytes { bytes, mime: mime.into() }
    }
}

/// A request for one short video clip. Only `prompt` is required; the optional
/// `first_frame` turns this into image-to-video, and the remaining knobs map to
/// the vendor's generation parameters when set.
#[derive(Debug, Clone, Default)]
pub struct VideoRequest {
    pub prompt: String,
    /// First-frame reference for image-to-video. `None` → text-to-video.
    pub first_frame: Option<ImageRef>,
    /// e.g. `"480p"`, `"720p"`, `"1080p"`.
    pub resolution: Option<String>,
    /// e.g. `"16:9"`, `"9:16"`, `"1:1"`.
    pub ratio: Option<String>,
    /// Clip length in seconds.
    pub duration: Option<u32>,
    pub watermark: Option<bool>,
    pub seed: Option<i64>,
}

impl VideoRequest {
    /// A bare text-to-video prompt with all knobs left at the vendor default.
    pub fn new(prompt: impl Into<String>) -> Self {
        Self { prompt: prompt.into(), ..Default::default() }
    }
}

/// Where an async video task is in its lifecycle. The non-terminal states
/// (`Queued`, `Running`) mean "poll again later"; the rest are terminal.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum VideoStatus {
    Queued,
    Running,
    Succeeded { video_url: String, last_frame_url: Option<String> },
    Failed { message: String },
    Cancelled,
    Expired,
}

impl VideoStatus {
    /// True once the task has stopped changing — a poll loop exits here.
    pub fn is_terminal(&self) -> bool {
        !matches!(self, VideoStatus::Queued | VideoStatus::Running)
    }
}

/// A video task as last observed: its upstream id plus current [`VideoStatus`].
#[derive(Debug, Clone)]
pub struct VideoTask {
    pub id: String,
    pub status: VideoStatus,
}

enum Backend {
    Disabled,
    Doubao(doubao_video_gen::Config),
}

static BACKEND: OnceLock<Backend> = OnceLock::new();

/// The default wire when the store selects none — the only video-gen impl today.
const DEFAULT_WIRE: &str = "doubao";

/// Resolve the video-gen backend into the process-global config from the credential
/// store. A non-empty `store_key` (BYOK or broker-managed) enables the capability
/// on the configured `wire` (`None` → [`DEFAULT_WIRE`]); no key → disabled. An
/// unknown wire is an error. Adding a vendor is a new `Backend` variant plus a
/// match arm here. Idempotent — the first init wins.
pub fn init(
    store_key: Option<&str>,
    base_url: Option<&str>,
    model: Option<&str>,
    wire: Option<&str>,
) -> anyhow::Result<()> {
    let backend = if store_key.map(|k| !k.trim().is_empty()).unwrap_or(false) {
        match wire.unwrap_or(DEFAULT_WIRE) {
            "doubao" => Backend::Doubao(doubao_video_gen::Config::from_store(store_key, base_url, model)?),
            other => anyhow::bail!("unknown video-gen wire: {other}"),
        }
    } else {
        Backend::Disabled
    };
    let _ = BACKEND.set(backend);
    Ok(())
}

/// Whether a provider is configured.
pub fn available() -> bool {
    matches!(BACKEND.get(), Some(Backend::Doubao(_)))
}

/// Submit a generation request and return the task id to poll. Fast: this only
/// enqueues the work, it does not wait for the clip.
pub async fn submit(req: &VideoRequest) -> anyhow::Result<String> {
    match BACKEND.get() {
        Some(Backend::Doubao(cfg)) => doubao_video_gen::submit(cfg, req).await,
        _ => anyhow::bail!("video generation not configured (set a video key in Settings)"),
    }
}

/// Fetch the current state of a previously-submitted task. Callers poll this
/// until [`VideoStatus::is_terminal`] on their own cadence.
pub async fn poll(task_id: &str) -> anyhow::Result<VideoTask> {
    match BACKEND.get() {
        Some(Backend::Doubao(cfg)) => doubao_video_gen::poll(cfg, task_id).await,
        _ => anyhow::bail!("video generation not configured (set a video key in Settings)"),
    }
}
