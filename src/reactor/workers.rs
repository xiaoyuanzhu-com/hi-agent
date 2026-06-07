//! Working sessions — the reactor's hands.
//!
//! The reactor keeps a single voice and must never block the floor on slow
//! work, so heavy or long-running tasks are delegated here. A worker is a
//! *voice-mute capability within the scene*: it has the full substrate — the
//! scene's memory, tools, code execution, the right to spawn further workers —
//! but no voice of its own. It never speaks and never draws on the screen: it
//! cannot emit on the reactor's expression channels (thought, audio, view). That
//! mute-ness is what preserves single-voice coherence: only the reactor expresses
//! to the person.
//!
//! It is *not*, however, channel-blind. Over hi-agent's own HTTP surface
//! (`HI_AGENT_BASE_URL` in its env) a worker may **perceive input channels**
//! (e.g. `GET /api/in/vision` for live frames) — running detection, CV, whatever
//! the task needs on the raw bytes, all *outside* the turn loop so it never
//! contends with the reactor's serialized speech. It does not write to any output
//! channel: expression (speech and views alike) stays the reactor's, so a worker
//! reports what it found and the reactor decides what to show.
//!
//! The collaboration bus is asynchronous and worker→reactor here: a worker runs
//! to completion (or until it must ask something), then posts a [`WorkerReport`]
//! back into the scene's queue as a `LoopInput::Worker`. It never interrupts live
//! speech — the report waits its turn like any other input, and the next turn
//! folds it into what the mind says. Questions are *non-blocking*: a worker that
//! hits ambiguity flags it via the `ask` tool and then proceeds on its best
//! assumption (fix-forward), so the floor is never held waiting on an answer.
//!
//! Progress-checking is emergent rather than wired: each worker streams its
//! output into an inspectable transcript, and [`WorkerRegistry::render_status`]
//! surfaces a live tail of every running worker into the reactor's prompt, so
//! the mind can decide on its own social timing whether to wait, nudge, or
//! speak to what it sees.

use std::collections::HashMap;
use std::fmt::Write as _;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::Duration;

use tokio::sync::{Mutex, Notify, mpsc};
use tokio::task::JoinHandle;
use tokio::time::timeout;

use crate::acp::{AcpSession, SessionOpts, SessionUpdate};
use crate::agent::SessionRole;
use crate::observatory::{EventKind, Observatory, WorkerState};
use crate::types::Scene;

use super::{LoopInput, Reactor};

/// Per-scene-unique handle for a working session. Small and `Copy`; it tags the
/// worker in status lines and in the reports it posts back.
pub(super) type WorkerId = u64;

/// How long a finished working session stays warm — its ACP session held open and
/// resumable via `delegate worker:<id>` — before it closes itself to free the
/// subprocess context. A refinement arriving within this window continues the same
/// session with full context; a later one falls back to a fresh worker.
const WORKER_IDLE_TTL: Duration = Duration::from_secs(15 * 60);

/// A worker's follow-up mailbox: a single pending message the registry merges into
/// while the worker is busy, drained by the drive loop the moment it goes free. This
/// is the worker-side analog of the reactor's commit-after-quiet — every follow-up
/// that lands while the session is occupied is *concatenated* into this one message,
/// so the worker picks up all of it in a single next prompt rather than running each
/// as its own round-trip. No LLM-smart merge: the worker's own model parses the
/// combined text. The `notify` wakes the drive loop when it's idle-waiting; `closed`
/// (flipped under the same lock the drive loop takes) lets a racing follow-up tell
/// the worker has shut down and fall back to a fresh spawn.
#[derive(Default)]
struct MailboxState {
    pending: Option<String>,
    closed: bool,
}

struct FollowMailbox {
    state: std::sync::Mutex<MailboxState>,
    notify: Notify,
}

impl FollowMailbox {
    fn new() -> Arc<Self> {
        Arc::new(Self { state: std::sync::Mutex::new(MailboxState::default()), notify: Notify::new() })
    }
}

const WORKER_SYSTEM_PROMPT: &str = "You are a working session spun up by a \
human-interface agent to carry out one specific delegated task. You have full \
access to files, code execution, memory, and the rest of the harness's tools — \
use them freely to actually complete the work, not merely plan it.\n\
\n\
You have no voice of your own and nothing you produce reaches the person \
directly: you neither speak nor draw on their screen. The agent owns all \
expression — it does the talking and decides what to show. Your job is to DO the \
task and then report the result: finish with a clear, self-contained summary of \
what you did and what came of it. That summary is handed back to the agent \
verbatim, so include everything it needs to act on or to relay — don't assume it \
can see your working notes. If something should be shown to the person, say so in \
your report and let the agent present it.\n\
\n\
You MAY use hi-agent's own input channels to perceive. The server's base URL is \
in the `HI_AGENT_BASE_URL` environment variable, and your scene is `{scene}` — \
send it as the `X-HI-Scene` header on every such request. For example, the live \
camera:\n\
    `GET $HI_AGENT_BASE_URL/api/in/vision` with header `X-HI-Scene: {scene}`\n\
  (a live video stream — one camera session per response, `video/webm`; \
re-request for the next). Decode and sample frames however the task needs — \
detection, CV, etc. is your job. You do not write to any output channel; \
presenting is the agent's job.\n\
\n\
When your task is to build a view to show on screen, first read the house style and \
the bar from the file at `$HI_AGENT_PROMPTS_DIR/appearance.md`, then author the \
component to that standard. Your working directory is the agent's global workspace — \
`ls` it to see existing projects before you name a new one. Save the finished view as \
a `.jsx` file (a default-exported component) under a project folder, e.g. \
`badminton-top10/leader.jsx`; its ref is that path without the extension \
(`badminton-top10/leader`). For images, download them into the project folder with \
your own tools and reference them by their served path — a file you save at \
`badminton-top10/leader.jpg` is served at `/workspace/badminton-top10/leader.jpg`; \
never hotlink a remote URL. Report every ref you saved in your summary — that ref is \
how the agent puts your view on screen.\n\
\n\
If you hit something genuinely ambiguous, do not stall waiting for an answer. \
Make the most reasonable assumption, note it, and keep going — the agent can \
correct course later. If you must surface a question, call the `ask` tool with it \
and then proceed on your best assumption anyway; the agent sees the question and \
may steer you, but you never wait. Work to completion.\n\
\n\
You may be handed a follow-up task later in this same session, building on what you \
just did — your earlier work, files, and findings are all still here, so extend them \
rather than starting over or duplicating them.";

/// The worker's system prompt, with its scene interpolated so it can tag every
/// input-channel request with the right `X-HI-Scene`. The server base URL is
/// delivered out-of-band in the subprocess env
/// ([`crate::config::ENV_SERVER_BASE_URL`]), which the prompt references as
/// `$HI_AGENT_BASE_URL`. Output side-effects (the `ask` tool) ride the MCP attach,
/// which carries the scene/role/worker-id headers for the worker automatically.
fn worker_system_prompt(scene: &Scene) -> String {
    WORKER_SYSTEM_PROMPT.replace("{scene}", &scene.0)
}

/// A report a worker posts back to the reactor's per-scene loop. It enters the
/// queue as a `LoopInput::Worker`, so it waits its turn and never interrupts
/// live speech.
pub(super) struct WorkerReport {
    pub(super) id: WorkerId,
    pub(super) task: String,
    pub(super) kind: WorkerReportKind,
}

pub(super) enum WorkerReportKind {
    /// The task finished; the string is the worker's self-contained summary.
    Done(String),
    /// The task errored out (session open failed, prompt failed, etc.).
    Failed(String),
    /// A non-blocking question raised mid-flight; the worker keeps going.
    Question(String),
}

/// One live working session. The registry holds it to inspect its transcript, to
/// resume it with follow-up tasks, and to know when its drive task has finally
/// exited; the drive task owns the session itself and closes it once it goes idle
/// past the TTL (or is told to stop).
struct Worker {
    /// The current (or most recent) task — updated on each follow-up — for status
    /// lines and the reports posted back.
    task: String,
    /// The worker's accumulated (channel-stripped) output, grown by its drive
    /// task and read by [`WorkerRegistry::render_status`].
    transcript: Arc<Mutex<String>>,
    /// Follow-up mailbox. Merging a task in resumes the warm session with full
    /// context; if the worker is still mid-prompt, the task is concatenated into the
    /// pending message and picked up whole when it next goes free.
    mailbox: Arc<FollowMailbox>,
    /// Whether the drive loop is mid-prompt right now, vs. idle and resumable.
    busy: Arc<AtomicBool>,
    drive: JoinHandle<()>,
}

/// The scene's live working sessions. Owned by the per-scene loop, so a plain
/// map suffices — no locking. Survives reactor-session hot-swaps: workers are
/// independent of the mind's own lifecycle within a scene.
pub(super) struct WorkerRegistry {
    scene: Scene,
    /// A clone of the scene's queue sender, handed to each worker's drive task so
    /// its reports land back in the same loop.
    inbound: mpsc::Sender<LoopInput>,
    workers: HashMap<WorkerId, Worker>,
    next_id: WorkerId,
}

impl WorkerRegistry {
    pub(super) fn new(scene: Scene, inbound: mpsc::Sender<LoopInput>) -> Self {
        Self {
            scene,
            inbound,
            workers: HashMap::new(),
            next_id: 1,
        }
    }

    /// Spawn a channel-mute working session for `task` on this scene's process
    /// (workers multiplex inside the scene's single subprocess). Returns once the
    /// session is open and its drive task is running; the work proceeds in the
    /// background and reports back through the queue.
    pub(super) async fn spawn(
        &mut self,
        reactor: &Reactor,
        task: String,
    ) -> anyhow::Result<WorkerId> {
        let id = self.next_id;
        self.next_id += 1;

        let session = Arc::new(
            reactor
                .inner
                .agent
                .session(
                    &self.scene,
                    SessionRole::Worker,
                    Some(id),
                    SessionOpts {
                        system_prompt: Some(worker_system_prompt(&self.scene)),
                        // The worker's cwd is the agent's global workspace, so a
                        // build sub-agent works in a real project dir (ls/write).
                        cwd: Some(reactor.inner.workspace_dir.clone()),
                    },
                )
                .await?,
        );

        let observatory = reactor.inner.observatory.clone();
        observatory
            .record(&self.scene, EventKind::WorkerSpawned { id, task: task.clone() })
            .await;

        let transcript = Arc::new(Mutex::new(String::new()));
        let busy = Arc::new(AtomicBool::new(true));
        let mailbox = FollowMailbox::new();
        let drive = tokio::spawn(drive_worker(
            id,
            task.clone(),
            session,
            transcript.clone(),
            self.inbound.clone(),
            observatory,
            self.scene.clone(),
            mailbox.clone(),
            busy.clone(),
        ));

        self.workers.insert(
            id,
            Worker {
                task,
                transcript,
                mailbox,
                busy,
                drive,
            },
        );
        tracing::info!(scene = %self.scene, worker = id, "spawned working session");
        Ok(id)
    }

    /// Resume an existing warm worker with a follow-up `task`, so a refinement
    /// continues the SAME session — full context, no clobbering — instead of a cold
    /// fresh one. The task is *merged* into the worker's mailbox: if it's still
    /// mid-prompt the task is concatenated onto whatever else is pending and the
    /// whole lot is picked up in one go when it next goes free; if it's idle-waiting,
    /// this wakes it. When the target is gone (its idle session already closed, or it
    /// shut down between our lookup and the merge), falls back to spawning a fresh
    /// worker so the request is never silently lost.
    pub(super) async fn follow_up(
        &mut self,
        reactor: &Reactor,
        id: WorkerId,
        task: String,
    ) -> anyhow::Result<WorkerId> {
        if let Some(w) = self.workers.get_mut(&id) {
            // Merge under the mailbox lock — the same critical section the drive
            // loop takes when deciding to close, so we can't lose a task to a
            // simultaneously-closing worker.
            let accepted = {
                let mut st = w.mailbox.state.lock().unwrap();
                if st.closed {
                    false
                } else {
                    st.pending = Some(match st.pending.take() {
                        Some(prev) => format!("{prev}\n\n{task}"),
                        None => task.clone(),
                    });
                    true
                }
            };
            if accepted {
                w.mailbox.notify.notify_one();
                w.task = task.clone();
                reactor
                    .inner
                    .observatory
                    .record(&self.scene, EventKind::WorkerResumed { id, task })
                    .await;
                tracing::info!(scene = %self.scene, worker = id, "merged follow-up into working session");
                return Ok(id);
            }
            // The worker closed itself (idle past TTL) before we got the lock; drop
            // the stale handle and fall through to a fresh spawn.
            self.workers.remove(&id);
        }
        tracing::info!(scene = %self.scene, worker = id, "follow-up target gone; spawning fresh worker");
        self.spawn(reactor, task).await
    }

    /// Forget workers whose drive task has finished, so the map doesn't grow.
    /// Their result already rode back as a report; this just drops the handle.
    pub(super) fn reap(&mut self) {
        self.workers.retain(|_, w| !w.drive.is_finished());
    }

    /// Build a question report for the `ask` tool, attributing it to the worker's
    /// task. The MCP `ask` handler only knows the worker id; the loop owns the
    /// registry, so it resolves the task here. An ask from an unknown id (already
    /// reaped, say) still surfaces, tagged as such.
    pub(super) fn question_report(&self, id: WorkerId, question: String) -> WorkerReport {
        let task = self
            .workers
            .get(&id)
            .map(|w| w.task.clone())
            .unwrap_or_else(|| "(finished worker)".to_string());
        WorkerReport { id, task, kind: WorkerReportKind::Question(question) }
    }

    /// A compact, stable-ordered view of every live worker — its id, task, whether
    /// it's running now or idle-and-resumable, and a short tail of its transcript —
    /// for injection into the reactor's prompt. The id tells the mind which worker
    /// to continue via `delegate worker:<id>`. Empty string when nothing is live.
    pub(super) async fn render_status(&self) -> String {
        if self.workers.is_empty() {
            return String::new();
        }
        let mut ids: Vec<&WorkerId> = self.workers.keys().collect();
        ids.sort();

        let mut s = String::from("## Working sessions (delegated)\n");
        for id in ids {
            let w = &self.workers[id];
            let busy = w.busy.load(Ordering::Relaxed);
            let queued = w.mailbox.state.lock().unwrap().pending.is_some();
            let tail = {
                let t = w.transcript.lock().await;
                tail_chars(&t, 240)
            };
            if busy {
                let suffix = if queued { "; follow-up queued" } else { "" };
                let _ = write!(s, "- worker {id} (running{suffix}): \"{}\"", w.task);
            } else {
                let _ = write!(
                    s,
                    "- worker {id} (idle — resumable: delegate with worker:{id} to continue it): \"{}\"",
                    w.task
                );
            }
            if !tail.is_empty() {
                let _ = write!(s, "\n    latest: {tail}");
            }
            s.push('\n');
        }
        s
    }
}

/// Render one report for the `## New signals` section the reactor sees.
pub(super) fn render_report(report: &WorkerReport) -> String {
    match &report.kind {
        WorkerReportKind::Done(answer) => format!(
            "working session {} finished — task was \"{}\":\n{}",
            report.id,
            report.task,
            answer.trim()
        ),
        WorkerReportKind::Failed(err) => format!(
            "working session {} FAILED — task was \"{}\": {}",
            report.id,
            report.task,
            err.trim()
        ),
        WorkerReportKind::Question(q) => format!(
            "working session {} (task \"{}\") asks: {}",
            report.id,
            report.task,
            q.trim()
        ),
    }
}

/// Drive one worker across one or more tasks, posting a terminal report after each
/// and staying warm in between so a follow-up can resume the same session with full
/// context. Runs as its own task so the reactor stays free; the session is closed
/// (this returns) once the worker sits idle past [`WORKER_IDLE_TTL`].
async fn drive_worker(
    id: WorkerId,
    initial_task: String,
    session: Arc<AcpSession>,
    transcript: Arc<Mutex<String>>,
    inbound: mpsc::Sender<LoopInput>,
    observatory: Observatory,
    scene: Scene,
    mailbox: Arc<FollowMailbox>,
    busy: Arc<AtomicBool>,
) {
    let mut task = initial_task;
    loop {
        busy.store(true, Ordering::Relaxed);
        let kind = match run_worker(id, &task, &session, &transcript, &observatory, &scene).await {
            Ok(answer) => WorkerReportKind::Done(answer),
            Err(err) => WorkerReportKind::Failed(err.to_string()),
        };
        busy.store(false, Ordering::Relaxed);
        let (state, summary_chars) = match &kind {
            WorkerReportKind::Done(answer) => (WorkerState::Done, answer.chars().count()),
            WorkerReportKind::Failed(err) => (WorkerState::Failed, err.chars().count()),
            // Questions are interim, never terminal — a drive pass only ever ends in
            // Done or Failed, so this arm is unreachable, but keep it total.
            WorkerReportKind::Question(_) => (WorkerState::Running, 0),
        };
        observatory
            .record(&scene, EventKind::WorkerFinished { id, state, summary_chars })
            .await;
        let report = WorkerReport { id, task: task.clone(), kind };
        if inbound.send(LoopInput::Worker(report)).await.is_err() {
            tracing::warn!(worker = id, "worker report dropped; scene loop gone");
            return;
        }

        // Stay warm for a follow-up; pick up everything that accumulated in the
        // mailbox as one merged prompt. Close (return, dropping the session) once
        // idle past the TTL.
        match wait_for_followup(&mailbox).await {
            Some(next) => task = next,
            None => {
                tracing::info!(scene = %scene, worker = id, "working session idle past ttl; closing");
                return;
            }
        }
    }
}

/// Block until the worker has a follow-up to run, returning the merged pending
/// message — or `None` if it sat idle past [`WORKER_IDLE_TTL`], in which case the
/// mailbox is flipped `closed` (under the same lock `follow_up` takes) so a racing
/// follow-up spawns a fresh worker instead of merging into a dead one.
async fn wait_for_followup(mailbox: &FollowMailbox) -> Option<String> {
    loop {
        // Fast path: take whatever's already merged in.
        {
            let mut st = mailbox.state.lock().unwrap();
            if let Some(task) = st.pending.take() {
                return Some(task);
            }
        }
        // Nothing pending — wait for a nudge or the idle TTL. `Notify` holds a
        // permit if `notify_one` raced ahead of this `notified()`, so no wakeup is
        // lost between the take above and the wait here.
        match timeout(WORKER_IDLE_TTL, mailbox.notify.notified()).await {
            Ok(()) => continue, // nudged — loop back to take the pending task
            Err(_) => {
                // Idle past the TTL. Decide to close, but yield to a follow-up that
                // landed in the meantime — both resolved under the one lock.
                let mut st = mailbox.state.lock().unwrap();
                if let Some(task) = st.pending.take() {
                    return Some(task);
                }
                st.closed = true;
                return None;
            }
        }
    }
}

/// Prompt the worker session with the task, streaming its output into the
/// transcript, and return the full reply as the task's result. Questions are no
/// longer parsed from the text — the worker raises them by calling the `ask`
/// tool, which arrives on the loop's control channel out of band.
async fn run_worker(
    id: WorkerId,
    task: &str,
    session: &AcpSession,
    transcript: &Arc<Mutex<String>>,
    observatory: &Observatory,
    scene: &Scene,
) -> anyhow::Result<String> {
    let mut run = session.prompt(task.to_string()).await?;
    let mut full = String::new();

    loop {
        match run.next_update().await {
            Some(SessionUpdate::Text(text)) => {
                full.push_str(&text);
                transcript.lock().await.push_str(&text);
                // Mirror the live tail so the dashboard shows what the worker is
                // doing right now.
                observatory.worker_progress(scene, id, &full).await;
            }
            // Thoughts, tool calls, and unmodelled events don't enter the
            // transcript — only the worker's text output does.
            Some(_) => {}
            None => break,
        }
    }

    run.wait().await?;
    Ok(full.trim().to_string())
}

/// Last `n` characters of `s`, flattened to a single line for a status tail.
fn tail_chars(s: &str, n: usize) -> String {
    let trimmed = s.trim();
    let start = trimmed.chars().count().saturating_sub(n);
    let tail: String = trimmed.chars().skip(start).collect();
    tail.replace('\n', " ").trim().to_string()
}
