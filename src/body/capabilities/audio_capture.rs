//! Native microphone capture — raw 16 kHz mono PCM straight from the OS, no browser.
//!
//! Inbound speech normally arrives over the WebSocket from a page's `getUserMedia`.
//! This captures the mic **in-process** instead, so a headless gesture — the
//! press-and-hold-⌘ attention ([`crate::body::gesture`]) — can listen with no page open.
//! The frames it yields match the pipeline's contract exactly (16 kHz mono signed
//! 16-bit little-endian PCM), so they feed
//! [`crate::foundation::server::audio::ingest_pcm_stream`] the same as the browser mic.
//!
//! Like the other desktop capabilities ([`super::hotkey`], [`super::screencast`],
//! [`super::input`]) the vendor is the operating system, selected at compile time;
//! on a platform without an impl this reports unavailable and the gesture's listen
//! half is simply inert.

use bytes::Bytes;
use tokio::sync::mpsc;

/// A live capture. **Dropping it stops the mic** and ends the frame stream, which
/// lets a downstream [`ingest_pcm_stream`](crate::foundation::server::audio::ingest_pcm_stream)
/// finalize on its own.
pub struct Capture {
    #[cfg(target_os = "macos")]
    _vendor: crate::foundation::vendors::macos_audio_capture::Capture,
}

/// Whether this build can capture the mic natively. Compile-time, not a permission
/// check — a macOS build still needs the **Microphone** grant for frames to flow.
pub fn available() -> bool {
    cfg!(target_os = "macos")
}

/// Start capturing the default input device as 16 kHz mono 16-bit PCM. Returns a
/// [`Capture`] (drop to stop) and the receiver of frames. Errs when capture is
/// unavailable on this platform or the device can't be opened.
pub fn start() -> anyhow::Result<(Capture, mpsc::Receiver<Bytes>)> {
    #[cfg(target_os = "macos")]
    {
        let (vendor, frames) = crate::foundation::vendors::macos_audio_capture::start()?;
        Ok((Capture { _vendor: vendor }, frames))
    }
    #[cfg(not(target_os = "macos"))]
    {
        anyhow::bail!("native mic capture is not supported on this platform")
    }
}
