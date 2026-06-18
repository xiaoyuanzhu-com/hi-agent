//! Output sequencer — turns the mind's `say`/`show_view` tool calls into paced
//! speech and views.
//!
//! With output expressed as tool calls (not parsed from the reply stream), the
//! calls arrive on the `/mcp` HTTP handler — a different task than the per-scene
//! loop, which is busy awaiting the prompt. So each scene runs one sequencer
//! task that owns the turn's TTS span and view pacing. It receives an ordered run
//! of [`Beat`]s — a `TurnStart`, then the turn's `Say`/`Show` calls in arrival
//! order, then a `TurnEnd` — and renders them onto the reactor's outbound seam.
//!
//! The buffer is the whole point: a tool call is accepted into this queue and
//! acked immediately, so the mind never waits on synthesis or client playback. A
//! `Show` flushes the pending spoken sentence first (so a view lands right as its
//! sentence begins, not racing ahead), exactly as the old inline pacing did —
//! only now driven by tool-call order rather than document order.

use std::time::{Duration, Instant};

use tokio::sync::{mpsc, oneshot};
use tokio::task::JoinHandle;
use uuid::Uuid;

use crate::capabilities::tts::{self, TtsStream};
use crate::segment::{Segmenter, Terminator};
use crate::types::{Channel, Scene, ViewOp};

use super::{OutboundSignal, Reactor, interleave};

/// Quiet window after the last `Say` before the open /thought utterance is
/// closed. Within-utterance chunk gaps are sub-second; a gap this long means the
/// mind has moved on to tool work, and holding the reply body open would strand
/// the polling client.
const UTTERANCE_QUIET_CLOSE: Duration = Duration::from_secs(3);

/// One ordered unit the sequencer renders. `Say`/`Show` come from the mind's
/// tool calls (via [`super::ToolSink`]); `TurnStart`/`TurnEnd` bracket a turn and
/// are sent by [`super::run_turn`]. `TurnEnd` carries a one-shot the sequencer
/// fills with the turn's spoken reply, so the loop can size the context budget and
/// log the turn.
pub(super) enum Beat {
    TurnStart { turn: u64 },
    Say(String),
    Show { id: Option<String>, op: String, source: String },
    TurnEnd { done: oneshot::Sender<String> },
}

/// One scene's sequencer task. Drains `beats` for the life of the scene, holding
/// the current turn's pacing state between beats.
pub(super) async fn run_sequencer(reactor: Reactor, scene: Scene, mut beats: mpsc::Receiver<Beat>) {
    // Per-turn state, reset on each TurnStart. The TTS span is opened lazily on the
    // first `Say` so a silent turn emits no audio span at all.
    let mut turn: u64 = 0;
    // Stays false until the first TurnStart. Say/Show beats arriving before any turn
    // is bracketed — i.e. from the warm-up prompt, which pre-sends the system prompt
    // ahead of the first real turn — are dropped, so warm-up never reaches the user
    // even if the model emits speech with nothing to act on.
    let mut armed = false;
    let mut splitter = Segmenter::new(Terminator, Instant::now());
    let mut synth_tx: Option<mpsc::Sender<String>> = None;
    let mut synth_handle: Option<JoinHandle<()>> = None;
    let mut full_reply = String::new();
    // The open /thought utterance, if any, and when speech last landed on it. A
    // turn is not one utterance: the mind may say a sentence, then work tools for
    // minutes — leaving the reply body open that whole time strands the client
    // (its long-poll times out mid-utterance, and the bus then replays the text to
    // the next poll). A pause ends an utterance, like speech: after
    // [`UTTERANCE_QUIET_CLOSE`] without a `Say`, close /thought; a later `Say` in
    // the same turn simply opens the next utterance (the bus auto-opens on push).
    let mut quiet_deadline: Option<tokio::time::Instant> = None;

    loop {
        let beat = match quiet_deadline {
            Some(deadline) => tokio::select! {
                b = beats.recv() => match b {
                    Some(b) => b,
                    None => break,
                },
                _ = tokio::time::sleep_until(deadline) => {
                    super::emit_end_of_utterance(&reactor, &scene).await;
                    quiet_deadline = None;
                    continue;
                }
            },
            None => match beats.recv().await {
                Some(b) => b,
                None => break,
            },
        };
        match beat {
            Beat::TurnStart { turn: t } => {
                turn = t;
                armed = true;
                splitter = Segmenter::new(Terminator, Instant::now());
                synth_tx = None;
                synth_handle = None;
                full_reply.clear();
                quiet_deadline = None;
            }
            Beat::Say(text) => {
                if !armed || text.is_empty() {
                    continue;
                }
                // A barge-in landed on this turn: abandon the rest of what we were
                // going to say. Tear the voice span down (trailing frames cease)
                // and emit no more text — the mind re-forms its reply next turn
                // with the interruption note in hand. The cut turn keeps streaming
                // beats (fix-forward never cancels the prompt), so every trailing
                // one is dropped here until the next TurnStart clears the flag.
                if reactor.inner.interrupts.should_skip(&scene, turn).await {
                    if synth_tx.take().is_some() {
                        synth_handle = None;
                    }
                    if quiet_deadline.take().is_some() {
                        super::emit_end_of_utterance(&reactor, &scene).await;
                    }
                    continue;
                }
                if synth_tx.is_none() {
                    open_tts(&reactor, &scene, turn, &mut synth_tx, &mut synth_handle).await;
                }
                full_reply.push_str(&text);
                for emit in interleave::speak_emits(&text, &mut splitter, Instant::now()) {
                    super::perform(emit, &synth_tx, &reactor, &scene).await;
                }
                // /thought gets the raw chunk; TTS gets coalesced sentences (above).
                super::emit_thought_chunk(&reactor, &scene, text).await;
                quiet_deadline = Some(tokio::time::Instant::now() + UTTERANCE_QUIET_CLOSE);
            }
            Beat::Show { id, op, source } => {
                if !armed {
                    continue;
                }
                if reactor.inner.interrupts.should_skip(&scene, turn).await {
                    continue;
                }
                let (id, op) = resolve_view(id, &op);
                for emit in interleave::view_emits(&mut splitter, id, op, source) {
                    super::perform(emit, &synth_tx, &reactor, &scene).await;
                }
            }
            Beat::TurnEnd { done } => {
                // Flush the splitter's trailing partial sentence to TTS only.
                if let Some(tail) = splitter.flush() {
                    if let Some(tx) = &synth_tx {
                        let _ = tx.send(tail).await;
                    }
                }
                // Dropping the text sender signals end-of-input to the TTS session;
                // its drain task forwards trailing frames, then emits this turn's
                // AudioEnd on its own. We don't await it — it's turn-tagged, so the
                // next turn's span never collides with it.
                synth_tx = None;
                synth_handle = None;
                // Close the /thought utterance for this turn, unless the quiet
                // timer already did.
                if quiet_deadline.take().is_some() {
                    super::emit_end_of_utterance(&reactor, &scene).await;
                }
                if !full_reply.trim().is_empty() {
                    crate::channel_log::outbound(Channel::Text, &scene, full_reply.trim());
                }
                let _ = done.send(std::mem::take(&mut full_reply));
            }
        }
    }
}

/// Open this turn's streaming TTS span: announce it on the outbound seam
/// (`AudioBegin` carries the codec so the wire can set Content-Type first), then
/// spawn the frame drain. No-op when TTS is unconfigured — the turn is silent.
async fn open_tts(
    reactor: &Reactor,
    scene: &Scene,
    turn: u64,
    synth_tx: &mut Option<mpsc::Sender<String>>,
    synth_handle: &mut Option<JoinHandle<()>>,
) {
    if !tts::available() {
        return;
    }
    match tts::start().await {
        Ok(TtsStream { mime, text, frames }) => {
            let out = reactor.inner.out.clone();
            let _ = out
                .send(OutboundSignal::AudioBegin { scene: scene.clone(), turn, codec: mime })
                .await;
            // Stamp the voice span so a barge-in can be inferred against it
            // ("speech arrived while this turn was probably still sounding").
            reactor
                .inner
                .interrupts
                .audio_began(scene, turn, tokio::time::Instant::now())
                .await;
            let handle = tokio::spawn(super::forward_frames(frames, out, scene.clone(), turn));
            *synth_tx = Some(text);
            *synth_handle = Some(handle);
        }
        Err(err) => {
            tracing::warn!(scene = %scene, error = %err, "TTS session start failed; turn is silent");
        }
    }
}

/// Resolve a `show_view` call's raw arguments to an envelope id and op: an unknown
/// or missing op defaults to `show`; a missing id is synthesized (no animation
/// continuity, since only a reused id animates).
fn resolve_view(id: Option<String>, op: &str) -> (String, ViewOp) {
    let op = match op {
        "replace" => ViewOp::Replace,
        "dismiss" => ViewOp::Dismiss,
        _ => ViewOp::Show,
    };
    let id = id
        .filter(|s| !s.trim().is_empty())
        .unwrap_or_else(|| Uuid::now_v7().to_string());
    (id, op)
}
