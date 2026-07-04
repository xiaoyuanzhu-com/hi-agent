//! Process-wide "are we out of energy right now?" flag.
//!
//! The reactor's shared vendor state machine ([`crate::body::reactor`]) is the source
//! of truth for whether the account has hit its energy ceiling (a gateway 402): it
//! flips this flag as it enters and leaves the out-of-energy state (both recovery
//! paths — a completed turn and the balance poller's refill — run through
//! `Vendor::note_success`). The `/api/account/energy` handler reads it so the web app
//! raises the out-of-energy hint the instant we stop driving turns, and drops it the
//! instant energy refills. One process, one vendor, one flag — a global keeps the
//! wiring trivial (no handle threaded through `lib.rs` / the server build).

use std::sync::atomic::{AtomicBool, Ordering};

static OUT_OF_ENERGY: AtomicBool = AtomicBool::new(false);

/// Record whether the account is currently out of energy. Called by the vendor
/// state machine on the flip into (`true`) and out of (`false`) that state.
pub fn set(out: bool) {
    OUT_OF_ENERGY.store(out, Ordering::Relaxed);
}

/// Whether the account is currently out of energy (turns are held, not dropped).
pub fn is_out() -> bool {
    OUT_OF_ENERGY.load(Ordering::Relaxed)
}
