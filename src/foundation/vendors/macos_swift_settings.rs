//! Bridge to the native SwiftUI Settings window ([`swift/HiSettings.swift`]).
//!
//! Phase 1 of the UI-arch refactor (see CLAUDE.md § "UI architecture"): the SwiftUI
//! window replaces the hand-laid objc2 preferences window while the Rust process still
//! owns the app. The window is a **client of the engine's local config API** — it
//! reads/writes settings over HTTP, not via FFI into engine state. The only FFI is the
//! single entry point [`hi_settings_open`], which Swift implements (`@_cdecl`) and
//! `build.rs` compiles + links on macOS.
//!
//! The tray's "Settings…" action calls [`open`], which reads the local server port from
//! the config store and hands it to Swift so the window can reach the API.

use std::path::PathBuf;
use std::sync::OnceLock;

use crate::foundation::credentials::{get_setting, KEY_SERVER_PORT};

/// The data dir, stashed at startup so [`open`] can read the server port without
/// threading it through the tray's menu-action plumbing (which carries no data dir).
static DATA_DIR: OnceLock<PathBuf> = OnceLock::new();

unsafe extern "C" {
    /// Open (or focus) the SwiftUI Settings window. Defined in `swift/HiSettings.swift`.
    /// Must be called on the main thread (the caller — a tray menu action — already is;
    /// Swift also re-dispatches to main defensively). `port` is the local HTTP server's
    /// port, used to build the API base URL.
    fn hi_settings_open(port: u16);
}

/// Record the data dir so [`open`] can resolve the server port. Called once from the
/// tray setup, mirroring where the old `macos_settings::install` was wired.
pub fn init(data_dir: PathBuf) {
    let _ = DATA_DIR.set(data_dir);
}

/// Open the native Settings window. Reads the server port from the config store and
/// calls into Swift. No-op (with a warning) if the port isn't known yet.
pub fn open() {
    let Some(data_dir) = DATA_DIR.get() else {
        tracing::warn!("settings: open() before init(); ignoring");
        return;
    };
    let port = get_setting(data_dir, KEY_SERVER_PORT)
        .and_then(|s| s.trim().parse::<u16>().ok());
    let Some(port) = port else {
        tracing::warn!("settings: server port unknown; cannot open the window yet");
        return;
    };
    // SAFETY: `hi_settings_open` is the Swift `@_cdecl` entry linked by build.rs; it
    // takes a plain scalar and re-dispatches to the main thread internally.
    unsafe { hi_settings_open(port) };
}
