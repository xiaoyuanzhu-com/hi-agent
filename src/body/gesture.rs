//! The right-Command gestures — two ways the user pulls the agent's attention with
//! one key, both **best-effort and macOS-only**. Bound to the **right** Command alone
//! (left Command is the everyday shortcut modifier; see
//! [`crate::foundation::vendors::macos_hotkey`]):
//!
//! - **Double-tap the right ⌘ → "come and see this":** hands the agent a screenshot of
//!   the current screen. It is *not a new sense* — the screenshot lands exactly like a
//!   drag-dropped image (a handed file on the `file` channel) and wakes the mind
//!   ([`crate::foundation::server::files::receive_screenshot`]).
//! - **Press-and-hold the right ⌘ → continuous attention:** for as long as the key is
//!   held, the agent listens (native mic capture → the same audio ingest the browser mic
//!   uses) and may look at the screen (its existing `look` tool); on release it
//!   stops. The mic opens early (after a short capture threshold) and buffers a
//!   pre-roll, but that audio is only *processed* once the press also crosses the
//!   full hold threshold — so a genuine hold loses almost no leading speech while an
//!   accidental quick press opens nothing to process. No new processing path — the
//!   held speech rides the normal pipeline, carrying only a context note that it came
//!   from this headless gesture.
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

#[cfg(target_os = "macos")]
use bytes::Bytes;
#[cfg(target_os = "macos")]
use std::collections::VecDeque;
#[cfg(target_os = "macos")]
use tokio::sync::mpsc;

/// Arm the gestures: from now on a double-tap of the right Command hands the agent a
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
    // The follower mirrors the held conversation onto the menu-bar item's single
    // text field — the transcript while you hold, then the reply, in sequence.
    let (attn_tx, attn_rx) = tokio::sync::mpsc::unbounded_channel::<AttnEvent>();
    handle.spawn(tray_text_follower(state.clone(), scene.clone(), attn_rx));
    handle.spawn(recognizer_loop(state, scene, edge_rx, attn_tx));

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
            "right-Command gestures armed (double-tap → screenshot, press-hold → attention)"
        ),
        Err(e) => tracing::warn!(error = %e, "gesture: could not spawn listener thread; gestures disabled"),
    }
}

/// What the recognizer tells the tray-text follower: a committed hold began, or
/// it ended (release). Glance doesn't go through here — it's a self-contained
/// pulse on the tray icon.
#[cfg(target_os = "macos")]
enum AttnEvent {
    ListenStart,
    ListenStop,
}

/// Mirror a held-attention session onto the menu-bar item's single text field: the
/// live transcript while the user speaks, then the agent's reply — in sequence (we
/// only have the one field). Spawned once and fed `AttnEvent`s by the recognizer;
/// it reads transcript/reply off the same non-draining echoes the channel inspector
/// uses ([`AppState::input_echo`] / [`AppState::output_echo`]). The reply usually
/// lands *after* release, so `ListenStop` lets the strip linger for a short dwell
/// that each reply chunk extends, then collapses the item back to icon-only.
#[cfg(target_os = "macos")]
async fn tray_text_follower(
    state: Arc<AppState>,
    scene: Scene,
    mut events: tokio::sync::mpsc::UnboundedReceiver<AttnEvent>,
) {
    use crate::body::capabilities::tray;
    use crate::types::Channel;
    use tokio::sync::broadcast::error::RecvError;

    // How long the strip lingers after release so the reply can show; each reply
    // chunk pushes it out again.
    const DWELL: Duration = Duration::from_millis(3500);
    // Keep the menu-bar item from ballooning past the notch — show only the tail.
    const MAX_CHARS: usize = 48;
    fn tail(s: &str) -> String {
        let chars: Vec<char> = s.chars().collect();
        if chars.len() <= MAX_CHARS {
            s.to_string()
        } else {
            let cut: String = chars[chars.len() - MAX_CHARS..].iter().collect();
            format!("…{cut}")
        }
    }

    let mut input_rx = state.input_echo.subscribe();
    let mut output_rx = state.output_echo.subscribe();
    let mut active = false;
    let mut reply = String::new();
    let mut deadline: Option<Instant> = None;

    loop {
        let dwell = async {
            match deadline {
                Some(d) => tokio::time::sleep_until(tokio::time::Instant::from_std(d)).await,
                None => std::future::pending::<()>().await,
            }
        };
        tokio::select! {
            ev = events.recv() => match ev {
                None => break, // recognizer gone
                Some(AttnEvent::ListenStart) => {
                    active = true;
                    reply.clear();
                    deadline = None;
                    // Re-subscribe so we start from the live edge (no stale lines).
                    input_rx = state.input_echo.subscribe();
                    output_rx = state.output_echo.subscribe();
                    tray::set_listening(true);
                    tray::set_text("聆听…");
                }
                Some(AttnEvent::ListenStop) => {
                    if active {
                        deadline = Some(Instant::now() + DWELL);
                    }
                }
            },
            r = input_rx.recv(), if active => match r {
                Ok(echo) => {
                    if echo.scene == scene && echo.channel == Channel::Text {
                        tray::set_text(&tail(&echo.text)); // listen phase
                    }
                }
                Err(RecvError::Lagged(_)) => {}
                Err(RecvError::Closed) => {}
            },
            r = output_rx.recv(), if active => match r {
                Ok(echo) => {
                    if echo.scene == scene && echo.channel == Channel::Text {
                        reply.push_str(&echo.text); // speak phase
                        tray::set_text(&tail(&reply));
                        if deadline.is_some() {
                            deadline = Some(Instant::now() + DWELL);
                        }
                    }
                }
                Err(RecvError::Lagged(_)) => {}
                Err(RecvError::Closed) => {}
            },
            _ = dwell, if deadline.is_some() => {
                active = false;
                reply.clear();
                deadline = None;
                tray::set_text("");
                tray::set_listening(false);
            }
        }
    }
}

/// Drive the recognizers off the edge stream. Double-tap and hold run side by side
/// over the one key: a quick second press fires a glance; a single press still down
/// opens attention in two stages — past a short capture threshold the mic opens and
/// buffers a pre-roll, past the full threshold that pre-roll is committed to live
/// processing — and its release closes it. The two are kept from colliding — a
/// completed double-tap (or any other key) disarms a pending hold (discarding a
/// buffering pre-roll), and committing to a hold cancels a half-formed double-tap.
#[cfg(target_os = "macos")]
async fn recognizer_loop(
    state: Arc<AppState>,
    scene: Scene,
    mut edges: tokio::sync::mpsc::UnboundedReceiver<crate::body::capabilities::hotkey::Edge>,
    attn_tx: tokio::sync::mpsc::UnboundedSender<AttnEvent>,
) {
    use crate::body::capabilities::hotkey::{self, Edge, GestureEvent};

    let start = Instant::now();
    let mut dt = hotkey::DoubleTap::new(hotkey::DEFAULT_WINDOW);
    let mut hold = hotkey::Hold::new(hotkey::DEFAULT_CAPTURE, hotkey::DEFAULT_HOLD);
    let mut session: Option<MicSession> = None;

    loop {
        // Sleep until an armed press would cross its next threshold (capture, then
        // hold); if none is pending, wait forever (only an edge can wake us). Rebuilt
        // each iteration so it tracks the current pending press.
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
                        // Release: end a committed attention, or discard a still-buffering
                        // pre-roll (released before the hold threshold). `on_command_up`
                        // clears the recognizer's pending press either way.
                        let _ = hold.on_command_up(t);
                        stop_attention(&attn_tx, &mut session);
                    }
                    Edge::Other => {
                        dt.on_other_input();
                        hold.cancel();
                        // A chord (e.g. ⌘C) breaks a half-formed hold: drop a buffering
                        // pre-roll, but leave an already-committed attention running.
                        discard_capture(&mut session);
                    }
                }
            }
            _ = tick => {
                let t = start.elapsed().as_millis() as u64;
                match hold.poll(t) {
                    // Stage 1: open the mic and start buffering, no processing yet.
                    Some(GestureEvent::CaptureStart) => arm_capture(&state, &scene, &mut session),
                    // Stage 2: commit the pre-roll to live processing. Cancels a
                    // half-formed double-tap so a later tap doesn't pair with this press.
                    Some(GestureEvent::HoldStart) => {
                        dt.on_other_input();
                        commit_capture(&attn_tx, &mut session);
                    }
                    _ => {}
                }
            }
        }
    }

    // Tap thread ended — make sure we aren't left attending.
    stop_attention(&attn_tx, &mut session);
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
    "live attention: the user is holding the right ⌘ — showing you their screen and talking to \
     you right now; look at their screen if you need to, and respond as the conversation \
     warrants";

/// The control message that promotes a buffering capture into live processing.
#[cfg(target_os = "macos")]
enum Ctrl {
    /// Stop buffering the pre-roll and start feeding the audio ingest — committing
    /// this press to continuous attention.
    Commit,
}

/// A held-attention session, in one of two states the host moves through: first a
/// *buffering* capture (mic open, pre-roll accumulating, `committed == false`), then —
/// once the press crosses the hold threshold — a *committed* one feeding the audio
/// ingest. Dropping it stops the mic; the pump task then either discards the pre-roll
/// (released/chorded before commit) or lets the ingest finalize (committed).
#[cfg(target_os = "macos")]
struct MicSession {
    /// Dropping this stops the mic and ends the frame stream the pump reads.
    _capture: crate::body::capabilities::audio_capture::Capture,
    /// Send `Ctrl::Commit` to promote the buffered pre-roll into live processing.
    ctrl: mpsc::Sender<Ctrl>,
    /// Whether this press has crossed the hold threshold (pre-roll promoted to ingest).
    committed: bool,
}

/// Stage 1 — open the mic and start buffering a pre-roll, *without* processing it yet.
/// Called when a press crosses the short capture threshold; minimizing the lost
/// leading audio while the press is still being confirmed as a hold. If it goes on to
/// cross the hold threshold the host calls [`commit_capture`]; otherwise the pre-roll
/// is dropped ([`discard_capture`] / [`stop_attention`]) and no transcript is produced.
/// Idempotent (a re-entrant capture is ignored) and best-effort — no mic, no attention.
#[cfg(target_os = "macos")]
fn arm_capture(state: &Arc<AppState>, scene: &Scene, session: &mut Option<MicSession>) {
    if session.is_some() {
        return; // already capturing
    }
    if !crate::body::capabilities::audio_capture::available() {
        tracing::warn!("press-hold attention: native mic capture unavailable; nothing to listen with");
        return;
    }
    match crate::body::capabilities::audio_capture::start() {
        Ok((capture, frames)) => {
            let (ctrl_tx, ctrl_rx) = mpsc::channel::<Ctrl>(1);
            tokio::spawn(pump_capture(state.clone(), scene.clone(), frames, ctrl_rx));
            *session = Some(MicSession { _capture: capture, ctrl: ctrl_tx, committed: false });
            tracing::info!("press-hold attention: capturing (mic open, buffering pre-roll)");
        }
        Err(e) => tracing::warn!(
            error = %e,
            "press-hold attention: mic capture failed (Microphone permission?)"
        ),
    }
}

/// Stage 2 — commit a buffering capture to continuous attention: tell the pump to flush
/// the pre-roll and start feeding the audio ingest. Called when the press crosses the
/// hold threshold. No-op if not capturing or already committed.
#[cfg(target_os = "macos")]
fn commit_capture(
    attn_tx: &tokio::sync::mpsc::UnboundedSender<AttnEvent>,
    session: &mut Option<MicSession>,
) {
    if let Some(s) = session.as_mut()
        && !s.committed
    {
        // Capacity-1 channel, freshly created and sent on once — a send only fails if
        // the pump is already gone (mic stopped), in which case there's nothing to commit.
        if s.ctrl.try_send(Ctrl::Commit).is_ok() {
            s.committed = true;
            // The follower lights the icon colour and shows the transcript/reply text.
            let _ = attn_tx.send(AttnEvent::ListenStart);
            tracing::info!("press-hold attention: listening (processing)");
        }
    }
}

/// Drop a still-buffering capture — mic closes, pre-roll discarded — for a chord that
/// breaks a pending hold. A press that has already *committed* is left attending: a key
/// pressed during attention must not drop it.
#[cfg(target_os = "macos")]
fn discard_capture(session: &mut Option<MicSession>) {
    if session.as_ref().is_some_and(|s| !s.committed) {
        session.take();
        tracing::info!("press-hold attention: discarded (chord broke the hold)");
    }
}

/// Close the session: drop the capture (mic stops). If it had committed, the detached
/// ingest sees the stream end and finalizes its last utterance; if it was only
/// buffering, the pre-roll is discarded and no transcript is produced. No-op when not
/// attending.
#[cfg(target_os = "macos")]
fn stop_attention(
    attn_tx: &tokio::sync::mpsc::UnboundedSender<AttnEvent>,
    session: &mut Option<MicSession>,
) {
    if let Some(s) = session.take() {
        if s.committed {
            // The follower keeps the strip up through the reply, then settles back.
            let _ = attn_tx.send(AttnEvent::ListenStop);
            tracing::info!("press-hold attention: released (mic closed)");
        } else {
            tracing::info!("press-hold attention: discarded (released before hold)");
        }
    }
}

/// The buffering/forwarding task behind a [`MicSession`]. Until it receives
/// `Ctrl::Commit` it accumulates incoming PCM as a bounded pre-roll; on commit it
/// spawns the same audio ingest the browser mic uses, flushes the pre-roll into it,
/// then forwards live frames. When the capture is dropped (mic stops) the frame stream
/// ends and the task exits — finalizing the ingest if it had committed (its `tx` drops,
/// ending the stream), or dropping the pre-roll if it had not.
#[cfg(target_os = "macos")]
async fn pump_capture(
    state: Arc<AppState>,
    scene: Scene,
    mut frames: mpsc::Receiver<Bytes>,
    mut ctrl: mpsc::Receiver<Ctrl>,
) {
    // Cap the pre-roll so an unexpectedly long buffering window can't grow unbounded.
    // In practice it only spans the capture→hold gap (~300 ms); this is a safety rail.
    const MAX_PREROLL_CHUNKS: usize = 20; // ~2 s at 100 ms/chunk
    let mut preroll: VecDeque<Bytes> = VecDeque::new();
    let mut out: Option<mpsc::Sender<Bytes>> = None;
    let mut ctrl_open = true;

    loop {
        tokio::select! {
            ctrl_msg = ctrl.recv(), if ctrl_open => {
                match ctrl_msg {
                    Some(Ctrl::Commit) => {
                        let (tx, rx) = mpsc::channel::<Bytes>(64);
                        tokio::spawn(crate::foundation::server::audio::ingest_pcm_stream(
                            state.clone(),
                            scene.clone(),
                            None,
                            Some(ATTENTION_TAG.to_string()),
                            rx,
                        ));
                        let mut gone = false;
                        for chunk in preroll.drain(..) {
                            if tx.send(chunk).await.is_err() {
                                gone = true; // ingest already gone
                                break;
                            }
                        }
                        if gone {
                            break;
                        }
                        out = Some(tx);
                        ctrl_open = false; // committed; no further control expected
                    }
                    None => ctrl_open = false, // session dropped without committing
                }
            }
            frame = frames.recv() => {
                match frame {
                    Some(b) => match &out {
                        Some(tx) => {
                            if tx.send(b).await.is_err() {
                                break; // ingest gone
                            }
                        }
                        None => {
                            preroll.push_back(b);
                            while preroll.len() > MAX_PREROLL_CHUNKS {
                                preroll.pop_front();
                            }
                        }
                    },
                    None => break, // capture dropped → mic stopped
                }
            }
        }
    }
    // On exit: a committed `out` (tx) drops here, ending the ingest's stream so it
    // finalizes; an uncommitted `preroll` drops, discarding the buffered audio.
}

#[cfg(not(target_os = "macos"))]
pub fn install(_state: Arc<AppState>, _scene: Scene) {}
