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
//!    its own inbound signal (one VAD-segmented `POST /api/audio`), and the mind
//!    waits until no new signal has landed for a short settle before it
//!    responds, absorbing every burst in the meantime into one consolidated
//!    prompt. The cost is a little latency; the win is that the agent doesn't
//!    answer a half-finished thought, and nothing the human says is lost.
//!    Because the reply only starts once things have gone quiet, its output can
//!    stream straight to the client — no holding, no turn-tagging on the wire;
//!    superseded drafts are *never generated* rather than generated-then-discarded.
//! 2. **Barge-in.** If the human starts talking again *during* generation, the
//!    new signal cancels the in-flight *prompt* (`session/cancel`) — the
//!    persistent session itself stays warm; the loop re-settles and re-prompts
//!    with the merged batch. (The client mutes its
//!    own speaker reflexively the instant its mic goes hot, so the interruption
//!    feels instant regardless of this round-trip.)
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
pub mod outbound;
mod workers;

pub use outbound::OutboundSignal;

use chrono::{DateTime, Utc};
use tokio::sync::{Mutex, mpsc};
use tokio::time::{Instant, sleep_until, timeout};
use uuid::Uuid;

use crate::acp::{AcpSession, SessionOpts, SessionUpdate};
use crate::agent::AgentLayer;
use crate::memory::{Memory, build_for_scene};
use crate::types::{Channel, JournalEntry, Scene, Signal, SurfaceEnvelope, SurfaceMode, SurfaceOp};
use crate::voice::{Tts, TtsStream};
use bytes::Bytes;

/// How long the floor must stay quiet after the last finalized utterance before
/// the mind commits to replying. The human-interface tradeoff knob: higher =
/// more patient (never talks over a multi-burst thought) but more latency;
/// lower = snappier but more likely to answer a half-finished thought. Paired
/// with the client VAD's `endSilenceMs`, which governs how fast an utterance is
/// *finalized* (and POSTed); this governs how long we wait to see if another one
/// follows.
const RESPONSE_SETTLE: Duration = Duration::from_millis(700);

/// Built-in default soul, embedded at compile time from `default_soul.md`. The
/// agent's identity — how it speaks, what it values, how it renders surfaces — is
/// authored here as a tracked asset so a fresh install ships fully characterful.
/// On first run [`load_soul`] seeds `<data_dir>/SOUL.md` from this, after which
/// the admin can shape the agent's character by editing that file.
const DEFAULT_SOUL: &str = include_str!("default_soul.md");

/// Load the agent's soul — its identity (voice, values, guardrails, surface
/// house-style) — used as the system prompt for every scene's persistent reactor
/// session. On first run this seeds `<data_dir>/SOUL.md` from the built-in
/// [`DEFAULT_SOUL`], so a fresh install is immediately characterful and leaves an
/// editable file on disk; thereafter the admin can shape the agent's character by
/// editing SOUL.md without a rebuild. Read once at startup, so edits take effect
/// on the next restart. If seeding or reading fails for any reason, the in-memory
/// [`DEFAULT_SOUL`] still applies, so the agent always has a voice.
pub fn load_soul(data_dir: &Path) -> String {
    let path = data_dir.join("SOUL.md");

    // Seed the file from the built-in default on first run, so the admin has
    // something concrete to edit and the on-disk soul matches what's running.
    if !path.exists() {
        match std::fs::create_dir_all(data_dir).and_then(|()| std::fs::write(&path, DEFAULT_SOUL)) {
            Ok(()) => {
                tracing::info!(path = %path.display(), "seeded SOUL.md from built-in default soul");
            }
            Err(err) => {
                tracing::warn!(path = %path.display(), %err, "could not seed SOUL.md; using built-in default soul");
                return DEFAULT_SOUL.to_string();
            }
        }
    }

    match std::fs::read_to_string(&path) {
        Ok(text) if !text.trim().is_empty() => {
            tracing::info!(path = %path.display(), "loaded soul from SOUL.md");
            text
        }
        Ok(_) => {
            tracing::warn!(path = %path.display(), "SOUL.md is empty; using built-in default soul");
            DEFAULT_SOUL.to_string()
        }
        Err(err) => {
            tracing::warn!(path = %path.display(), %err, "could not read SOUL.md; using built-in default soul");
            DEFAULT_SOUL.to_string()
        }
    }
}

const SCENE_QUEUE_CAPACITY: usize = 64;

/// One item in a scene's turn queue. Both a human utterance and a worker's
/// report drive a reactor turn, but they enter differently: a human signal
/// comes through [`Reactor::deliver_to_scene`] and triggers barge-in (it cancels
/// the in-flight prompt); a worker report is posted straight into the queue by
/// the worker's drive task, so it waits its turn and never interrupts live
/// speech. Both land here and are settled into one batch.
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
fn schedule_alarm(alarms: &mut Alarms, scene: &Scene, block: &str) {
    let (delay_tok, note) = match block.split_once(char::is_whitespace) {
        Some((d, rest)) => (d, rest.trim()),
        None => (block, ""),
    };
    match parse_delay(delay_tok) {
        Some(delay) => {
            alarms.schedule(delay, note.to_owned(), Instant::now());
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
    /// Speech synthesis. `None` → the agent's replies are text-only (Phase 1
    /// behavior); when set, each sentence is synthesized and emitted as audio
    /// signals.
    tts: Option<Arc<dyn Tts>>,
    /// The reactor's single outbound seam: every channel signal it produces —
    /// text, synthesized speech, surfaces — goes out here in transport-free form
    /// (see [`outbound`]). A transport adapter binds these to a wire. The reactor
    /// has no knowledge of HTTP, `Content-Type`, or response framing.
    out: mpsc::Sender<OutboundSignal>,
    /// Monotonic cognition-turn counter. Each turn claims the next id;
    /// it tags audio spans and the channel logs so a reply is traceable end to
    /// end. (The client no longer needs it — turns are internal to the mind.)
    turn_seq: AtomicU64,
    scenes: Mutex<HashMap<Scene, SceneHandle>>,
}

struct SceneHandle {
    inbound: mpsc::Sender<LoopInput>,
    /// `None` when idle. Set to the in-flight session so the dispatcher can
    /// cancel it when a new signal arrives for this scene.
    in_flight: Arc<Mutex<Option<Arc<AcpSession>>>>,
}

pub fn start(
    memory: Memory,
    agent: AgentLayer,
    soul: String,
    mut inbound_rx: mpsc::Receiver<Signal>,
    out: mpsc::Sender<OutboundSignal>,
    tts: Option<Arc<dyn Tts>>,
) -> Reactor {
    let reactor = Reactor {
        inner: Arc::new(ReactorInner {
            memory,
            agent,
            soul,
            tts,
            out,
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
        let (sender, in_flight) = self.get_or_create_scene(scene.clone()).await;

        let in_flight_session: Option<Arc<AcpSession>> = {
            let guard = in_flight.lock().await;
            guard.as_ref().cloned()
        };
        if let Some(session) = in_flight_session {
            if let Err(err) = session.cancel().await {
                tracing::warn!(scene = %scene, error = %err, "session/cancel failed during interruption");
            } else {
                tracing::debug!(scene = %scene, "interrupting in-flight turn");
            }
        }

        if let Err(err) = sender.send(LoopInput::Human(signal)).await {
            tracing::error!(scene = %scene, error = %err, "scene inbound channel closed; dropping signal");
        }
    }

    async fn get_or_create_scene(
        &self,
        scene: Scene,
    ) -> (mpsc::Sender<LoopInput>, Arc<Mutex<Option<Arc<AcpSession>>>>) {
        let mut scenes = self.inner.scenes.lock().await;
        if let Some(handle) = scenes.get(&scene) {
            return (handle.inbound.clone(), handle.in_flight.clone());
        }

        let (tx, rx) = mpsc::channel::<LoopInput>(SCENE_QUEUE_CAPACITY);
        let in_flight: Arc<Mutex<Option<Arc<AcpSession>>>> = Arc::new(Mutex::new(None));
        scenes.insert(
            scene.clone(),
            SceneHandle {
                inbound: tx.clone(),
                in_flight: in_flight.clone(),
            },
        );
        drop(scenes);

        let task_reactor = self.clone();
        let task_scene = scene.clone();
        let task_in_flight = in_flight.clone();
        // The worker registry posts its reports back into this same queue, so
        // hand the loop a sender clone to seed it.
        let task_worker_inbound = tx.clone();
        tokio::spawn(async move {
            per_scene_loop(task_reactor, task_scene, rx, task_in_flight, task_worker_inbound).await;
        });

        (tx, in_flight)
    }
}

async fn per_scene_loop(
    reactor: Reactor,
    scene: Scene,
    mut inbound: mpsc::Receiver<LoopInput>,
    in_flight: Arc<Mutex<Option<Arc<AcpSession>>>>,
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

        match run_turn(&reactor, &scene, &batch, &in_flight, &mut reactor_session, &mut budget, &mut workers, &mut alarms).await {
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
                {
                    let mut guard = in_flight.lock().await;
                    *guard = None;
                }
                // Discard the possibly-wedged session; the next turn cold-opens a
                // fresh one and rebuilds context from the journal snapshot.
                reactor_session = None;
                budget.reset();
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
    in_flight: &Arc<Mutex<Option<Arc<AcpSession>>>>,
    reactor_session: &mut Option<Arc<AcpSession>>,
    budget: &mut heartbeat::ContextBudget,
    workers: &mut workers::WorkerRegistry,
    alarms: &mut Alarms,
) -> anyhow::Result<()> {
    let turn_id = reactor.inner.turn_seq.fetch_add(1, Ordering::Relaxed);

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
            *reactor_session = Some(opened.clone());
            opened
        }
    };

    {
        let mut guard = in_flight.lock().await;
        *guard = Some(session.clone());
    }

    let tts = reactor.inner.tts.clone();

    let outcome: anyhow::Result<usize> = async {
        let mut run = session.prompt(prompt_text).await?;

        // Per-turn streaming TTS: open ONE synthesis session for the whole turn
        // and push text into it as the agent produces it. Audio frames stream
        // back on a drain task as a single Start/Frame*/End run on /audio, so a
        // turn's speech is one continuous stream rather than per-sentence clips.
        // The sentence splitter survives only as a coalescer — it decides *when*
        // to push text (for prosody/request size), not playback boundaries; the
        // session stays open across sentences. All of this exists only when TTS
        // is configured.
        let mut splitter = SentenceSplitter::new();
        let mut extractor = SurfaceExtractor::new();
        // Pulls `[[delegate]] … [[/delegate]]` task blocks out of the reply: the
        // reactor delegates heavy work by naming a task inline, which spawns a
        // channel-mute working session and is NOT spoken to the scene.
        let mut delegate_extractor = MarkerExtractor::new(OPEN_DELEGATE, CLOSE_DELEGATE);
        // Pulls `[[alarm]] <delay> <note> [[/alarm]]` blocks out of the reply: the
        // mind scheduling its own future wake. Like delegate markers, never spoken.
        let mut alarm_extractor = MarkerExtractor::new(OPEN_ALARM, CLOSE_ALARM);
        // Accumulate the spoken text so the whole reply is logged once at end of
        // turn on the `channel` stream, rather than per-chunk (which is noisy).
        let mut full_reply = String::new();
        let (synth_tx, synth_handle) = match &tts {
            Some(tts) => match tts.start().await {
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
            },
            None => (None, None),
        };

        loop {
            match run.next_update().await {
                Some(SessionUpdate::Text(text)) => {
                    // Split rich-content surface blocks out of the spoken text.
                    let (clean, surfaces) = extractor.push(&text);
                    for envelope in surfaces {
                        emit_surface(reactor, scene, envelope).await;
                    }
                    // Then pull any `[[delegate]]` task blocks out of what's
                    // left and spawn a worker per task — these are never spoken.
                    let (spoken, tasks) = delegate_extractor.push(&clean);
                    for task in tasks {
                        if let Err(err) = workers.spawn(reactor, task).await {
                            tracing::warn!(scene = %scene, error = %err, "failed to spawn working session");
                        }
                    }
                    // Then pull any `[[alarm]]` blocks — self-scheduled wakes,
                    // also never spoken.
                    let (spoken, alarm_blocks) = alarm_extractor.push(&spoken);
                    for block in alarm_blocks {
                        schedule_alarm(alarms, scene, &block);
                    }
                    if !spoken.is_empty() {
                        full_reply.push_str(&spoken);
                        if let Some(tx) = &synth_tx {
                            for sentence in splitter.push(&spoken) {
                                let _ = tx.send(sentence).await;
                            }
                        }
                        emit_thought_chunk(reactor, scene, spoken).await;
                    }
                }
                Some(SessionUpdate::Thought(_)) => {
                    // Internal reasoning; do not surface.
                }
                Some(SessionUpdate::ToolCall(stub)) => {
                    tracing::debug!(scene = %scene, variant = stub.raw_variant, "tool call");
                }
                Some(SessionUpdate::Other(name)) => {
                    tracing::trace!(scene = %scene, variant = %name, "ignored ACP update");
                }
                None => break,
            }
        }

        // Drain any text the surface extractor was still holding, then the
        // delegate extractor, then flush the trailing partial sentence to TTS.
        let clean_tail = extractor.flush();
        let (mut spoken_tail, tail_tasks) = delegate_extractor.push(&clean_tail);
        spoken_tail.push_str(&delegate_extractor.flush());
        for task in tail_tasks {
            if let Err(err) = workers.spawn(reactor, task).await {
                tracing::warn!(scene = %scene, error = %err, "failed to spawn working session");
            }
        }
        // Drain the alarm extractor too, so an `[[alarm]]` that ended the reply
        // still schedules.
        let (mut spoken_tail, tail_alarms) = alarm_extractor.push(&spoken_tail);
        spoken_tail.push_str(&alarm_extractor.flush());
        for block in tail_alarms {
            schedule_alarm(alarms, scene, &block);
        }
        if !spoken_tail.is_empty() {
            full_reply.push_str(&spoken_tail);
            if let Some(tx) = &synth_tx {
                for sentence in splitter.push(&spoken_tail) {
                    let _ = tx.send(sentence).await;
                }
            }
            emit_thought_chunk(reactor, scene, spoken_tail).await;
        }
        if !full_reply.trim().is_empty() {
            crate::channel_log::outbound(Channel::Thought, scene, full_reply.trim());
        }
        if let Some(tx) = &synth_tx {
            if let Some(tail) = splitter.flush() {
                let _ = tx.send(tail).await;
            }
        }

        let mut cancelled = false;
        match run.wait().await {
            Ok(result) => {
                tracing::debug!(scene = %scene, stop = ?result.stop_reason, "turn finished");
            }
            Err(err) => {
                cancelled = true;
                tracing::debug!(scene = %scene, error = %err, "turn run ended with error (likely cancel)");
            }
        }

        // Dropping the text sender signals end-of-input: the TTS session sends
        // FinishSession, the drain task forwards trailing frames, then emits the
        // turn's `End`. On a cancel (barge-in) abort the drain so stale frames
        // aren't spoken over the user, and emit `End` ourselves so any open
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

    {
        let mut guard = in_flight.lock().await;
        *guard = None;
    }
    // The session is persistent — do NOT drop it. `in_flight` is cleared above
    // so a barge-in between turns finds no prompt to cancel; the caller's
    // `reactor_session` slot keeps the warm session alive for the next turn.

    // Fold this turn's size into the budget so the loop can decide whether the
    // session has grown enough to hot-swap. Only on success — a failed turn is
    // discarded along with its (possibly wedged) session.
    let reply_chars = outcome?;
    budget.record_turn(prompt_chars, reply_chars);
    Ok(())
}

async fn emit_thought_chunk(reactor: &Reactor, scene: &Scene, text: String) {
    let ts = Utc::now();
    let entry = JournalEntry::SignalOut {
        ts,
        channel: Channel::Thought,
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

/// Minimal incremental sentence splitter for per-sentence TTS, mirroring the
/// frontend `sentences.ts`: CJK terminators (。！？) split immediately; Latin
/// terminators (.!?…) split only when followed by whitespace, so decimals and
/// abbreviations aren't broken. The trailing partial waits for `flush()`.
struct SentenceSplitter {
    buf: String,
}

impl SentenceSplitter {
    fn new() -> Self {
        Self { buf: String::new() }
    }

    fn push(&mut self, chunk: &str) -> Vec<String> {
        self.buf.push_str(chunk);
        let mut out = Vec::new();
        while let Some(idx) = find_boundary(&self.buf) {
            let sentence = self.buf[..idx].trim().to_string();
            self.buf = self.buf[idx..].trim_start().to_string();
            if !sentence.is_empty() {
                out.push(sentence);
            }
        }
        out
    }

    fn flush(&mut self) -> Option<String> {
        let s = self.buf.trim().to_string();
        self.buf.clear();
        if s.is_empty() { None } else { Some(s) }
    }
}

/// Byte index just past the first sentence terminator that qualifies as a
/// boundary, or `None` if the buffer holds no complete sentence yet.
fn find_boundary(s: &str) -> Option<usize> {
    let mut chars = s.char_indices().peekable();
    while let Some((off, c)) = chars.next() {
        let end = off + c.len_utf8();
        if matches!(c, '。' | '！' | '？') {
            return Some(end);
        }
        if matches!(c, '.' | '!' | '?' | '…') {
            if let Some(&(_, next)) = chars.peek() {
                if next.is_whitespace() {
                    return Some(end);
                }
            }
        }
    }
    None
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

const OPEN_CARD: &str = "[[surface:card]]";
const OPEN_FULL: &str = "[[surface:full]]";
const CLOSE: &str = "[[/surface]]";

/// Streaming extractor that pulls `[[surface:…]] … [[/surface]]` HTML blocks out
/// of the agent's text. Text outside the markers passes through (spoken +
/// displayed); the inner HTML becomes a `SurfaceEnvelope`. A short tail that
/// could be a partial opener is held back so a marker split across chunks is
/// still recognized. Mirrors the convention taught in the soul (see [`DEFAULT_SOUL`]).
struct SurfaceExtractor {
    buf: String,
    inside: Option<SurfaceMode>,
}

impl SurfaceExtractor {
    fn new() -> Self {
        Self { buf: String::new(), inside: None }
    }

    fn push(&mut self, chunk: &str) -> (String, Vec<SurfaceEnvelope>) {
        self.buf.push_str(chunk);
        let mut text_out = String::new();
        let mut envelopes = Vec::new();

        loop {
            match self.inside {
                None => {
                    let card = self.buf.find(OPEN_CARD);
                    let full = self.buf.find(OPEN_FULL);
                    let opener = match (card, full) {
                        (Some(c), Some(f)) if c <= f => Some((c, SurfaceMode::Card, OPEN_CARD.len())),
                        (Some(_), Some(f)) => Some((f, SurfaceMode::Full, OPEN_FULL.len())),
                        (Some(c), None) => Some((c, SurfaceMode::Card, OPEN_CARD.len())),
                        (None, Some(f)) => Some((f, SurfaceMode::Full, OPEN_FULL.len())),
                        (None, None) => None,
                    };
                    if let Some((idx, mode, tok_len)) = opener {
                        text_out.push_str(&self.buf[..idx]);
                        self.buf = self.buf[idx + tok_len..].to_string();
                        self.inside = Some(mode);
                        continue;
                    }
                    // No opener: emit everything except a tail that could be the
                    // start of one continuing in the next chunk.
                    let keep = partial_open_suffix_len(&self.buf);
                    let emit_to = self.buf.len() - keep;
                    text_out.push_str(&self.buf[..emit_to]);
                    self.buf = self.buf[emit_to..].to_string();
                    break;
                }
                Some(mode) => {
                    if let Some(idx) = self.buf.find(CLOSE) {
                        let html = self.buf[..idx].trim().to_string();
                        self.buf = self.buf[idx + CLOSE.len()..].to_string();
                        self.inside = None;
                        envelopes.push(SurfaceEnvelope {
                            id: Uuid::now_v7().to_string(),
                            op: SurfaceOp::Show,
                            mode: Some(mode),
                            html: Some(html),
                            ttl_ms: None,
                        });
                        continue;
                    }
                    break; // close not present yet; keep buffering the HTML
                }
            }
        }
        (text_out, envelopes)
    }

    /// Emit any held-back text at end of turn. An unterminated block is dropped.
    fn flush(&mut self) -> String {
        let out = if self.inside.is_none() {
            std::mem::take(&mut self.buf)
        } else {
            String::new()
        };
        self.buf.clear();
        self.inside = None;
        out
    }
}

/// Length (bytes) of the longest suffix of `buf` that is a proper prefix of a
/// surface opener — i.e. a marker possibly split across chunks.
fn partial_open_suffix_len(buf: &str) -> usize {
    let max = OPEN_CARD.len().max(OPEN_FULL.len()) - 1;
    let start = buf.len().saturating_sub(max);
    for i in start..buf.len() {
        if !buf.is_char_boundary(i) {
            continue;
        }
        let suffix = &buf[i..];
        if OPEN_CARD.starts_with(suffix) || OPEN_FULL.starts_with(suffix) {
            return buf.len() - i;
        }
    }
    0
}

const OPEN_DELEGATE: &str = "[[delegate]]";
const CLOSE_DELEGATE: &str = "[[/delegate]]";

/// Streaming extractor for a single `OPEN … CLOSE` marker pair. Text outside the
/// markers passes through; each enclosed block's inner content is collected and
/// returned. A short tail that could be a partial opener is held back so a marker
/// split across chunks is still recognized.
///
/// The generic sibling of [`SurfaceExtractor`] (which carries a card/full mode
/// and yields envelopes). Used for `[[delegate]]` on the reactor side — the
/// mind names background work inline — and for `[[ask]]` on the worker side.
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
mod surface_tests {
    use super::SurfaceExtractor;
    use crate::types::SurfaceMode;

    #[test]
    fn passes_plain_text_through() {
        let mut e = SurfaceExtractor::new();
        let (t, s) = e.push("just talking, nothing to show");
        assert!(s.is_empty());
        assert_eq!(format!("{t}{}", e.flush()), "just talking, nothing to show");
    }

    #[test]
    fn extracts_a_card_across_chunks() {
        let mut e = SurfaceExtractor::new();
        let (t1, s1) = e.push("Here you go. [[surface:card]]<b>hi</b>");
        assert!(s1.is_empty());
        assert_eq!(t1, "Here you go. ");
        let (t2, s2) = e.push("[[/surface]] Done.");
        assert_eq!(s2.len(), 1);
        assert_eq!(s2[0].mode, Some(SurfaceMode::Card));
        assert_eq!(s2[0].html.as_deref(), Some("<b>hi</b>"));
        assert_eq!(format!("{t2}{}", e.flush()), " Done.");
    }

    #[test]
    fn recognizes_marker_split_across_chunks() {
        let mut e = SurfaceExtractor::new();
        let (t1, s1) = e.push("look [[surf");
        assert!(s1.is_empty());
        assert_eq!(t1, "look ");
        let (t2, s2) = e.push("ace:full]]<p>x</p>[[/surface]]");
        assert_eq!(s2.len(), 1);
        assert_eq!(s2[0].mode, Some(SurfaceMode::Full));
        assert_eq!(s2[0].html.as_deref(), Some("<p>x</p>"));
        assert_eq!(t2, "");
    }
}

#[cfg(test)]
mod tests {
    use super::SentenceSplitter;

    #[test]
    fn splits_latin_on_terminator_plus_space() {
        let mut s = SentenceSplitter::new();
        assert_eq!(s.push("Hello world"), Vec::<String>::new());
        assert_eq!(s.push(". How are"), vec!["Hello world.".to_string()]);
        assert_eq!(s.flush(), Some("How are".to_string()));
    }

    #[test]
    fn does_not_split_decimals() {
        let mut s = SentenceSplitter::new();
        assert_eq!(s.push("pi is 3.14 today"), Vec::<String>::new());
    }

    #[test]
    fn splits_cjk_immediately() {
        let mut s = SentenceSplitter::new();
        assert_eq!(
            s.push("你好。最近怎么样？"),
            vec!["你好。".to_string(), "最近怎么样？".to_string()]
        );
    }
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
