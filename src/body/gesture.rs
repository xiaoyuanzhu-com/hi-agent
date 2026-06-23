//! The Command-key gestures — two ways the user pulls the agent's attention with
//! one key, both **best-effort and macOS-only**:
//!
//! - **Double-tap ⌘ → "come and see this":** hands the agent a screenshot of the
//!   current screen. It is *not a new sense* — the screenshot lands exactly like a
//!   drag-dropped image (a handed file on the `file` channel) and wakes the mind
//!   ([`crate::foundation::server::files::receive_screenshot`]).
//! - **Press-and-hold ⌘ → continuous attention:** for as long as Command is held,
//!   the agent listens (native mic capture → the same audio ingest the browser mic
//!   uses) and may look at the screen (its existing `look` tool); on release it
//!   stops. No new processing path — the held speech rides the normal pipeline,
//!   carrying only a context note that it came from this headless gesture.
//!
//! The OS tap only emits raw [`Edge`](crate::body::capabilities::hotkey::Edge)s; the
//! recognizers and the hold's threshold timer run here, on the runtime, against one
//! clock. Observing the keys needs the **Accessibility / Input Monitoring** grant,
//! the screenshot needs **Screen Recording**, and the hold's mic needs
//! **Microphone**; any missing grant just makes that part inert, never fatal. On
//! non-macOS the whole thing is a no-op.

use std::sync::Arc;
use std::time::{Duration, Instant};

use crate::foundation::server::AppState;
use crate::types::Scene;

/// Arm the gestures: from now on a double-tap of Command hands the agent a
/// screenshot, and a press-and-hold opens continuous attention, both in `scene`.
/// Spawns the OS event-loop thread and the recognizer task and returns immediately.
/// Call once, after the reactor is running, from within the tokio runtime — it
/// captures the current runtime handle to drive the recognizers and the async
/// capture/ingest off the (blocking) event-loop thread.
#[cfg(target_os = "macos")]
pub fn install(state: Arc<AppState>, scene: Scene) {
    use crate::body::capabilities::hotkey;

    let scene_label = scene.to_string();
    let handle = tokio::runtime::Handle::current();

    // Raw Command-key edges flow from the OS tap thread to the async recognizer on
    // the runtime, which stamps them against its own clock (so the double-tap window
    // and the hold threshold share one clock with the timer below).
    let (edge_tx, edge_rx) = tokio::sync::mpsc::unbounded_channel::<hotkey::Edge>();
    handle.spawn(recognizer_loop(state, scene, edge_rx));

    let spawned = std::thread::Builder::new()
        .name("hotkey-gesture".to_string())
        .spawn(move || {
            let on_edge = move |e: hotkey::Edge| {
                // The tap callback must never block the OS run loop; unbounded +
                // non-blocking send. A closed receiver (recognizer gone) drops the edge.
                let _ = edge_tx.send(e);
            };
            // Blocks on the OS run loop for the process's life.
            if let Err(e) = hotkey::listen(on_edge) {
                tracing::warn!(error = %e, "gesture: Command-key listener unavailable; gestures disabled");
            }
        });
    match spawned {
        Ok(_) => tracing::info!(
            scene = %scene_label,
            "Command gestures armed (double-tap → screenshot, press-hold → attention)"
        ),
        Err(e) => tracing::warn!(error = %e, "gesture: could not spawn listener thread; gestures disabled"),
    }
}

/// Drive the recognizers off the edge stream. Double-tap and hold run side by side
/// over the one key: a quick second press fires a glance; a single press still down
/// past the threshold opens attention and its release closes it. The two are kept
/// from colliding — a completed double-tap (or any other key) disarms a pending
/// hold, and entering a hold cancels a half-formed double-tap.
#[cfg(target_os = "macos")]
async fn recognizer_loop(
    state: Arc<AppState>,
    scene: Scene,
    mut edges: tokio::sync::mpsc::UnboundedReceiver<crate::body::capabilities::hotkey::Edge>,
) {
    use crate::body::capabilities::hotkey::{self, Edge, GestureEvent};

    let start = Instant::now();
    let mut dt = hotkey::DoubleTap::new(hotkey::DEFAULT_WINDOW);
    let mut hold = hotkey::Hold::new(hotkey::DEFAULT_HOLD);
    let mut session: Option<MicSession> = None;

    loop {
        // Sleep until an armed press would cross the hold threshold; if none is
        // pending, wait forever (only an edge can wake us). Rebuilt each iteration
        // so it tracks the current pending press.
        let deadline = hold.next_deadline();
        let tick = async {
            match deadline {
                Some(d) => {
                    let now = start.elapsed().as_millis() as u64;
                    tokio::time::sleep(Duration::from_millis(d.saturating_sub(now))).await;
                }
                None => std::future::pending::<()>().await,
            }
        };

        tokio::select! {
            edge = edges.recv() => {
                let Some(edge) = edge else { break }; // tap thread gone
                let t = start.elapsed().as_millis() as u64;
                match edge {
                    Edge::CmdDown => {
                        hold.on_command_down(t);
                        if dt.on_command_down(t) {
                            // A completed double-tap: glance, and make sure this same
                            // press can't also become a hold.
                            dt.on_other_input();
                            hold.cancel();
                            glance(&state, &scene);
                        }
                    }
                    Edge::CmdUp => {
                        if hold.on_command_up(t) == Some(GestureEvent::HoldEnd) {
                            stop_attention(&mut session);
                        }
                    }
                    Edge::Other => {
                        dt.on_other_input();
                        hold.cancel();
                    }
                }
            }
            _ = tick => {
                let t = start.elapsed().as_millis() as u64;
                if hold.poll(t) == Some(GestureEvent::HoldStart) {
                    // Entering a hold cancels a half-formed double-tap so a later tap
                    // doesn't pair with this press.
                    dt.on_other_input();
                    start_attention(&state, &scene, &mut session);
                }
            }
        }
    }

    // Tap thread ended — make sure we aren't left attending.
    stop_attention(&mut session);
}

/// Capture the screen and hand it to the agent (the double-tap gesture). Flashes the
/// tray first as an instant ack of the *gesture* (before the async capture), then
/// spawns the capture + carrier ingest so a slow grab never stalls the recognizer.
#[cfg(target_os = "macos")]
fn glance(state: &Arc<AppState>, scene: &Scene) {
    crate::body::capabilities::tray::flash();
    let state = state.clone();
    let scene = scene.clone();
    tokio::spawn(async move {
        match crate::body::capabilities::screencast::grab_screen_png().await {
            Ok(png) => {
                if let Err(e) = crate::foundation::server::files::receive_screenshot(&state, &scene, &png).await {
                    tracing::warn!(error = %e, "gesture: handing screenshot to the agent failed");
                }
            }
            Err(e) => tracing::warn!(
                error = %e,
                "gesture: screen capture failed (Screen Recording permission?)"
            ),
        }
    });
}

/// The context note that rides the first utterance of a held-attention session, so
/// the mind knows this speech is live, headless, and screen-aware — and may look.
#[cfg(target_os = "macos")]
const ATTENTION_TAG: &str =
    "live attention: the user is holding ⌘ — showing you their screen and talking to \
     you right now; look at their screen if you need to, and respond as the conversation \
     warrants";

/// A held-attention session: the live mic capture feeding the audio ingest. Dropping
/// the capture stops the mic; the (detached) ingest then sees the stream end and
/// finalizes its last utterance on its own.
#[cfg(target_os = "macos")]
struct MicSession {
    _capture: crate::body::capabilities::audio_capture::Capture,
}

/// Open continuous attention: start native mic capture and feed it into the same
/// audio ingest the browser mic uses, tagged so the mind knows where it came from.
/// Idempotent (a re-entrant hold is ignored) and best-effort — no mic, no attention.
#[cfg(target_os = "macos")]
fn start_attention(state: &Arc<AppState>, scene: &Scene, session: &mut Option<MicSession>) {
    if session.is_some() {
        return; // already attending
    }
    if !crate::body::capabilities::audio_capture::available() {
        tracing::warn!("press-hold attention: native mic capture unavailable; nothing to listen with");
        return;
    }
    match crate::body::capabilities::audio_capture::start() {
        Ok((capture, frames)) => {
            crate::body::capabilities::tray::flash();
            let state = state.clone();
            let scene = scene.clone();
            tokio::spawn(async move {
                crate::foundation::server::audio::ingest_pcm_stream(
                    state,
                    scene,
                    None,
                    Some(ATTENTION_TAG.to_string()),
                    frames,
                )
                .await;
            });
            *session = Some(MicSession { _capture: capture });
            tracing::info!("press-hold attention: listening (mic open)");
        }
        Err(e) => tracing::warn!(
            error = %e,
            "press-hold attention: mic capture failed (Microphone permission?)"
        ),
    }
}

/// Close continuous attention: drop the capture, which stops the mic and lets the
/// ingest finalize. No-op when not attending.
#[cfg(target_os = "macos")]
fn stop_attention(session: &mut Option<MicSession>) {
    if session.take().is_some() {
        tracing::info!("press-hold attention: released (mic closed)");
    }
}

#[cfg(not(target_os = "macos"))]
pub fn install(_state: Arc<AppState>, _scene: Scene) {}
