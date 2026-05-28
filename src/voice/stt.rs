//! Speech-to-text capability trait.
//!
//! An `Stt` impl turns audio bytes into text. The trait is provider-agnostic;
//! see `volcengine_stt` for the one concrete impl shipping in v0. Swapping
//! providers is one file in `voice/`.

use async_trait::async_trait;
use bytes::Bytes;

#[async_trait]
pub trait Stt: Send + Sync {
    /// Transcribe `audio` (raw bytes) labeled with the given IANA mime type
    /// (e.g. `audio/wav`, `audio/mpeg`). The impl is responsible for any
    /// format-specific framing; the trait stays format-agnostic.
    async fn transcribe(&self, audio: Bytes, mime: &str) -> anyhow::Result<String>;
}
