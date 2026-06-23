//! Tray capability — a **menu-bar status item** that shows hi-agent is running and
//! offers a small menu (open the web UI, quit). The visible affordance of the
//! macOS desktop install shape.
//!
//! Like [`super::desktop_context`], [`super::hotkey`], [`super::input`], and
//! [`super::screencast`], the vendor is the operating system, selected at compile
//! time ([`crate::foundation::vendors::macos_tray`]) — there is nothing for the operator to
//! configure. Unlike those, this isn't a *sense*: it's an articulation/lifecycle
//! surface, so it has no place in a perception loop. It's driven once from the
//! macOS entry point ([`crate::run_with_tray`]).
//!
//! A status item is AppKit, which **must run on the process main thread** and own
//! the AppKit event loop — so [`run`] blocks the caller for the process lifetime.

use std::sync::Arc;

use tokio::sync::Notify;

/// Whether this build can show a menu-bar status item. Compile-time only — a macOS
/// build still needs a GUI login session (window server) for the item to actually
/// appear; without one, [`run`] errors and the caller falls back to headless.
pub fn available() -> bool {
    cfg!(target_os = "macos")
}

/// Show the status item and run the OS menu loop. **Blocks the calling thread for
/// the lifetime of the process** (it drives the AppKit run loop), so call it on the
/// process main thread. The menu's "Open" launches `url` in the default browser;
/// "Quit" calls `shutdown.notify_waiters()` so the server can drain gracefully.
///
/// Errors (rather than blocking) if the platform has no impl or the status item
/// can't be created — e.g. no window-server session (running over SSH / headless).
pub fn run(url: String, shutdown: Arc<Notify>) -> anyhow::Result<()> {
    #[cfg(target_os = "macos")]
    {
        crate::foundation::vendors::macos_tray::run(url, shutdown)
    }
    #[cfg(not(target_os = "macos"))]
    {
        let _ = (url, shutdown);
        anyhow::bail!("menu-bar status item is not supported on this platform")
    }
}

/// Briefly pulse the menu-bar icon to acknowledge that a user gesture was received
/// — today, the double-tap-Command "come and see this" ([`crate::body::gesture`]). Meant
/// as an *instant* ack of the gesture itself, not a signal that whatever it kicked
/// off has finished. Best-effort: a no-op off macOS, or when no status item is up
/// (headless, or before the tray has loaded).
pub fn flash() {
    #[cfg(target_os = "macos")]
    crate::foundation::vendors::macos_tray::flash();
}
