//! Vision capability — image and video understanding (visual input → text).
//!
//! The returned text is the whole point: cognition here is text-only, so vision
//! is the perception path that lets visual signal enter the same symbol stream
//! as everything else.
//!
//! The capability is a module of free functions over a process-global,
//! once-initialized config: [`init_from_env`] reads `VISION_PROVIDER`,
//! [`available`] reports whether a provider is configured, and [`understand`]
//! dispatches to it. The config never appears in a signature.
//!
//! **No caller wires this in yet.** `POST /api/in/vision` still only persists
//! frames. This module is the capability a future, deliberately-triggered
//! perception path will call; wiring it in later is purely additive.

use std::sync::OnceLock;

use bytes::Bytes;

use crate::foundation::vendors::doubao_vision;

/// A piece of visual input to understand. The two variants map to the two
/// content-part kinds the upstream distinguishes (`image_url` vs `video_url`);
/// each carries its bytes inline or points at a URL via [`MediaSource`].
#[derive(Debug, Clone)]
pub enum VisualMedia {
    Image(MediaSource),
    Video(MediaSource),
}

impl VisualMedia {
    /// Image from raw bytes + IANA mime (e.g. `image/jpeg`). Encoded to a base64
    /// `data:` URL by the vendor — the common case for a captured frame.
    pub fn image_bytes(bytes: Bytes, mime: impl Into<String>) -> Self {
        VisualMedia::Image(MediaSource::Bytes { bytes, mime: mime.into() })
    }

    /// Image referenced by a remote URL or a pre-built `data:` URL.
    pub fn image_url(url: impl Into<String>) -> Self {
        VisualMedia::Image(MediaSource::Url(url.into()))
    }

    /// Video from raw bytes + IANA mime (e.g. `video/mp4`). Encoded to a base64
    /// `data:` URL by the vendor. Large clips are better passed as a URL.
    pub fn video_bytes(bytes: Bytes, mime: impl Into<String>) -> Self {
        VisualMedia::Video(MediaSource::Bytes { bytes, mime: mime.into() })
    }

    /// Video referenced by a remote URL or a pre-built `data:` URL.
    pub fn video_url(url: impl Into<String>) -> Self {
        VisualMedia::Video(MediaSource::Url(url.into()))
    }
}

/// Where a piece of media's bytes come from: an already-usable URL (remote or
/// `data:`) passed through untouched, or raw bytes the vendor base64-encodes
/// into a `data:` URL at request time.
#[derive(Debug, Clone)]
pub enum MediaSource {
    Url(String),
    Bytes { bytes: Bytes, mime: String },
}

enum Backend {
    Disabled,
    Doubao(doubao_vision::Config),
}

static BACKEND: OnceLock<Backend> = OnceLock::new();

const ENV_PROVIDER: &str = "VISION_PROVIDER";

/// Resolve the provider from `VISION_PROVIDER` into the process-global config.
/// Unset or `none` disables the capability; an unknown name is an error.
/// Idempotent — the first init wins.
pub fn init_from_env() -> anyhow::Result<()> {
    let backend = match std::env::var(ENV_PROVIDER).unwrap_or_default().as_str() {
        "" | "none" => Backend::Disabled,
        "doubao" => Backend::Doubao(doubao_vision::Config::from_env()?),
        other => anyhow::bail!("unknown {ENV_PROVIDER}: {other}"),
    };
    let _ = BACKEND.set(backend);
    Ok(())
}

/// Whether a provider is configured.
pub fn available() -> bool {
    matches!(BACKEND.get(), Some(Backend::Doubao(_)))
}

/// Understand `media` under the instruction `prompt` (e.g. "Describe what you
/// see") and return the model's natural-language answer.
pub async fn understand(media: VisualMedia, prompt: &str) -> anyhow::Result<String> {
    match BACKEND.get() {
        Some(Backend::Doubao(cfg)) => doubao_vision::understand(cfg, media, prompt).await,
        _ => anyhow::bail!("vision not configured (set {ENV_PROVIDER})"),
    }
}
