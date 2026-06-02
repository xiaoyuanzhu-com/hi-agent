//! Imagery capability — image and video *generation*.
//!
//! Where [`crate::vision`] turns visual input into text (perception), this
//! module turns a text prompt into visual *output*: a still image or a short
//! video clip. It is the generation counterpart to what TTS is for voice — the
//! agent expresses, a provider renders.
//!
//! Two capabilities, each with its own trait and provider switch, because their
//! upstream shapes differ:
//!
//!   - **Image** generation is *synchronous* — one request, one response with
//!     the picture(s). Modelled by [`ImageGen`].
//!   - **Video** generation is an *asynchronous task* — you `submit` a request,
//!     get a task id back, then `poll` until it reaches a terminal status.
//!     Modelled by [`VideoGen`]. The split keeps the multi-minute wait honest
//!     instead of hiding it behind a single blocking call.
//!
//! The one concrete impl shipping today is [`doubao::DoubaoImageGen`] /
//! [`doubao::DoubaoVideoGen`] (Volcengine Ark). Swapping or adding a provider is
//! one file plus an arm in the `build_*` fns.
//!
//! **No caller wires this in yet.** The module is built and unit-tested
//! standalone so a later *emission* path — e.g. a `[[image:…]]` / `[[video:…]]`
//! surface marker the reactor articulates, the visual analogue of TTS rendering
//! spoken text — can call it as a purely additive change.

use std::sync::Arc;

use async_trait::async_trait;
use bytes::Bytes;

pub mod doubao;

const ENV_IMAGE_GEN_PROVIDER: &str = "IMAGE_GEN_PROVIDER";
const ENV_VIDEO_GEN_PROVIDER: &str = "VIDEO_GEN_PROVIDER";

// --- Image generation -------------------------------------------------------

/// How the provider should return a generated image: a hosted `url` (the
/// default; cheaper on the wire but expires upstream, so a caller persists it
/// promptly) or inline base64 (`b64_json`).
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub enum ImageFormat {
    #[default]
    Url,
    B64Json,
}

/// A request for one or more still images. Only `prompt` is required; every
/// other field is `None` → "let the model decide", so the provider's own
/// defaults apply (size, watermark, …) rather than us hard-coding them.
#[derive(Debug, Clone, Default)]
pub struct ImageRequest {
    pub prompt: String,
    /// e.g. `"1024x1024"`, `"2K"`, or `"adaptive"`; provider-specific.
    pub size: Option<String>,
    pub seed: Option<i64>,
    pub watermark: Option<bool>,
    pub response_format: ImageFormat,
}

impl ImageRequest {
    /// A bare prompt with all knobs left at the provider default.
    pub fn new(prompt: impl Into<String>) -> Self {
        Self { prompt: prompt.into(), ..Default::default() }
    }
}

/// One generated image. Exactly one of `url` / `b64_json` is populated,
/// matching the requested [`ImageFormat`].
#[derive(Debug, Clone)]
pub struct GeneratedImage {
    pub url: Option<String>,
    pub b64_json: Option<String>,
    pub size: Option<String>,
}

#[async_trait]
pub trait ImageGen: Send + Sync {
    /// Generate image(s) for `req` and return them. Synchronous: the future
    /// resolves once the picture(s) are ready.
    async fn generate(&self, req: &ImageRequest) -> anyhow::Result<Vec<GeneratedImage>>;
}

// --- Video generation -------------------------------------------------------

/// A reference image for image-to-video (e.g. the first frame). Either an
/// already-usable URL passed through untouched, or raw bytes the provider
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
/// the provider's generation parameters when set.
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
    /// A bare text-to-video prompt with all knobs left at the provider default.
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

#[async_trait]
pub trait VideoGen: Send + Sync {
    /// Submit a generation request and return the task id to poll. Fast: this
    /// only enqueues the work, it does not wait for the clip.
    async fn submit(&self, req: &VideoRequest) -> anyhow::Result<String>;

    /// Fetch the current state of a previously-submitted task. Callers poll
    /// this until [`VideoStatus::is_terminal`] on their own cadence.
    async fn poll(&self, task_id: &str) -> anyhow::Result<VideoTask>;
}

// --- Provider selection -----------------------------------------------------

/// Construct the image-generation provider selected by `IMAGE_GEN_PROVIDER`, or
/// `None` if unset or `none`. An unknown provider name is an error so
/// misconfiguration surfaces at startup rather than at first use.
pub fn build_image_gen() -> anyhow::Result<Option<Arc<dyn ImageGen>>> {
    let provider = std::env::var(ENV_IMAGE_GEN_PROVIDER).unwrap_or_default();
    match provider.as_str() {
        "" | "none" => Ok(None),
        "doubao" | "volcengine" => Ok(Some(Arc::new(doubao::DoubaoImageGen::from_env()?))),
        other => anyhow::bail!("unknown {ENV_IMAGE_GEN_PROVIDER}: {other}"),
    }
}

/// Construct the video-generation provider selected by `VIDEO_GEN_PROVIDER`, or
/// `None` if unset or `none`.
pub fn build_video_gen() -> anyhow::Result<Option<Arc<dyn VideoGen>>> {
    let provider = std::env::var(ENV_VIDEO_GEN_PROVIDER).unwrap_or_default();
    match provider.as_str() {
        "" | "none" => Ok(None),
        "doubao" | "volcengine" => Ok(Some(Arc::new(doubao::DoubaoVideoGen::from_env()?))),
        other => anyhow::bail!("unknown {ENV_VIDEO_GEN_PROVIDER}: {other}"),
    }
}
