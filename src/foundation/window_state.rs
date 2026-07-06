//! Process-wide "is the native face window on screen right now?" flag, plus a wake
//! signal for the out-of-energy balance poller.
//!
//! The macOS face window is reused, not rebuilt: opening it calls [`set_open(true)`]
//! ([`crate::foundation::vendors::macos_window`] `present`), the close button calls
//! [`set_open(false)`] (`windowWillClose:`). The out-of-energy poller reads [`is_open`]
//! to pick its cadence — a few seconds while the user is looking, an hour while the
//! window is shut — and awaits [`opened`] so the closed→open edge triggers an immediate
//! re-check (the user may have just paid on the web while the window was hidden).
//!
//! Non-macOS builds never call the setters, so the window reads as closed (the slow
//! cadence) — the right default for a headless process nobody is watching, which the
//! reopen edge can't rescue. One process, one window, one flag; a global keeps the
//! wiring trivial (mirrors [`crate::foundation::energy_state`]).

use std::sync::OnceLock;
use std::sync::atomic::{AtomicBool, Ordering};

use tokio::sync::Notify;

static OPEN: AtomicBool = AtomicBool::new(false);

/// The closed→open edge signal. Lazily created so it needs no const-Notify support.
fn opened_notify() -> &'static Notify {
    static OPENED: OnceLock<Notify> = OnceLock::new();
    OPENED.get_or_init(Notify::new)
}

/// Record whether the native face window is on screen. The false→true edge (the window
/// coming to the front) wakes the out-of-energy poller so it re-checks the balance at
/// once — a payment made while the window was hidden shouldn't wait out the schedule.
pub fn set_open(open: bool) {
    let was = OPEN.swap(open, Ordering::Relaxed);
    if open && !was {
        opened_notify().notify_one();
    }
}

/// Whether the native face window is currently on screen.
pub fn is_open() -> bool {
    OPEN.load(Ordering::Relaxed)
}

/// Await the next closed→open transition. The out-of-energy poller selects on this to
/// cut its current sleep short and poll immediately when the window reappears.
pub fn opened() -> &'static Notify {
    opened_notify()
}
