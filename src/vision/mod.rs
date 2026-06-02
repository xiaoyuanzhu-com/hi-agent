//! Vision capability — image and video understanding.
//!
//! A `Vision` impl turns a still image or a video into *text* (a description or
//! an answer to a prompt). That text is the whole point: cognition here is
//! text-only, so vision is the perception path that lets visual signal enter the
//! same symbol stream as everything else — the analogue of what STT does for
//! audio (see [`crate::voice`]).
//!
//! The trait is provider-agnostic; the one concrete impl shipping today is
//! [`doubao::DoubaoVision`] (Volcengine Ark, OpenAI-compatible). Swapping or
//! adding a provider is one file plus an arm in [`build_vision`].
//!
//! **No caller wires this in yet.** `POST /api/vision`
//! ([`crate::server::vision`]) still only persists frames — it does not call a
//! `Vision` provider, because journaling an understanding of every streamed
//! frame would flood the per-scene snapshot. This module is the capability that a
//! future, deliberately-triggered perception path will call; it is built and
//! tested standalone so that wiring it in later is purely additive.

use std::sync::Arc;

use async_trait::async_trait;
use bytes::Bytes;

pub mod doubao;

const ENV_VISION_PROVIDER: &str = "VISION_PROVIDER";

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
    /// `data:` URL by the provider — the common case for a captured frame.
    pub fn image_bytes(bytes: Bytes, mime: impl Into<String>) -> Self {
        VisualMedia::Image(MediaSource::Bytes { bytes, mime: mime.into() })
    }

    /// Image referenced by a remote URL or a pre-built `data:` URL.
    pub fn image_url(url: impl Into<String>) -> Self {
        VisualMedia::Image(MediaSource::Url(url.into()))
    }

    /// Video from raw bytes + IANA mime (e.g. `video/mp4`). Encoded to a base64
    /// `data:` URL by the provider. Large clips are better passed as a URL.
    pub fn video_bytes(bytes: Bytes, mime: impl Into<String>) -> Self {
        VisualMedia::Video(MediaSource::Bytes { bytes, mime: mime.into() })
    }

    /// Video referenced by a remote URL or a pre-built `data:` URL.
    pub fn video_url(url: impl Into<String>) -> Self {
        VisualMedia::Video(MediaSource::Url(url.into()))
    }
}

/// Where a piece of media's bytes come from: an already-usable URL (remote or
/// `data:`) passed through untouched, or raw bytes the provider base64-encodes
/// into a `data:` URL at request time.
#[derive(Debug, Clone)]
pub enum MediaSource {
    Url(String),
    Bytes { bytes: Bytes, mime: String },
}

#[async_trait]
pub trait Vision: Send + Sync {
    /// Understand `media` under the instruction `prompt` (e.g. "Describe what you
    /// see") and return the model's natural-language answer. The impl owns any
    /// format-specific framing; the trait stays transport- and provider-agnostic.
    async fn understand(&self, media: VisualMedia, prompt: &str) -> anyhow::Result<String>;
}

/// Construct the vision provider selected by `VISION_PROVIDER`, or `None` if
/// unset or `none`. An unknown provider name is an error so misconfiguration
/// surfaces at startup rather than at first use.
pub fn build_vision() -> anyhow::Result<Option<Arc<dyn Vision>>> {
    let provider = std::env::var(ENV_VISION_PROVIDER).unwrap_or_default();
    match provider.as_str() {
        "" | "none" => Ok(None),
        "doubao" | "volcengine" => {
            let vision = doubao::DoubaoVision::from_env()?;
            Ok(Some(Arc::new(vision)))
        }
        other => anyhow::bail!("unknown VISION_PROVIDER: {other}"),
    }
}
