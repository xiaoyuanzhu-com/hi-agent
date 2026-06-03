//! Reactor — the *mind*. Per-scene queues + one persistent session per scene.
//!
//! One mpsc per scene, one task per scene; turns run serially against a single
//! ACP session that is opened on the scene's first turn and reused forever as
//! the scene's continuous mind. Cognition is delegated to that session; the
//! reactor never blocks on it.
//!
//! ## Turn-taking lives here, not in the client
//!
//! The client is a dumb face: it streams the mic and renders what arrives. It
//! does not decide *when* the agent speaks — the mind does, and these are the
//! two rules:
//!
//! 1. **Commit-after-quiet.** A finalized utterance does not immediately make
//!    the agent reply. The human often speaks in bursts; each burst arrives as
//!    its own inbound signal (one segmented utterance over `/api/in/audio`), and the mind
//!    waits until no new signal has landed for a short settle before it
//!    responds, absorbing every burst in the meantime into one consolidated
//!    prompt. The cost is a little latency; the win is that the agent doesn't
//!    answer a half-finished thought, and nothing the human says is lost.
//!    Because the reply only starts once things have gone quiet, its output can
//!    stream straight to the client — no holding, no turn-tagging on the wire;
//!    superseded drafts are *never generated* rather than generated-then-discarded.
//! 2. **Fix-forward, no reflexive cancel.** A new signal never cancels the
//!    in-flight prompt. The per-scene loop is serial — it runs one turn to
//!    completion before draining the next batch — so a signal that lands during
//!    generation simply queues and is folded into the next turn. The warm
//!    session remembers fragments it chose not to act on yet, so a thought spread
//!    across several bursts reassembles across turns; the mind corrects course
//!    rather than being cut off. (The client mutes its own speaker reflexively the
//!    instant its mic goes hot, so an interruption feels instant regardless.)
//!
//! ## Heavy work goes to a working session, not onto the floor
//!
//! The mind keeps a single voice, so it must never block the floor on slow
//! work. When a turn needs research, multi-step tool use, or anything
//! long-running, the reply names the task inside `[[delegate]] … [[/delegate]]`
//! markers; the reactor spawns a channel-mute [`workers`] session for it and
//! keeps talking. The worker runs with the same substrate (memory, tools) but
//! no voice of its own, and posts its result — or a question, if it gets
//! stuck — back into this scene's queue, where it lands as just another input
//! the next turn folds into what the mind says.

use std::collections::HashMap;
use std::path::Path;
use std::sync::Arc;
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::Duration;

mod heartbeat;
mod interleave;
pub mod outbound;
mod workers;

pub use outbound::OutboundSignal;

/// The heartbeat's soft context-budget ceiling, surfaced so the observatory can
/// render each scene's budget as a fraction of where a hot-swap kicks in.
pub fn swap_budget_chars() -> usize {
    heartbeat::SWAP_AFTER_CHARS
}

use chrono::{DateTime, Utc};
use tokio::sync::{Mutex, mpsc};
use tokio::time::{Instant, sleep_until, timeout};

use crate::acp::{AcpSession, SessionOpts, SessionUpdate};
use crate::agent::AgentLayer;
use crate::capabilities::tts::{self, TtsStream};
use crate::memory::{Memory, build_for_scene};
use crate::observatory::{EventKind, Observatory, SessionKind};
use crate::segment::{Segmenter, Terminator};
use crate::types::{Channel, JournalEntry, Scene, Signal, SurfaceEnvelope};
use bytes::Bytes;

/// How long the floor must stay quiet after the last finalized utterance before
/// the mind commits to replying. The human-interface tradeoff knob: higher =
/// more patient (never talks over a multi-burst thought) but more latency;
/// lower = snappier but more likely to answer a half-finished thought. Paired
/// with the client VAD's `endSilenceMs`, which governs how fast an utterance is
/// *finalized* (and POSTed); this governs how long we wait to see if another one
/// follows.
const RESPONSE_SETTLE: Duration = Duration::from_millis(700);

/// Built-in base soul, embedded at compile time from `default_soul.md`. The
/// agent's identity — how it speaks, what it values, how it renders surfaces — is
/// authored here as a tracked asset, so it ships in the binary and updates
/// transparently with every build. [`load_soul`] always uses this as the base,
/// layering an optional `<data_dir>/SOUL.md` on top.
const DEFAULT_SOUL: &str = include_str!("default_soul.md");

/// Separator that introduces the operator's override layer. Placed after the
/// bundled base so its instructions take precedence — the model honors the
/// later, more specific guidance where the two conflict.
const OVERRIDE_HEADER: &str = "\n\n# Operator overrides\n\nThe operator added the guidance below. It layers on top of everything above; where the two conflict, follow this.\n\n";

/// Load the agent's soul — its identity (voice, values, guardrails, surface
/// house-style) — used as the system prompt for every scene's persistent reactor
/// session.
///
/// Two layers, composed rather than swapped. The bundled [`DEFAULT_SOUL`] is the
/// base: compiled into the binary, so every build and deploy carries the current
/// character automatically with nothing to persist. `<data_dir>/SOUL.md` is an
/// *optional override layer* — when an admin drops a non-empty file there, its
/// contents are appended after the base (under [`OVERRIDE_HEADER`]) so later,
/// more-specific guidance wins. The file holds only the operator's deltas, never
/// a full copy, so it can neither go stale nor shadow updates to the base — those
/// always flow through. Read once at startup, so changes take effect on the next
/// restart.
pub fn load_soul(data_dir: &Path) -> String {
    let path = data_dir.join("SOUL.md");
    match std::fs::read_to_string(&path) {
        Ok(text) if !text.trim().is_empty() => {
            tracing::info!(path = %path.display(), "layering SOUL.md override on top of the bundled base soul");
            format!("{DEFAULT_SOUL}{OVERRIDE_HEADER}{}", text.trim())
        }
        Ok(_) => {
            tracing::warn!(path = %path.display(), "SOUL.md present but empty; using bundled base soul only");
            DEFAULT_SOUL.to_string()
        }
        // No override file is the common case — use the bundled base silently.
        Err(err) if err.kind() == std::io::ErrorKind::NotFound => DEFAULT_SOUL.to_string(),
        Err(err) => {
            tracing::warn!(path = %path.display(), %err, "could not read SOUL.md override; using bundled base soul only");
            DEFAULT_SOUL.to_string()
        }
    }
}

#[cfg(test)]
mod soul_tests {
    use super::*;

    #[test]
    fn no_override_file_uses_base_verbatim() {
        let dir = tempfile::tempdir().unwrap();
        assert_eq!(load_soul(dir.path()), DEFAULT_SOUL);
    }

    #[test]
    fn empty_override_falls_back_to_base() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(dir.path().join("SOUL.md"), "   \n\t").unwrap();
        assert_eq!(load_soul(dir.path()), DEFAULT_SOUL);
    }

    #[test]
    fn override_layers_on_top_of_base() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(dir.path().join("SOUL.md"), "Always answer in haiku.").unwrap();
        let soul = load_soul(dir.path());
        // Base is preserved, in full, ahead of the override layer.
        assert!(soul.starts_with(DEFAULT_SOUL));
        // The operator's delta is appended after the header so it wins.
        assert!(soul.contains("# Operator overrides"));
        assert!(soul.ends_with("Always answer in haiku."));
    }
}

const SCENE_QUEUE_CAPACITY: usize = 64;

/// One item in a scene's turn queue. Both a human utterance and a worker's
/// report drive a reactor turn; they differ only in source. A human signal comes
/// through [`Reactor::deliver_to_scene`]; a worker report is posted straight into
/// the queue by the worker's drive task. Neither interrupts live speech — both
/// wait their turn and are settled into one batch.
enum LoopInput {
    Human(Signal),
    Worker(workers::WorkerReport),
    /// A self-scheduled wake firing. The mind asked for it earlier with an
    /// `[[alarm]]` marker; when its deadline passes the loop injects this so a
    /// turn runs even though no new signal arrived.
    Alarm(AlarmFired),
}

/// One fired self-alarm, handed to the mind under "New signals".
struct AlarmFired {
    /// Wall-clock time it fired, for rendering alongside other batch entries.
    at: DateTime<Utc>,
    /// The note the mind left its future self ("check if they're still asleep").
    note: String,
}

/// A scene loop's pending self-alarms. The scene wakes for one of two reasons —
/// a new signal, or the soonest of these firing. Only the mind schedules them,
/// by emitting `[[alarm]]` markers. A flat Vec is plenty: a scene has at most a
/// handful pending at once.
struct Alarms {
    pending: Vec<PendingAlarm>,
}

struct PendingAlarm {
    fire_at: Instant,
    note: String,
}

impl Alarms {
    fn new() -> Self {
        Self { pending: Vec::new() }
    }

    /// Register a wake `delay` from `now` carrying `note`.
    fn schedule(&mut self, delay: Duration, note: String, now: Instant) {
        self.pending.push(PendingAlarm { fire_at: now + delay, note });
    }

    /// The soonest pending deadline, or `None` if nothing is scheduled — the
    /// loop then blocks on the inbound queue with no timer arm at all.
    fn next_deadline(&self) -> Option<Instant> {
        self.pending.iter().map(|a| a.fire_at).min()
    }

    /// Remove and return every alarm whose deadline has passed by `now`.
    fn take_due(&mut self, now: Instant) -> Vec<AlarmFired> {
        let at = Utc::now();
        let mut fired = Vec::new();
        let mut i = 0;
        while i < self.pending.len() {
            if self.pending[i].fire_at <= now {
                let a = self.pending.swap_remove(i);
                fired.push(AlarmFired { at, note: a.note });
            } else {
                i += 1;
            }
        }
        fired
    }
}

const OPEN_ALARM: &str = "[[alarm]]";
const CLOSE_ALARM: &str = "[[/alarm]]";

/// Parse an `[[alarm]]` delay token: a bare integer is seconds, or an integer
/// with an `s`/`m`/`h` suffix (`30s`, `20m`, `1h`). `None` for anything
/// unparseable, so a malformed alarm is dropped rather than firing at a wrong
/// time.
fn parse_delay(tok: &str) -> Option<Duration> {
    let tok = tok.trim();
    let (digits, mult) = if let Some(n) = tok.strip_suffix(|c| c == 's' || c == 'S') {
        (n, 1)
    } else if let Some(n) = tok.strip_suffix(|c| c == 'm' || c == 'M') {
        (n, 60)
    } else if let Some(n) = tok.strip_suffix(|c| c == 'h' || c == 'H') {
        (n, 3600)
    } else {
        (tok, 1)
    };
    let n: u64 = digits.trim().parse().ok()?;
    Some(Duration::from_secs(n.saturating_mul(mult)))
}

/// Parse one `[[alarm]]` block body — `"<delay> <note>"` — and register it: the
/// first whitespace-delimited token is the delay, the rest is the note. A block
/// whose delay won't parse is logged and dropped.
async fn schedule_alarm(reactor: &Reactor, alarms: &mut Alarms, scene: &Scene, block: &str) {
    let (delay_tok, note) = match block.split_once(char::is_whitespace) {
        Some((d, rest)) => (d, rest.trim()),
        None => (block, ""),
    };
    match parse_delay(delay_tok) {
        Some(delay) => {
            alarms.schedule(delay, note.to_owned(), Instant::now());
            reactor
                .inner
                .observatory
                .record(
                    scene,
                    EventKind::AlarmScheduled { note: note.to_owned(), delay_s: delay.as_secs() },
                )
                .await;
            tracing::info!(scene = %scene, delay_s = delay.as_secs(), note = %note, "alarm scheduled");
        }
        None => {
            tracing::warn!(scene = %scene, token = %delay_tok, "ignoring [[alarm]] with unparseable delay");
        }
    }
}

#[derive(Clone)]
pub struct Reactor {
    inner: Arc<ReactorInner>,
}

struct ReactorInner {
    memory: Memory,
    agent: AgentLayer,
    /// The admin-authored identity seeded into every scene's reactor session as
    /// its system prompt (see [`load_soul`]). Loaded once at startup, shared
    /// read-only across scenes; the heartbeat re-seeds replacement sessions with
    /// it too, so a hot-swapped mind keeps the same character.
    soul: String,
    /// The reactor's single outbound seam: every channel signal it produces —
    /// text, synthesized speech, surfaces — goes out here in transport-free form
    /// (see [`outbound`]). A transport adapter binds these to a wire. The reactor
    /// has no knowledge of HTTP, `Content-Type`, or response framing.
    out: mpsc::Sender<OutboundSignal>,
    /// Structured visibility into the session lifecycle. Turn, session, swap,
    /// worker and alarm events feed it; the HTTP front serves it.
    observatory: Observatory,
    /// Monotonic cognition-turn counter. Each turn claims the next id;
    /// it tags audio spans and the channel logs so a reply is traceable end to
    /// end. (The client no longer needs it — turns are internal to the mind.)
    turn_seq: AtomicU64,
    scenes: Mutex<HashMap<Scene, SceneHandle>>,
}

struct SceneHandle {
    inbound: mpsc::Sender<LoopInput>,
}

pub fn start(
    memory: Memory,
    agent: AgentLayer,
    soul: String,
    mut inbound_rx: mpsc::Receiver<Signal>,
    out: mpsc::Sender<OutboundSignal>,
    observatory: Observatory,
) -> Reactor {
    let reactor = Reactor {
        inner: Arc::new(ReactorInner {
            memory,
            agent,
            soul,
            out,
            observatory,
            turn_seq: AtomicU64::new(0),
            scenes: Mutex::new(HashMap::new()),
        }),
    };
    let dispatch_reactor = reactor.clone();

    tokio::spawn(async move {
        while let Some(signal) = inbound_rx.recv().await {
            let scene = signal.scene.clone();
            dispatch_reactor.deliver_to_scene(scene, signal).await;
        }
        tracing::warn!("reactor inbound channel closed; dispatch loop exiting");
    });

    reactor
}

impl Reactor {
    async fn deliver_to_scene(&self, scene: Scene, signal: Signal) {
        let sender = self.get_or_create_scene(scene.clone()).await;

        // A new signal never cancels the in-flight prompt: the serial per-scene
        // loop folds it into the next turn (fix-forward), and the lightweight
        // reactor decides per turn whether to act or wait for the rest.
        if let Err(err) = sender.send(LoopInput::Human(signal)).await {
            tracing::error!(scene = %scene, error = %err, "scene inbound channel closed; dropping signal");
        }
    }

    async fn get_or_create_scene(&self, scene: Scene) -> mpsc::Sender<LoopInput> {
        let mut scenes = self.inner.scenes.lock().await;
        if let Some(handle) = scenes.get(&scene) {
            return handle.inbound.clone();
        }

        let (tx, rx) = mpsc::channel::<LoopInput>(SCENE_QUEUE_CAPACITY);
        scenes.insert(scene.clone(), SceneHandle { inbound: tx.clone() });
        drop(scenes);

        let task_reactor = self.clone();
        let task_scene = scene.clone();
        // The worker registry posts its reports back into this same queue, so
        // hand the loop a sender clone to seed it.
        let task_worker_inbound = tx.clone();
        tokio::spawn(async move {
            per_scene_loop(task_reactor, task_scene, rx, task_worker_inbound).await;
        });

        tx
    }
}

async fn per_scene_loop(
    reactor: Reactor,
    scene: Scene,
    mut inbound: mpsc::Receiver<LoopInput>,
    worker_inbound: mpsc::Sender<LoopInput>,
) {
    // The scene's persistent reactor session: opened lazily on the first turn,
    // then reused for every later turn as the scene's continuous mind. Only this
    // loop touches it, so a plain local `Option` suffices; the heartbeat swap
    // below replaces it in place, between turns.
    let mut reactor_session: Option<Arc<AcpSession>> = None;
    // Tracks how much the live session has accumulated, so we know when to
    // hot-swap it before it rots or overflows.
    let mut budget = heartbeat::ContextBudget::new();
    // The scene's live working sessions. Heavy/tool-using work the reactor
    // delegates runs here; workers post progress and results back through
    // `worker_inbound` into this same loop.
    let mut workers = workers::WorkerRegistry::new(scene.clone(), worker_inbound);
    // Self-alarms the mind has scheduled. They give the loop a second reason to
    // wake — time passing — on top of an incoming signal; see the `select!` below.
    let mut alarms = Alarms::new();
    loop {
        // Wait for the first reason to wake: a new signal, or — when the mind has
        // set alarms — the soonest firing. An alarm wake injects synthetic batch
        // items so a turn runs even with no new signal; the mind then looks at the
        // situation and decides what to do (including nothing).
        let mut batch: Vec<LoopInput> = Vec::new();
        match alarms.next_deadline() {
            Some(deadline) => {
                tokio::select! {
                    recvd = inbound.recv() => match recvd {
                        Some(s) => batch.push(s),
                        None => {
                            tracing::info!(scene = %scene, "per-scene inbound closed; exiting loop");
                            return;
                        }
                    },
                    _ = sleep_until(deadline) => {
                        for fired in alarms.take_due(Instant::now()) {
                            reactor
                                .inner
                                .observatory
                                .record(&scene, EventKind::AlarmFired { note: fired.note.clone() })
                                .await;
                            batch.push(LoopInput::Alarm(fired));
                        }
                    }
                }
            }
            None => match inbound.recv().await {
                Some(s) => batch.push(s),
                None => {
                    tracing::info!(scene = %scene, "per-scene inbound closed; exiting loop");
                    return;
                }
            },
        }

        // A timer can resolve with nothing actually due; don't run an empty turn.
        if batch.is_empty() {
            continue;
        }

        // Commit-after-quiet: wait for things to settle before replying. Keep
        // absorbing utterances; each one that lands resets the wait. When the
        // settle elapses with nothing new, commit to a reply.
        let closed = loop {
            while let Ok(extra) = inbound.try_recv() {
                batch.push(extra);
            }
            match timeout(RESPONSE_SETTLE, inbound.recv()).await {
                Ok(Some(extra)) => batch.push(extra), // another utterance — keep collecting
                Ok(None) => break true,               // inbound closed mid-settle
                Err(_) => break false,                // quiet elapsed → commit to a reply
            }
        };
        if closed {
            tracing::info!(scene = %scene, "per-scene inbound closed; exiting loop");
            return;
        }

        // Forget any workers that have finished, so the registry doesn't grow.
        workers.reap();

        match run_turn(&reactor, &scene, &batch, &mut reactor_session, &mut budget, &mut workers, &mut alarms).await {
            Ok(()) => {
                // Between turns: if the live session has grown past budget, hot-swap
                // it now. The human is consuming the reply just delivered, so the
                // summarize-and-reopen happens in that natural gap — invisible, never
                // a cold restart. A swap failure leaves the warm session in place.
                if budget.should_swap() {
                    if let Some(current) = reactor_session.clone() {
                        match heartbeat::swap(&reactor, &scene, &current).await {
                            Ok(fresh) => {
                                reactor_session = Some(fresh);
                                budget.reset();
                                tracing::info!(scene = %scene, "reactor session hot-swapped");
                            }
                            Err(err) => {
                                tracing::warn!(scene = %scene, error = %err, "hot-swap failed; keeping warm session");
                            }
                        }
                    }
                }
            }
            Err(err) => {
                tracing::warn!(scene = %scene, error = %err, "turn failed");
                // Discard the possibly-wedged session; the next turn cold-opens a
                // fresh one and rebuilds context from the journal snapshot.
                if let Some(dead) = reactor_session.take() {
                    reactor
                        .inner
                        .observatory
                        .record(
                            &scene,
                            EventKind::SessionClosed {
                                kind: SessionKind::Reactor,
                                id: dead.id().0.to_string(),
                            },
                        )
                        .await;
                }
                budget.reset();
                reactor.inner.observatory.set_budget(&scene, 0).await;
            }
        }
    }
}

/// One turn: prompt the scene's persistent reactor session (opening it on the
/// first turn), stream text updates to `/thought`, and broadcast
/// `EndOfUtterance` when the turn ends.
///
/// A cold session — just opened, or discarded after an error — is seeded with
/// the journal snapshot, since it has no memory of prior turns. A warm session
/// already lived through them, so it gets only the new signals; the snapshot is
/// the durable backstop, not per-turn context to re-send.
async fn run_turn(
    reactor: &Reactor,
    scene: &Scene,
    batch: &[LoopInput],
    reactor_session: &mut Option<Arc<AcpSession>>,
    budget: &mut heartbeat::ContextBudget,
    workers: &mut workers::WorkerRegistry,
    alarms: &mut Alarms,
) -> anyhow::Result<()> {
    let turn_id = reactor.inner.turn_seq.fetch_add(1, Ordering::Relaxed);
    reactor
        .inner
        .observatory
        .record(
            scene,
            EventKind::TurnStarted { turn: turn_id, input: preview(&render_batch(batch)) },
        )
        .await;

    // What the delegated workers are doing right now, so the live session can
    // nudge one, wait, or fold a finished result into its reply. Empty when
    // nothing is delegated.
    let worker_status = workers.render_status().await;
    let new_signals = format!("## New signals\n{}", render_batch(batch));

    // Seed the journal snapshot only when cold; a warm session already lived the
    // history and gets only what's new (plus the live worker view). The snapshot
    // is the durable backstop, not per-turn context to re-send.
    let prompt_text = match reactor_session.as_ref() {
        Some(_) => join_sections(&[&worker_status, &new_signals]),
        None => {
            let snap = build_for_scene(&reactor.inner.memory, scene).await?;
            join_sections(&[&snap.render_for_prompt(), &worker_status, &new_signals])
        }
    };
    let prompt_chars = prompt_text.chars().count();

    // Get-or-open the scene's persistent reactor session. Opened once, carrying
    // the system prompt — which the session consumes on its first prompt and
    // never re-sends. Every later turn prompts this same warm session, so
    // continuity lives in the session, not in a per-turn rebuild.
    let session = match reactor_session {
        Some(s) => s.clone(),
        None => {
            let opened = Arc::new(
                reactor
                    .inner
                    .agent
                    .session(
                        scene,
                        SessionOpts {
                            system_prompt: Some(reactor.inner.soul.clone()),
                            cwd: None,
                        },
                    )
                    .await?,
            );
            reactor
                .inner
                .observatory
                .record(
                    scene,
                    EventKind::SessionOpened {
                        kind: SessionKind::Reactor,
                        id: opened.id().0.to_string(),
                    },
                )
                .await;
            *reactor_session = Some(opened.clone());
            opened
        }
    };

    let outcome: anyhow::Result<usize> = async {
        let mut run = session.prompt(prompt_text).await?;

        // Per-turn streaming TTS: open ONE synthesis session for the whole turn.
        // Audio frames stream back on a drain task as a single Start/Frame*/End
        // run on /audio, so a turn's speech is one continuous stream rather than
        // per-sentence clips; the session stays open across sentences. The drain
        // loop below owns this text sender and coalesces sentences into it. All of
        // this exists only when TTS is configured.
        let (synth_tx, synth_handle) = if tts::available() {
            match tts::start().await {
                Ok(TtsStream { mime, text, frames }) => {
                    let out = reactor.inner.out.clone();
                    // Announce the span first so the adapter can open a response
                    // with the right Content-Type before any frame arrives.
                    let _ = out
                        .send(OutboundSignal::AudioBegin {
                            scene: scene.clone(),
                            turn: turn_id,
                            codec: mime,
                        })
                        .await;
                    let handle =
                        tokio::spawn(forward_frames(frames, out, scene.clone(), turn_id));
                    (Some(text), Some(handle))
                }
                Err(err) => {
                    tracing::warn!(scene = %scene, error = %err, "TTS session start failed; turn is silent");
                    (None, None)
                }
            }
        } else {
            (None, None)
        };

        // Per-turn output state. The reply is parsed into ordered segments
        // (`interleave::Extractor`) and each is released just-in-time below,
        // decoupled from parse-time — so a surface is paced to its sentence
        // instead of racing ahead of all audio. The splitter coalesces spoken text
        // into sentences for TTS; the delegate/alarm extractors pull side-effect
        // markers out of the spoken run; `full_reply` is logged once at end of turn.
        let mut extractor = interleave::Extractor::new();
        let mut splitter = Segmenter::new(Terminator, std::time::Instant::now());
        let mut delegate_extractor = MarkerExtractor::new(OPEN_DELEGATE, CLOSE_DELEGATE);
        let mut alarm_extractor = MarkerExtractor::new(OPEN_ALARM, CLOSE_ALARM);
        let mut full_reply = String::new();

        let mut ended = false;
        while !ended {
            let segs = match run.next_update().await {
                Some(SessionUpdate::Text(text)) => extractor.push(&text),
                Some(SessionUpdate::Thought(_)) => continue, // internal reasoning
                Some(SessionUpdate::ToolCall(stub)) => {
                    tracing::debug!(scene = %scene, variant = stub.raw_variant, "tool call");
                    continue;
                }
                Some(SessionUpdate::Other(name)) => {
                    tracing::trace!(scene = %scene, variant = %name, "ignored ACP update");
                    continue;
                }
                // End of stream: release the extractor's held-back tail through the
                // same body, then leave the loop.
                None => {
                    ended = true;
                    extractor.flush().into_iter().collect()
                }
            };

            for seg in segs {
                match seg {
                    interleave::Segment::Spoken(text) => {
                        // `[[delegate]]` / `[[alarm]]` are side-effects pulled out
                        // here (spawn a worker / schedule a wake), never spoken.
                        let (clean, tasks) = delegate_extractor.push(&text);
                        for task in tasks {
                            if let Err(err) = workers.spawn(reactor, task).await {
                                tracing::warn!(scene = %scene, error = %err, "failed to spawn working session");
                            }
                        }
                        let (residual, blocks) = alarm_extractor.push(&clean);
                        for block in blocks {
                            schedule_alarm(reactor, alarms, scene, &block).await;
                        }
                        if residual.is_empty() {
                            continue;
                        }
                        full_reply.push_str(&residual);
                        for emit in interleave::speak_emits(
                            &residual,
                            &mut splitter,
                            std::time::Instant::now(),
                        ) {
                            perform(emit, &synth_tx, reactor, scene).await;
                        }
                        // /thought gets the raw residual chunk; TTS gets coalesced
                        // sentences (above) — the two channels keep their own pacing.
                        emit_thought_chunk(reactor, scene, residual).await;
                    }
                    interleave::Segment::Surface(env) => {
                        for emit in interleave::surface_emits(&mut splitter, env) {
                            perform(emit, &synth_tx, reactor, scene).await;
                        }
                    }
                }
            }
        }

        // The marker extractors may still hold a partial-opener tail; flush them
        // (delegate first, feeding its tail through alarm, mirroring the streaming
        // chain) so a marker that ended the reply still resolves.
        let deleg_tail = delegate_extractor.flush();
        let (mut spoken_tail, alarm_blocks) = alarm_extractor.push(&deleg_tail);
        spoken_tail.push_str(&alarm_extractor.flush());
        for block in alarm_blocks {
            schedule_alarm(reactor, alarms, scene, &block).await;
        }
        if !spoken_tail.is_empty() {
            full_reply.push_str(&spoken_tail);
            for emit in
                interleave::speak_emits(&spoken_tail, &mut splitter, std::time::Instant::now())
            {
                perform(emit, &synth_tx, reactor, scene).await;
            }
            emit_thought_chunk(reactor, scene, spoken_tail).await;
        }
        if !full_reply.trim().is_empty() {
            crate::channel_log::outbound(Channel::Text, scene, full_reply.trim());
        }
        // Flush the splitter's trailing partial sentence to TTS only (no /thought —
        // it was already mirrored as part of its raw chunk).
        if let Some(tail) = splitter.flush() {
            if let Some(tx) = &synth_tx {
                let _ = tx.send(tail).await;
            }
        }

        let mut cancelled = false;
        let stop_reason = match run.wait().await {
            Ok(result) => {
                tracing::debug!(scene = %scene, stop = ?result.stop_reason, "turn finished");
                Some(format!("{:?}", result.stop_reason))
            }
            Err(err) => {
                cancelled = true;
                tracing::debug!(scene = %scene, error = %err, "turn run ended with error (likely cancel)");
                None
            }
        };
        reactor
            .inner
            .observatory
            .record(
                scene,
                EventKind::TurnFinished {
                    turn: turn_id,
                    stop_reason,
                    reply_chars: full_reply.chars().count(),
                    reply: preview(&full_reply),
                },
            )
            .await;

        // Dropping the text sender signals end-of-input: the TTS session sends
        // FinishSession, the drain task forwards trailing frames, then emits the
        // turn's `End`. If the run ended in error (a wedged or crashed session,
        // not a human interruption — those no longer cancel) abort the drain so
        // stale frames aren't spoken, and emit `End` ourselves so any open
        // GET /audio response for this turn closes promptly.
        drop(synth_tx);
        if let Some(handle) = synth_handle {
            if cancelled {
                handle.abort();
                let _ = reactor
                    .inner
                    .out
                    .send(OutboundSignal::AudioEnd {
                        scene: scene.clone(),
                        turn: turn_id,
                    })
                    .await;
            }
        }
        Ok(full_reply.chars().count())
    }
    .await;

    // End of utterance — closes the GET /thought response that's been
    // streaming this turn's chunks.
    emit_end_of_utterance(reactor, scene).await;

    // The session is persistent — do NOT drop it. The caller's `reactor_session`
    // slot keeps the warm session alive for the next turn.

    // Fold this turn's size into the budget so the loop can decide whether the
    // session has grown enough to hot-swap. Only on success — a failed turn is
    // discarded along with its (possibly wedged) session.
    let reply_chars = outcome?;
    budget.record_turn(prompt_chars, reply_chars);
    reactor.inner.observatory.set_budget(scene, budget.chars()).await;
    Ok(())
}

async fn emit_thought_chunk(reactor: &Reactor, scene: &Scene, text: String) {
    let ts = Utc::now();
    let entry = JournalEntry::SignalOut {
        ts,
        channel: Channel::Text,
        scene: scene.clone(),
        body: text.clone(),
        media_path: None,
    };
    if let Err(err) = reactor.inner.memory.journal.append(entry).await {
        tracing::error!(scene = %scene, error = %err, "journal append failed for outbound thought");
    }
    let _ = reactor
        .inner
        .out
        .send(OutboundSignal::Text {
            scene: scene.clone(),
            chunk: text,
        })
        .await;
}

/// Carry one release action to its wire carrier: speech to TTS, a surface to
/// /surface. Thought mirroring and the once-per-turn reply log are handled inline
/// by the caller, since they track the raw spoken chunk rather than the paced
/// emits.
async fn perform(
    emit: interleave::Emit,
    synth_tx: &Option<mpsc::Sender<String>>,
    reactor: &Reactor,
    scene: &Scene,
) {
    match emit {
        interleave::Emit::Speak(sentence) => {
            if let Some(tx) = synth_tx {
                let _ = tx.send(sentence).await;
            }
        }
        interleave::Emit::Show(env) => emit_surface(reactor, scene, env).await,
    }
}

async fn emit_end_of_utterance(reactor: &Reactor, scene: &Scene) {
    let _ = reactor
        .inner
        .out
        .send(OutboundSignal::TextEnd { scene: scene.clone() })
        .await;
}

/// Join non-empty prompt sections with a blank line between them, trimming each.
/// Lets a turn assemble whichever of {snapshot, worker status, new signals}
/// actually have content without leaving stray blank headers.
fn join_sections(sections: &[&str]) -> String {
    sections
        .iter()
        .map(|s| s.trim())
        .filter(|s| !s.is_empty())
        .collect::<Vec<_>>()
        .join("\n\n")
}

/// Cap a message at a sane length for an observatory event. The session log is
/// a developer view, not a transcript store; a long reply is truncated with an
/// ellipsis rather than streaming kilobytes through the SSE feed and the ring.
fn preview(s: &str) -> String {
    const MAX: usize = 2000;
    let s = s.trim();
    if s.chars().count() <= MAX {
        return s.to_string();
    }
    let head: String = s.chars().take(MAX).collect();
    format!("{head}…")
}

fn render_batch(batch: &[LoopInput]) -> String {
    use std::fmt::Write as _;
    let mut s = String::new();
    for input in batch {
        match input {
            LoopInput::Human(sig) => {
                let ts = sig.ts.format("%H:%M:%S");
                let _ = writeln!(
                    s,
                    "[{}] {} on /{}: \"{}\"",
                    ts, sig.scene, sig.channel, sig.body
                );
            }
            LoopInput::Worker(report) => {
                let _ = writeln!(s, "{}", workers::render_report(report));
            }
            LoopInput::Alarm(a) => {
                let ts = a.at.format("%H:%M:%S");
                let _ = writeln!(s, "[{}] (alarm) \"{}\"", ts, a.note);
            }
        }
    }
    s
}

/// Background task: drain one turn's synthesized audio frames onto the /audio
/// channel, emitting an `AudioFrame` per chunk and a closing `AudioEnd`. The
/// span's `AudioBegin` (which carries the codec) is sent by the caller before
/// this task is spawned. Send errors are ignored — no subscriber connected is
/// fine. Logs the turn's total bytes once at the end; the spoken text is already
/// logged on /thought.
async fn forward_frames(
    mut frames: mpsc::Receiver<Bytes>,
    out: mpsc::Sender<OutboundSignal>,
    scene: Scene,
    turn: u64,
) {
    let mut total = 0usize;
    while let Some(bytes) = frames.recv().await {
        total += bytes.len();
        let _ = out
            .send(OutboundSignal::AudioFrame {
                scene: scene.clone(),
                turn,
                bytes,
            })
            .await;
    }
    let _ = out
        .send(OutboundSignal::AudioEnd {
            scene: scene.clone(),
            turn,
        })
        .await;
    tracing::info!(
        target: "channel",
        dir = "out",
        channel = "audio",
        scene = %scene,
        turn = turn,
        bytes = total,
        "channel out (tts stream)",
    );
}

/// Emit one rich-content envelope on the /surface channel for this scene.
async fn emit_surface(reactor: &Reactor, scene: &Scene, envelope: SurfaceEnvelope) {
    tracing::info!(
        target: "channel",
        dir = "out",
        channel = "surface",
        scene = %scene,
        op = ?envelope.op,
        mode = ?envelope.mode,
        html_len = envelope.html.as_deref().map(str::len).unwrap_or(0),
        "channel out (surface)",
    );
    let _ = reactor
        .inner
        .out
        .send(OutboundSignal::Surface {
            scene: scene.clone(),
            envelope,
        })
        .await;
}

const OPEN_DELEGATE: &str = "[[delegate]]";
const CLOSE_DELEGATE: &str = "[[/delegate]]";

/// Streaming extractor for a single `OPEN … CLOSE` marker pair. Text outside the
/// markers passes through; each enclosed block's inner content is collected and
/// returned. A short tail that could be a partial opener is held back so a marker
/// split across chunks is still recognized.
///
/// The generic sibling of the surface extractor in [`interleave`] (which carries
/// a card/full mode and yields envelopes). Used for `[[delegate]]` on the reactor
/// side — the mind names background work inline — and for `[[ask]]` on the
/// worker side.
struct MarkerExtractor {
    open: &'static str,
    close: &'static str,
    buf: String,
    inside: bool,
}

impl MarkerExtractor {
    fn new(open: &'static str, close: &'static str) -> Self {
        Self {
            open,
            close,
            buf: String::new(),
            inside: false,
        }
    }

    /// Feed a chunk. Returns `(text_outside_markers, blocks_completed_this_call)`.
    fn push(&mut self, chunk: &str) -> (String, Vec<String>) {
        self.buf.push_str(chunk);
        let mut text_out = String::new();
        let mut blocks = Vec::new();

        loop {
            if self.inside {
                if let Some(idx) = self.buf.find(self.close) {
                    let inner = self.buf[..idx].trim().to_string();
                    self.buf = self.buf[idx + self.close.len()..].to_string();
                    self.inside = false;
                    if !inner.is_empty() {
                        blocks.push(inner);
                    }
                    continue;
                }
                break; // close not present yet; keep buffering the block body
            } else {
                if let Some(idx) = self.buf.find(self.open) {
                    text_out.push_str(&self.buf[..idx]);
                    self.buf = self.buf[idx + self.open.len()..].to_string();
                    self.inside = true;
                    continue;
                }
                // No opener: emit everything except a tail that could be the
                // start of one continuing in the next chunk.
                let keep = partial_marker_suffix_len(&self.buf, self.open);
                let emit_to = self.buf.len() - keep;
                text_out.push_str(&self.buf[..emit_to]);
                self.buf = self.buf[emit_to..].to_string();
                break;
            }
        }
        (text_out, blocks)
    }

    /// Emit any held-back text at end of stream. An unterminated block is dropped.
    fn flush(&mut self) -> String {
        let out = if self.inside {
            String::new()
        } else {
            std::mem::take(&mut self.buf)
        };
        self.buf.clear();
        self.inside = false;
        out
    }
}

/// Length (bytes) of the longest suffix of `buf` that is a proper prefix of
/// `marker` — i.e. a marker possibly split across chunks.
fn partial_marker_suffix_len(buf: &str, marker: &str) -> usize {
    let max = marker.len() - 1;
    let start = buf.len().saturating_sub(max);
    for i in start..buf.len() {
        if !buf.is_char_boundary(i) {
            continue;
        }
        let suffix = &buf[i..];
        if marker.starts_with(suffix) {
            return buf.len() - i;
        }
    }
    0
}

#[cfg(test)]
mod alarm_tests {
    use super::{Alarms, parse_delay};
    use std::time::Duration;
    use tokio::time::Instant;

    #[test]
    fn parse_delay_reads_units() {
        assert_eq!(parse_delay("1200"), Some(Duration::from_secs(1200)));
        assert_eq!(parse_delay("30s"), Some(Duration::from_secs(30)));
        assert_eq!(parse_delay("20m"), Some(Duration::from_secs(1200)));
        assert_eq!(parse_delay("1h"), Some(Duration::from_secs(3600)));
        assert_eq!(parse_delay("  45  "), Some(Duration::from_secs(45)));
    }

    #[test]
    fn parse_delay_rejects_garbage() {
        assert_eq!(parse_delay("soon"), None);
        assert_eq!(parse_delay(""), None);
        assert_eq!(parse_delay("m"), None);
    }

    #[test]
    fn fires_in_deadline_order_and_keeps_the_rest() {
        let t0 = Instant::now();
        let mut alarms = Alarms::new();
        assert_eq!(alarms.next_deadline(), None);

        alarms.schedule(Duration::from_secs(60), "later".into(), t0);
        alarms.schedule(Duration::from_secs(10), "sooner".into(), t0);
        assert_eq!(alarms.next_deadline(), Some(t0 + Duration::from_secs(10)));

        // Nothing due before the soonest deadline.
        assert!(alarms.take_due(t0 + Duration::from_secs(5)).is_empty());

        // At 10s only "sooner" fires; the 60s one stays pending.
        let fired = alarms.take_due(t0 + Duration::from_secs(10));
        assert_eq!(fired.len(), 1);
        assert_eq!(fired[0].note, "sooner");
        assert_eq!(alarms.next_deadline(), Some(t0 + Duration::from_secs(60)));

        // Past the last deadline the remaining one fires and the queue empties.
        let fired = alarms.take_due(t0 + Duration::from_secs(120));
        assert_eq!(fired.len(), 1);
        assert_eq!(fired[0].note, "later");
        assert_eq!(alarms.next_deadline(), None);
    }
}
