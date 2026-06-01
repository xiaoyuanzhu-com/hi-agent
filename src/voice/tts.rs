//! Text-to-speech capability trait.
//!
//! A `Tts` impl renders text into audio. Synthesis is a *streaming session*,
//! not a one-shot call: [`Tts::start`] opens a session, the caller pushes text
//! incrementally as the agent produces it, and audio frames stream back as they
//! are synthesized. One session spans a whole turn, so the audio is one
//! continuous stream rather than a sequence of per-sentence clips — the brain
//! consolidates, the client just plays.

use async_trait::async_trait;
use bytes::Bytes;
use tokio::sync::mpsc;

/// A live synthesis session. Feed text via [`text`](Self::text) as it becomes
/// available; drop the sender to signal end-of-input (the provider flushes any
/// trailing audio and closes). Drain [`frames`](Self::frames) for the audio
/// bytes; the receiver closes when synthesis ends or the session errors.
///
/// Every frame shares the same [`mime`](Self::mime) — it is fixed for the life
/// of the session and known the moment the session opens, so the HTTP layer can
/// set `Content-Type` before the first byte.
pub struct TtsStream {
    /// IANA mime type for every frame in this stream, e.g. `audio/mpeg`.
    pub mime: String,
    /// Push text to be spoken. Send each chunk as it arrives; dropping the
    /// sender signals that no more text is coming.
    pub text: mpsc::Sender<String>,
    /// Synthesized audio frames, in order. Closes when synthesis completes.
    pub frames: mpsc::Receiver<Bytes>,
}

#[async_trait]
pub trait Tts: Send + Sync {
    /// Open a streaming synthesis session. Returns once the session is ready to
    /// accept text; synthesis is driven by pushing text and draining frames.
    async fn start(&self) -> anyhow::Result<TtsStream>;
}
