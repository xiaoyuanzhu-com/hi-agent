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
//! long-running, the mind calls the `delegate` tool with the task; the reactor
//! spawns a channel-mute [`workers`] session for it and keeps talking. The worker
//! runs with the same substrate (memory, tools) but no voice of its own, and
//! posts its result — or a question, if it gets stuck — back into this scene's
//! queue, where it lands as just another input the next turn folds into what the
//! mind says.

use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::Duration;

mod heartbeat;
mod interleave;
pub mod outbound;
mod sequencer;
mod tools;
mod workers;

pub use outbound::OutboundSignal;
pub use tools::{SceneControl, ToolRegistry, ToolSink};

/// The heartbeat's soft context-budget ceiling, surfaced so the observatory can
/// render each scene's budget as a fraction of where a hot-swap kicks in.
pub fn swap_budget_chars() -> usize {
    heartbeat::SWAP_AFTER_CHARS
}

use chrono::Utc;
use tokio::sync::{Mutex, mpsc, oneshot};
use tokio::time::{Instant, sleep_until, timeout};

use crate::acp::{AcpSession, SessionOpts, SessionUpdate};
use crate::agent::{AgentLayer, SessionRole};
use crate::memory::{Memory, build_for_scene};
use crate::observatory::{EventKind, Observatory, SessionKind};
use crate::types::{Channel, JournalEntry, Origin, Scene, Signal, ViewEnvelope, ViewOp};
use bytes::Bytes;
use uuid::Uuid;

/// How long the floor must stay quiet after the last finalized utterance before
/// the mind commits to replying. The human-interface tradeoff knob: higher =
/// more patient (never talks over a multi-burst thought) but more latency;
/// lower = snappier but more likely to answer a half-finished thought. Paired
/// with the client VAD's `endSilenceMs`, which governs how fast an utterance is
/// *finalized* (and POSTed); this governs how long we wait to see if another one
/// follows.
const RESPONSE_SETTLE: Duration = Duration::from_millis(700);

/// Built-in base prompts, embedded at compile time. `core.md` is the *mind's*
/// system prompt — who it is, how it talks, how it presents (by delegating a build
/// and showing the result by ref) and delegates. `appearance.md` is the *view
/// builder's* craft guide — read off disk by a build sub-agent, never loaded into
/// the mind's context. Both ship in the binary and refresh on every build;
/// [`install_prompts`] materialises them under `<data_dir>/prompts/`.
const CORE_BASE: &str = include_str!("core.md");
const APPEARANCE_BASE: &str = include_str!("appearance.md");

/// Separator that introduces the operator's override layer. Placed after the
/// bundled base so its instructions take precedence — the model honors the
/// later, more specific guidance where the two conflict.
const OVERRIDE_HEADER: &str = "\n\n# Operator overrides\n\nThe operator added the guidance below. It layers on top of everything above; where the two conflict, follow this.\n\n";

/// Compose a bundled base prompt with an optional operator override layer. The
/// base is the embedded current text; `<prompts_dir>/<local_name>` (e.g.
/// `core.local.md`) holds only the operator's deltas, appended under
/// [`OVERRIDE_HEADER`] so later, more-specific guidance wins. Missing or empty
/// override ⇒ the base verbatim, so it can neither go stale nor shadow updates.
fn compose_prompt(base: &str, prompts_dir: &Path, local_name: &str) -> String {
    let path = prompts_dir.join(local_name);
    match std::fs::read_to_string(&path) {
        Ok(text) if !text.trim().is_empty() => format!("{base}{OVERRIDE_HEADER}{}", text.trim()),
        _ => base.to_string(),
    }
}

/// Install the bundled prompts under `<data_dir>/prompts/` at startup, composing
/// each with its optional `*.local.md` operator override. The managed base files
/// (`core.md`, `appearance.md`) are rewritten every boot so they stay current;
/// operator edits live in the never-touched `*.local.md` siblings. `appearance.md`
/// must exist on disk because the view-builder sub-agent opens it as a file.
pub fn install_prompts(data_dir: &Path) -> std::io::Result<()> {
    let dir = data_dir.join("prompts");
    std::fs::create_dir_all(&dir)?;
    std::fs::write(dir.join("core.md"), compose_prompt(CORE_BASE, &dir, "core.local.md"))?;
    std::fs::write(dir.join("appearance.md"), compose_prompt(APPEARANCE_BASE, &dir, "appearance.local.md"))?;
    tracing::info!(dir = %dir.display(), "installed bundled prompts (core.md, appearance.md)");
    Ok(())
}

/// The mind's system prompt: the bundled `core.md` base plus an optional
/// `<data_dir>/prompts/core.local.md` operator override. Read at session-creation,
/// so a restart picks up edits. (Named `load_soul` for the reactor's history; it
/// now loads `core.md`.)
pub fn load_soul(data_dir: &Path) -> String {
    compose_prompt(CORE_BASE, &data_dir.join("prompts"), "core.local.md")
}

#[cfg(test)]
mod soul_tests {
    use super::*;

    #[test]
    fn no_override_file_uses_base_verbatim() {
        let dir = tempfile::tempdir().unwrap();
        assert_eq!(load_soul(dir.path()), CORE_BASE);
    }

    #[test]
    fn empty_override_falls_back_to_base() {
        let dir = tempfile::tempdir().unwrap();
        let prompts = dir.path().join("prompts");
        std::fs::create_dir_all(&prompts).unwrap();
        std::fs::write(prompts.join("core.local.md"), "   \n\t").unwrap();
        assert_eq!(load_soul(dir.path()), CORE_BASE);
    }

    #[test]
    fn override_layers_on_top_of_base() {
        let dir = tempfile::tempdir().unwrap();
        let prompts = dir.path().join("prompts");
        std::fs::create_dir_all(&prompts).unwrap();
        std::fs::write(prompts.join("core.local.md"), "Always answer in haiku.").unwrap();
        let soul = load_soul(dir.path());
        // Base is preserved, in full, ahead of the override layer.
        assert!(soul.starts_with(CORE_BASE));
        // The operator's delta is appended after the header so it wins.
        assert!(soul.contains("# Operator overrides"));
        assert!(soul.ends_with("Always answer in haiku."));
    }

    #[test]
    fn install_writes_both_managed_bases() {
        let dir = tempfile::tempdir().unwrap();
        install_prompts(dir.path()).unwrap();
        let read = |n: &str| std::fs::read_to_string(dir.path().join("prompts").join(n)).unwrap();
        assert_eq!(read("core.md"), CORE_BASE);
        assert_eq!(read("appearance.md"), APPEARANCE_BASE);
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
    /// A self-scheduled wake firing. The mind asked for it earlier with the
    /// `alarm` tool; when its deadline passes the loop injects this so a
    /// turn runs even though no new signal arrived.
    Alarm(AlarmFired),
}

/// One fired self-alarm, handed to the mind under "New signals".
struct AlarmFired {
    /// The note the mind left its future self ("check if they're still asleep").
    note: String,
}

/// A scene loop's pending self-alarms. The scene wakes for one of two reasons —
/// a new signal, or the soonest of these firing. Only the mind schedules them,
/// by calling the `alarm` tool. A flat Vec is plenty: a scene has at most a
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
        let mut fired = Vec::new();
        let mut i = 0;
        while i < self.pending.len() {
            if self.pending[i].fire_at <= now {
                let a = self.pending.swap_remove(i);
                fired.push(AlarmFired { note: a.note });
            } else {
                i += 1;
            }
        }
        fired
    }
}

/// Parse an alarm delay token: a bare integer is seconds, or an integer
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

/// Register a self-alarm from the `alarm` tool's `delay`/`note` arguments. A
/// delay that won't parse is logged and dropped (fix-forward — the mind isn't
/// blocked on it).
async fn schedule_alarm(reactor: &Reactor, alarms: &mut Alarms, scene: &Scene, delay: &str, note: &str) {
    match parse_delay(delay) {
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
            tracing::warn!(scene = %scene, token = %delay, "ignoring alarm with unparseable delay");
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
    /// Compiles agent-authored `[[view]]` source into an ESM module the browser
    /// imports. Invoked just-in-time when a view segment is released, so the
    /// compiled module URL is what rides the /view channel.
    view_compiler: crate::views::ViewCompiler,
    /// Scene→tool-sink table the `/mcp` server routes tool calls through. Each
    /// scene loop registers its sink here as it stands up; shared (cloneable)
    /// with the HTTP front. See [`tools`].
    tools: ToolRegistry,
    /// Absolute path to the agent's global working directory (`<data_dir>/workspace`).
    /// Handed to every worker session as its `cwd`, so a build sub-agent works in a
    /// real project dir — `ls`-ing existing projects, writing source — like a human
    /// in their repo. Absolutized at startup (the child may run with a different cwd).
    workspace_dir: PathBuf,
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
    mut warm_rx: mpsc::Receiver<Scene>,
    out: mpsc::Sender<OutboundSignal>,
    observatory: Observatory,
    view_compiler: crate::views::ViewCompiler,
    tools: ToolRegistry,
    workspace_dir: PathBuf,
) -> Reactor {
    let reactor = Reactor {
        inner: Arc::new(ReactorInner {
            memory,
            agent,
            soul,
            out,
            observatory,
            view_compiler,
            tools,
            workspace_dir,
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

    // Warm-up requests: a scene-presence GET (a client opening a `/api/out/*`
    // long-poll) asks us to stand the scene up now, so its subprocess and ACP
    // session are open before the first utterance lands. `ensure_scene` is
    // idempotent — repeated GETs for an already-live scene are no-ops.
    let warm_reactor = reactor.clone();
    tokio::spawn(async move {
        while let Some(scene) = warm_rx.recv().await {
            warm_reactor.ensure_scene(scene).await;
        }
        tracing::warn!("reactor warm channel closed; warm-up loop exiting");
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

    /// Stand a scene's loop up now (idempotent), so its warm-up prologue runs and
    /// the scene is hot before the first utterance. Driven by a scene-presence
    /// signal — a client opening one of the scene's `/api/out/*` long-polls; an
    /// already-live scene is a no-op.
    pub async fn ensure_scene(&self, scene: Scene) {
        let _ = self.get_or_create_scene(scene).await;
    }

    async fn get_or_create_scene(&self, scene: Scene) -> mpsc::Sender<LoopInput> {
        let mut scenes = self.inner.scenes.lock().await;
        if let Some(handle) = scenes.get(&scene) {
            return handle.inbound.clone();
        }

        let (tx, rx) = mpsc::channel::<LoopInput>(SCENE_QUEUE_CAPACITY);
        scenes.insert(scene.clone(), SceneHandle { inbound: tx.clone() });
        drop(scenes);

        // The scene's tool control channel: the `/mcp` server forwards delegate/
        // alarm/ask calls here, the loop applies them. Register the sink before the
        // loop's session opens so a tool call can never arrive with no route.
        let (control_tx, control_rx) = mpsc::channel::<SceneControl>(SCENE_QUEUE_CAPACITY);

        // The scene's output beats: say/show_view tool calls (and the loop's turn
        // brackets) flow to a dedicated sequencer task that paces speech and views.
        // Output bypasses the turn loop so it streams while the prompt still runs.
        let (beats_tx, beats_rx) = mpsc::channel::<sequencer::Beat>(SCENE_QUEUE_CAPACITY);
        {
            let seq_reactor = self.clone();
            let seq_scene = scene.clone();
            tokio::spawn(async move {
                sequencer::run_sequencer(seq_reactor, seq_scene, beats_rx).await;
            });
        }

        self.inner
            .tools
            .register(
                scene.clone(),
                ToolSink { control: control_tx.clone(), beats: beats_tx.clone() },
            )
            .await;

        let task_reactor = self.clone();
        let task_scene = scene.clone();
        // The worker registry posts its reports back into this same queue, so
        // hand the loop a sender clone to seed it.
        let task_worker_inbound = tx.clone();
        tokio::spawn(async move {
            per_scene_loop(
                task_reactor,
                task_scene,
                rx,
                task_worker_inbound,
                control_rx,
                control_tx,
                beats_tx,
            )
            .await;
        });

        tx
    }
}

/// Why the per-scene loop's wait resolved. Keeps the `select!` arms tiny so the
/// borrow checker doesn't trip on mutating `workers`/`alarms` inside them.
enum Woke {
    Inbound(Option<LoopInput>),
    Control(Option<SceneControl>),
    Timer,
}

/// Apply one tool control command. Delegate and alarm are side-effects that run
/// without a turn (returns `None`); a worker `ask` becomes a question report the
/// loop folds into its next turn (returns `Some`). Worker-registry and alarm
/// state are the loop's own, so this is the only place off-loop tool calls touch
/// them — through the control channel, no locking.
async fn apply_control(
    reactor: &Reactor,
    scene: &Scene,
    workers: &mut workers::WorkerRegistry,
    alarms: &mut Alarms,
    ctl: SceneControl,
) -> Option<LoopInput> {
    match ctl {
        SceneControl::Delegate { task } => {
            if let Err(err) = workers.spawn(reactor, task).await {
                tracing::warn!(scene = %scene, error = %err, "failed to spawn working session");
            }
            None
        }
        SceneControl::Alarm { delay, note } => {
            schedule_alarm(reactor, alarms, scene, &delay, &note).await;
            None
        }
        SceneControl::WorkerAsk { id, question } => {
            reactor
                .inner
                .observatory
                .record(scene, EventKind::WorkerQuestion { id, question: question.clone() })
                .await;
            Some(LoopInput::Worker(workers.question_report(id, question)))
        }
    }
}

async fn per_scene_loop(
    reactor: Reactor,
    scene: Scene,
    mut inbound: mpsc::Receiver<LoopInput>,
    worker_inbound: mpsc::Sender<LoopInput>,
    mut control: mpsc::Receiver<SceneControl>,
    // Held only to keep the control channel open: the registry holds the other
    // sender, but keeping a clone here means `control.recv()` never resolves to
    // `None` while this loop runs, so a quiet tool channel can't end the scene.
    _control_keepalive: mpsc::Sender<SceneControl>,
    // The scene's output sequencer inlet. The loop sends each turn's TurnStart/
    // TurnEnd brackets here; the `/mcp` handler sends the say/show_view beats
    // between them. The same sender is the keepalive for the sequencer task.
    beats: mpsc::Sender<sequencer::Beat>,
) {
    // The scene's persistent reactor session: opened lazily on the first turn,
    // then reused for every later turn as the scene's continuous mind. Only this
    // loop touches it, so a plain local `Option` suffices; the heartbeat swap
    // below replaces it in place, between turns.
    let mut reactor_session: Option<Arc<AcpSession>> = None;
    // Whether the live session has been seeded with the journal snapshot yet.
    // Warm-up opens the session without prompting, so it can be `Some` yet
    // unseeded; the first real turn sends the snapshot and flips this. A hot-swap
    // bakes the journal tail into the replacement's system prompt, so a swapped
    // session stays seeded; a session discarded after a turn failure resets this
    // so the next cold-open re-seeds.
    let mut seeded = false;
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

    // Warm-up: this loop was just stood up (a scene-presence GET, or the first
    // utterance). Pull the cold-start forward now — spawn the subprocess and open
    // the persistent ACP session — so that work is off the first real turn's
    // critical path. We deliberately do NOT prompt the model here: the soul and
    // journal snapshot are still delivered by the first real turn (which sees an
    // open-but-unseeded session). Best-effort; on failure the first turn cold-opens
    // as before.
    warm_up(&reactor, &scene, &mut reactor_session).await;

    loop {
        // Wait for a turn-driving reason: a new signal, a fired alarm, or a worker
        // question. Tool control commands (delegate/alarm) are pure side-effects —
        // applied as they arrive without starting a turn; only a worker `ask`
        // becomes a turn-driving item. When the mind has set alarms, the soonest
        // also wakes the loop.
        let mut batch: Vec<LoopInput> = Vec::new();
        'wait: loop {
            let woke = match alarms.next_deadline() {
                Some(deadline) => tokio::select! {
                    recvd = inbound.recv() => Woke::Inbound(recvd),
                    ctl = control.recv() => Woke::Control(ctl),
                    _ = sleep_until(deadline) => Woke::Timer,
                },
                None => tokio::select! {
                    recvd = inbound.recv() => Woke::Inbound(recvd),
                    ctl = control.recv() => Woke::Control(ctl),
                },
            };
            match woke {
                Woke::Inbound(Some(s)) => {
                    batch.push(s);
                    break 'wait;
                }
                Woke::Inbound(None) => {
                    tracing::info!(scene = %scene, "per-scene inbound closed; exiting loop");
                    return;
                }
                // The keepalive sender means this is effectively unreachable; treat
                // a closed control channel as "nothing to apply" and keep waiting.
                Woke::Control(None) => continue 'wait,
                Woke::Control(Some(ctl)) => {
                    if let Some(input) =
                        apply_control(&reactor, &scene, &mut workers, &mut alarms, ctl).await
                    {
                        batch.push(input);
                        break 'wait;
                    }
                    // A delegate/alarm side-effect was applied; keep waiting for a
                    // turn-driving reason rather than running an empty turn.
                }
                Woke::Timer => {
                    for fired in alarms.take_due(Instant::now()) {
                        reactor
                            .inner
                            .observatory
                            .record(&scene, EventKind::AlarmFired { note: fired.note.clone() })
                            .await;
                        batch.push(LoopInput::Alarm(fired));
                    }
                    if !batch.is_empty() {
                        break 'wait;
                    }
                }
            }
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

        match run_turn(&reactor, &scene, &batch, &mut reactor_session, &mut seeded, &mut budget, &mut workers, &beats).await {
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
                // The fresh session that replaces it must re-ingest the snapshot.
                seeded = false;
                budget.reset();
                reactor.inner.observatory.set_budget(&scene, 0).await;
            }
        }
    }
}

/// One-time scene warm-up, run once before the per-scene loop blocks on its first
/// input. Opens the scene's persistent reactor session so the subprocess spawn and
/// ACP `session/new` are off the first reply's critical path — that's the whole
/// job. It does NOT prompt the model: the soul (system prompt) and journal
/// snapshot are still delivered by the first real turn, which sees an
/// open-but-unseeded session. Warming the upstream prompt cache would need a
/// throwaway round-trip; that is deliberately not done here — unproven benefit,
/// real cost.
///
/// Best-effort: any failure is logged and leaves the session unopened, so the
/// first real turn just cold-opens as it did before.
async fn warm_up(reactor: &Reactor, scene: &Scene, reactor_session: &mut Option<Arc<AcpSession>>) {
    // Defensive: the prologue runs once on a fresh loop, so the session is always
    // cold here, but never re-open an already-open session.
    if reactor_session.is_some() {
        return;
    }
    match open_session(reactor, scene).await {
        Ok(session) => {
            *reactor_session = Some(session);
            tracing::info!(scene = %scene, "reactor session warmed up (opened, unseeded)");
        }
        Err(err) => {
            tracing::warn!(scene = %scene, error = %err, "scene warm-up failed; first turn will cold-start");
        }
    }
}

/// Compose a session's system prompt: the soul, plus the always-loaded memory
/// core (`self.md` + `hot.md`) when it exists. Loaded at session-creation, so a
/// hot-swap picks up whatever the last reflection wrote.
pub(super) fn system_prompt_with_core(soul: &str, core: &str) -> String {
    let core = core.trim();
    if core.is_empty() {
        soul.to_owned()
    } else {
        format!("{soul}\n\n{core}")
    }
}

/// Open a fresh persistent reactor session for `scene`, carrying the soul (plus
/// the memory core) as its system prompt, and record the lifecycle event. The
/// session consumes the system prompt on its first `prompt()` and never re-sends
/// it. Shared by the warm-up prologue and the cold path of [`run_turn`].
async fn open_session(reactor: &Reactor, scene: &Scene) -> anyhow::Result<Arc<AcpSession>> {
    let core = crate::memory::load_core(&reactor.inner.memory).await;
    let system_prompt = system_prompt_with_core(&reactor.inner.soul, &core);
    let session = Arc::new(
        reactor
            .inner
            .agent
            .session(
                scene,
                SessionRole::Reactor,
                None,
                SessionOpts {
                    system_prompt: Some(system_prompt),
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
                id: session.id().0.to_string(),
            },
        )
        .await;
    Ok(session)
}

/// One turn: prompt the scene's persistent reactor session (opening it on the
/// first turn) and bracket it on the scene's output sequencer. Spoken text and
/// views no longer ride the reply stream — the mind emits them as `say`/`show_view`
/// tool calls that land on the sequencer out of band — so here we only seed the
/// prompt, drive it to completion, and report the turn. The sequencer returns the
/// turn's spoken reply (for the context budget and the turn log).
///
/// An unseeded session — never prompted (freshly cold-opened, or warmed by the
/// prologue) — is seeded with the journal snapshot, since it carries no memory of
/// prior turns. A seeded session already ingested that history, so it gets only
/// the new signals; the snapshot is the durable backstop, not per-turn context to
/// re-send. `seeded` decouples "snapshot delivered" from "session open", since
/// warm-up opens a session without seeding it.
async fn run_turn(
    reactor: &Reactor,
    scene: &Scene,
    batch: &[LoopInput],
    reactor_session: &mut Option<Arc<AcpSession>>,
    seeded: &mut bool,
    budget: &mut heartbeat::ContextBudget,
    workers: &mut workers::WorkerRegistry,
    beats: &mpsc::Sender<sequencer::Beat>,
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

    // Seed the journal snapshot only when the session is unseeded; a seeded
    // session already lived the history and gets only what's new (plus the live
    // worker view). The snapshot is the durable backstop, not per-turn context to
    // re-send.
    // Bracket the turn on the sequencer (it renders say()/show_view() that arrive
    // out-of-band as tool calls between these two beats). Sent once, before the
    // retry loop, so the whole turn — every attempt — lives inside one bracket and
    // closes exactly once below, even if every attempt fails.
    let _ = beats.send(sequencer::Beat::TurnStart { turn: turn_id }).await;

    // Drive the prompt to completion, retrying a failed attempt on a freshly
    // restarted ACP session with exponential backoff. An LLM-side failure surfaces
    // as a `session/prompt` that resolves with an error (or never returns a
    // response); the wedged session is discarded and the next attempt cold-opens a
    // clean one and re-ingests the journal snapshot. The raw error frames are
    // already mirrored to the ACP tap (the /inspect window) at the wire, so they
    // need no extra plumbing here.
    const MAX_ATTEMPTS: u32 = 3;
    let mut attempt: u32 = 0;
    let mut prompt_chars = 0usize;
    let drive: anyhow::Result<Option<String>> = loop {
        attempt += 1;

        // Build the prompt and acquire the session *inside* the attempt, so a
        // failure to open (or to build the snapshot) is itself retriable and the
        // turn still closes its sequencer bracket below rather than bailing early.
        // An unseeded session — fresh after a discard — re-seeds with the snapshot.
        let attempt_result: anyhow::Result<Option<String>> = async {
            let prompt_text = if *seeded {
                join_sections(&[&worker_status, &new_signals])
            } else {
                let snap = build_for_scene(&reactor.inner.memory, scene).await?;
                join_sections(&[&snap.render_for_prompt(), &worker_status, &new_signals])
            };
            prompt_chars = prompt_text.chars().count();

            let session = match reactor_session {
                Some(s) => s.clone(),
                None => {
                    let opened = open_session(reactor, scene).await?;
                    *reactor_session = Some(opened.clone());
                    opened
                }
            };

            // Output rides the tool channel now, so this stream carries only
            // tool-call notifications and the stop; any plain text the model emits
            // instead of a say() call is dropped (and warned).
            let mut run = session.prompt(prompt_text).await?;
            let mut ended = false;
            while !ended {
                match run.next_update().await {
                    Some(SessionUpdate::ToolCall(stub)) => {
                        tracing::debug!(scene = %scene, variant = stub.raw_variant, "tool call");
                    }
                    Some(SessionUpdate::Text(text)) => {
                        if !text.trim().is_empty() {
                            tracing::warn!(scene = %scene, "reactor emitted plain text instead of a say() tool call; dropping it");
                        }
                    }
                    Some(_) => {} // thoughts and unmodelled updates
                    None => ended = true,
                }
            }
            let result = run.wait().await?;
            tracing::debug!(scene = %scene, stop = ?result.stop_reason, "turn finished");
            Ok(Some(format!("{:?}", result.stop_reason)))
        }
        .await;

        match attempt_result {
            Ok(stop_reason) => break Ok(stop_reason),
            Err(err) => {
                tracing::warn!(scene = %scene, attempt, error = %err, "prompt attempt failed");
                // Discard the possibly-wedged session so the next attempt restarts
                // the ACP session from cold and rebuilds context from the snapshot.
                if let Some(dead) = reactor_session.take() {
                    reactor
                        .inner
                        .observatory
                        .record(
                            scene,
                            EventKind::SessionClosed {
                                kind: SessionKind::Reactor,
                                id: dead.id().0.to_string(),
                            },
                        )
                        .await;
                }
                *seeded = false;
                budget.reset();
                reactor.inner.observatory.set_budget(scene, 0).await;

                if attempt >= MAX_ATTEMPTS {
                    break Err(err);
                }
                // Exponential backoff before the restart: 250ms, then 500ms.
                let backoff = Duration::from_millis(250u64 << (attempt - 1));
                tracing::info!(scene = %scene, attempt, ?backoff, "restarting ACP session after backoff");
                tokio::time::sleep(backoff).await;
            }
        }
    };

    // On terminal failure (all attempts exhausted), tell the human something went
    // wrong before closing the turn — otherwise the reply is an unexplained silence.
    // Routed through the normal say() seam so it reaches the /text channel (and the
    // long-poll waiting on it) and closes cleanly on TurnEnd.
    if let Err(err) = &drive {
        let _ = beats
            .send(sequencer::Beat::Say(format!(
                "抱歉，我这边出了点问题，没能完成这次回应。({err})"
            )))
            .await;
    }

    // Always close the turn on the sequencer, even on error, so any open audio
    // span ends and the /thought utterance closes. It hands back this turn's
    // spoken reply, accumulated from the say() calls.
    let (done_tx, done_rx) = oneshot::channel();
    let _ = beats.send(sequencer::Beat::TurnEnd { done: done_tx }).await;
    let reply = done_rx.await.unwrap_or_default();

    // A turn that failed every attempt propagates, so the caller's error arm runs
    // (the session is already discarded above; it re-resets seeded/budget).
    let stop_reason = drive?;
    reactor
        .inner
        .observatory
        .record(
            scene,
            EventKind::TurnFinished {
                turn: turn_id,
                stop_reason,
                reply_chars: reply.chars().count(),
                reply: preview(&reply),
            },
        )
        .await;

    // The session is persistent — do NOT drop it. The caller's `reactor_session`
    // slot keeps the warm session alive for the next turn.

    // The session has now ingested the snapshot (this turn delivered it if it was
    // unseeded); later turns send only what's new.
    *seeded = true;
    budget.record_turn(prompt_chars, reply.chars().count());
    reactor.inner.observatory.set_budget(scene, budget.chars()).await;
    Ok(())
}

async fn emit_thought_chunk(reactor: &Reactor, scene: &Scene, text: String) {
    let ts = Utc::now();
    let entry = JournalEntry::SignalOut {
        id: Uuid::now_v7().to_string(),
        ts,
        channel: Channel::Text,
        scene: scene.clone(),
        body: text.clone(),
        media: None,
        origin: Some(Origin::Reactor),
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
        interleave::Emit::ShowView { id, op, source } => {
            emit_view(reactor, scene, id, op, source).await
        }
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
                use crate::memory::snapshot::{Speaker, transcript_line};
                let chan = sig.channel.with_stream(sig.stream.as_deref());
                let _ = writeln!(s, "{}", transcript_line(Speaker::Them, &chan, &sig.body));
            }
            LoopInput::Worker(report) => {
                let _ = writeln!(s, "{}", workers::render_report(report));
            }
            LoopInput::Alarm(a) => {
                let _ = writeln!(s, "(alarm) \"{}\"", a.note);
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

/// Emit one agent-authored view on the /view channel for this scene. A `show`/
/// `replace` compiles the source to a module first (just-in-time, after the
/// preceding sentence has flushed, so it stays paced to narration); a `dismiss`
/// carries no module. A compile failure is logged and the view is dropped — the
/// turn's speech already went out, so a broken view never breaks the reply.
async fn emit_view(reactor: &Reactor, scene: &Scene, id: String, op: ViewOp, source: String) {
    let module_url = if op == ViewOp::Dismiss {
        None
    } else {
        match reactor.inner.view_compiler.compile(&source).await {
            Ok(url) => Some(url),
            Err(err) => {
                tracing::error!(scene = %scene, id = %id, error = %err, "view compile failed; dropping view");
                return;
            }
        }
    };
    tracing::info!(
        target: "channel",
        dir = "out",
        channel = "view",
        scene = %scene,
        id = %id,
        op = ?op,
        module = module_url.as_deref().unwrap_or(""),
        "channel out (view)",
    );
    let _ = reactor
        .inner
        .out
        .send(OutboundSignal::View {
            scene: scene.clone(),
            envelope: ViewEnvelope { id, op, module_url, ttl_ms: None },
        })
        .await;
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
