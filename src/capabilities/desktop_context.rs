//! Desktop context capability — a snapshot of what the user is looking at and
//! holding, right now, on the machine this process runs on.
//!
//! This is the payload behind the "share my current context" desktop gesture
//! (e.g. double-tap Ctrl): the agent receives the user's visual field and
//! working material instead of a bare text prompt. One binary serves every
//! install shape; on a platform without an impl (Docker, headless server) the
//! capability simply reports unavailable and everything else works as before.
//!
//! Unlike the API-backed capabilities, the "vendor" here is the operating
//! system, so selection is compile-time (`cfg(target_os)`) rather than env
//! config — there is no `init_from_env` and nothing to configure. Every field
//! of the snapshot is best-effort: a missing OS permission (Screen Recording,
//! Automation) or an empty clipboard yields `None` for that field, never an
//! error, so one denied prompt does not take the whole gesture down. Failures
//! are logged at `warn` for diagnosability.
//!
//! **No caller wires this in yet.** The double-Ctrl hotkey listener and the
//! send-to-session path are the future callers; wiring them in later is purely
//! additive.

use bytes::Bytes;
use chrono::{DateTime, Utc};

/// Everything we could gather about the user's current desktop context.
/// `captured_at` is always present; every other field is best-effort.
#[derive(Debug, Clone)]
pub struct ContextSnapshot {
    pub captured_at: DateTime<Utc>,
    /// PNG of the main display.
    pub screenshot_png: Option<Bytes>,
    /// Text flavor of the clipboard, if any.
    pub clipboard_text: Option<String>,
    /// Image flavor of the clipboard as PNG, if any.
    pub clipboard_image_png: Option<Bytes>,
    /// Name of the frontmost application (e.g. `Safari`).
    pub frontmost_app: Option<String>,
    /// Title of the frontmost window (e.g. `flight booking — Safari`).
    pub frontmost_window_title: Option<String>,
    /// URL of the frontmost browser tab, when the frontmost app is a known
    /// scriptable browser.
    pub browser_url: Option<String>,
}

/// Whether this build has a capture impl for the current platform. Note this
/// is a compile-time fact, not a permission check — a macOS build in an SSH
/// session reports `true` but will capture little.
pub fn available() -> bool {
    cfg!(target_os = "macos")
}

/// Take a best-effort snapshot of the current desktop context.
/// Errs only where [`available`] is `false`.
pub async fn capture() -> anyhow::Result<ContextSnapshot> {
    #[cfg(target_os = "macos")]
    {
        Ok(crate::vendors::macos_desktop_context::capture().await)
    }
    #[cfg(not(target_os = "macos"))]
    {
        anyhow::bail!("desktop context capture is not supported on this platform")
    }
}
