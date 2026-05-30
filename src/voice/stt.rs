//! Speech-to-text capability trait.
//!
//! An `Stt` impl turns audio bytes into text. The trait is provider-agnostic;
//! see `volcengine_stt` for the one concrete impl shipping in v0. Swapping
//! providers is one file in `voice/`.

use async_trait::async_trait;
use bytes::Bytes;
use tokio::sync::mpsc;

/// One transcript update from a streaming recognition.
///
/// The upstream is two-pass: it emits a fast, rolling preliminary text
/// (`is_final = false`) that keeps changing as you speak, then a polished,
/// punctuated/ITN-corrected final (`is_final = true`) for the utterance.
#[derive(Debug, Clone)]
pub struct Transcript {
    pub text: String,
    pub is_final: bool,
}

#[async_trait]
pub trait Stt: Send + Sync {
    /// Transcribe `audio` (raw bytes) labeled with the given IANA mime type
    /// (e.g. `audio/wav`, `audio/mpeg`). The impl is responsible for any
    /// format-specific framing; the trait stays format-agnostic.
    async fn transcribe(&self, audio: Bytes, mime: &str) -> anyhow::Result<String>;

    /// Streaming transcription. Consumes 16 kHz mono 16-bit little-endian PCM
    /// chunks from `audio_rx` until it closes, emitting incremental and final
    /// [`Transcript`]s on `out` as the upstream produces them. Returns the
    /// final transcript text. `out` send errors (receiver gone) are non-fatal;
    /// recognition continues so the final can still be returned for journaling.
    async fn transcribe_streaming(
        &self,
        audio_rx: mpsc::Receiver<Bytes>,
        out: mpsc::Sender<Transcript>,
    ) -> anyhow::Result<String>;
}
