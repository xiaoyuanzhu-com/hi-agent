//! Process-wide "is the managed account out of energy right now?" flag.
//!
//! In xiaoyuanzhu (managed) mode the whole account draws on one shared budget, so
//! **any** 402 means the same thing — the LLM (songguo), STT/TTS, or vision all
//! signal "out of energy" — and the freshly-fetched balance is the ground truth.
//! This flag collects those signals so the web app can raise the out-of-energy hint
//! the instant we notice, from whichever source noticed first:
//!   - [`note_402`] — a 402 from any managed capability (immediate, mid-session).
//!   - [`reconcile`] — the balance the broker keeps fresh (startup refresh + the 60s
//!     poll + the out-of-energy poller), which both raises when empty and clears on
//!     refill.
//! In BYOK a 402 is the user's own vendor account, not our energy — so [`note_402`]
//! is a no-op there. The `/api/account/energy` handler reads [`is_out`]. One process,
//! one account, one flag — a global keeps the wiring trivial.

use std::path::Path;
use std::sync::atomic::{AtomicBool, Ordering};

use crate::foundation::credentials::{Credentials, Mode};

static OUT_OF_ENERGY: AtomicBool = AtomicBool::new(false);

fn set(out: bool) {
    OUT_OF_ENERGY.store(out, Ordering::Relaxed);
}

/// Whether the managed account is currently out of energy (turns are held, not
/// dropped, and the paid capabilities will 402).
pub fn is_out() -> bool {
    OUT_OF_ENERGY.load(Ordering::Relaxed)
}

/// A 402 from any managed capability (LLM / STT / TTS / vision …) → out of energy,
/// but only in xiaoyuanzhu mode; a BYOK 402 is the user's own vendor account. Raises
/// the flag immediately so the hint doesn't wait for the next balance poll. `data_dir`
/// is read to check the mode.
pub fn note_402(data_dir: &Path) {
    if matches!(Credentials::load(data_dir).mode, Mode::Xiaoyuanzhu) {
        set(true);
    }
}

/// Reconcile against a freshly fetched managed balance: empty (`remaining <= 0`) →
/// out of energy, has budget → recovered. An unknown balance (`total <= 0`, i.e. not
/// yet fetched) is ignored so we never false-positive before the first poll. Callers
/// are the broker energy paths, which already run only in managed mode.
pub fn reconcile(remaining: i64, total: i64) {
    if total > 0 {
        set(remaining <= 0);
    }
}
