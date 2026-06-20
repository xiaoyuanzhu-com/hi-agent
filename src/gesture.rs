//! The "come and see this" gesture — a double-tap of Command hands the agent a
//! screenshot of the user's current screen.
//!
//! It is **not a new sense**: the screenshot lands exactly like a drag-dropped
//! image (a handed file on the `file` channel) and wakes the mind, which infers
//! what the user wants and helps. The only net-new piece is this trigger;
//! everything downstream is the existing file carrier
//! ([`crate::server::files::receive_screenshot`]).
//!
//! macOS only and best-effort: observing the key chord needs the **Accessibility
//! / Input Monitoring** grant and capturing pixels needs **Screen Recording**;
//! missing either, the gesture stays inert and the rest of the agent runs
//! unaffected. On non-macOS it is a no-op.

use std::sync::Arc;

use crate::server::AppState;
use crate::types::Scene;

/// Arm the gesture: from now on a double-tap of Command captures the screen and
/// hands it to the agent in `scene`. Spawns a dedicated thread for the OS event
/// loop and returns immediately. Call once, after the reactor is running, from
/// within the tokio runtime — it captures the current runtime handle to schedule
/// the async capture + ingest off the (blocking) event-loop thread.
#[cfg(target_os = "macos")]
pub fn install(state: Arc<AppState>, scene: Scene) {
    use crate::capabilities::hotkey;

    let scene_label = scene.to_string();
    let handle = tokio::runtime::Handle::current();
    let spawned = std::thread::Builder::new()
        .name("hotkey-gesture".to_string())
        .spawn(move || {
            let on_fire = move || {
                let state = state.clone();
                let scene = scene.clone();
                // The tap callback runs on the OS run-loop thread; hop onto the
                // runtime for the async capture + carrier ingest.
                handle.spawn(async move {
                    match crate::capabilities::screencast::grab_screen_png().await {
                        Ok(png) => {
                            if let Err(e) =
                                crate::server::files::receive_screenshot(&state, &scene, &png).await
                            {
                                tracing::warn!(error = %e, "gesture: handing screenshot to the agent failed");
                            }
                        }
                        Err(e) => tracing::warn!(
                            error = %e,
                            "gesture: screen capture failed (Screen Recording permission?)"
                        ),
                    }
                });
            };
            // Blocks on the OS run loop for the process's life.
            if let Err(e) = hotkey::listen(hotkey::DEFAULT_WINDOW, on_fire) {
                tracing::warn!(
                    error = %e,
                    "gesture: double-tap-Command listener unavailable; gesture disabled"
                );
            }
        });
    match spawned {
        Ok(_) => tracing::info!(
            scene = %scene_label,
            "come-and-see-this gesture armed (double-tap Command)"
        ),
        Err(e) => tracing::warn!(error = %e, "gesture: could not spawn listener thread; gesture disabled"),
    }
}

#[cfg(not(target_os = "macos"))]
pub fn install(_state: Arc<AppState>, _scene: Scene) {}
