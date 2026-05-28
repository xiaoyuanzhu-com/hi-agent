//! Text-to-speech capability trait.
//!
//! A `Tts` impl renders text into an audio blob. The trait is
//! provider-agnostic; see `volcengine_tts` for the v0 concrete impl. The
//! returned `AudioBlob` carries enough information for the audio long-poll to
//! stream the bytes with a correct `Content-Type` and for the media-storage
//! helper to pick a file extension.

use async_trait::async_trait;
use bytes::Bytes;

/// One synthesized audio clip — the bytes plus enough metadata to serve them.
pub struct AudioBlob {
    pub bytes: Bytes,
    /// IANA mime type, e.g. `audio/mpeg`, `audio/wav`, `audio/ogg`.
    pub mime: String,
    /// File extension to use under `data/media/audio/out/<id>.<ext>`. Kept
    /// `&'static str` because impls return one of a small fixed set.
    pub ext: &'static str,
}

#[async_trait]
pub trait Tts: Send + Sync {
    async fn synthesize(&self, text: &str) -> anyhow::Result<AudioBlob>;
}
