//! Working sessions — the reactor's hands.
//!
//! The reactor keeps a single voice and must never block the floor on slow
//! work, so heavy or long-running tasks are delegated here. A worker is a
//! *voice-mute capability within the scene*: it has the full substrate — the
//! scene's memory, tools, code execution, the right to spawn further workers —
//! but no voice of its own. It never speaks: it cannot emit on the reactor's
//! expression channels (thought / audio / surface). That voice-mute-ness is what
//! preserves single-voice coherence: only the reactor speaks to the person.
//!
//! It is *not*, however, channel-blind. Over hi-agent's own HTTP surface
//! (`HI_AGENT_BASE_URL` in its env) a worker may **perceive input channels**
//! (e.g. `GET /api/vision` for live frames) and **drive the non-voice `overlay`
//! channel** (`POST /api/overlay`) — the deliberate continuous-data exception
//! that lets a worker, say, run face detection and push rects to the UI without
//! ever holding the voice. Both ride *outside* the turn loop, so they never
//! contend with the reactor's serialized speech.
//!
//! The collaboration bus is asynchronous and worker→reactor here: a worker runs
//! to completion (or until it must ask something), then posts a [`WorkerReport`]
//! back into the scene's queue as a `LoopInput::Worker`. It never interrupts live
//! speech — the report waits its turn like any other input, and the next turn
//! folds it into what the mind says. Questions are *non-blocking*: a worker that
//! hits ambiguity flags it via `[[ask]]` and then proceeds on its best
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

use tokio::sync::{Mutex, mpsc};
use tokio::task::JoinHandle;

use crate::acp::{AcpSession, SessionOpts, SessionUpdate};
use crate::types::Scene;

use super::{LoopInput, MarkerExtractor, Reactor};

/// Per-scene-unique handle for a working session. Small and `Copy`; it tags the
/// worker in status lines and in the reports it posts back.
pub(super) type WorkerId = u64;

/// A worker flags a question it needs the reactor to weigh in on with these
/// markers, then proceeds on its best assumption. The content is lifted out of
/// the transcript and posted as a [`WorkerReportKind::Question`].
const OPEN_ASK: &str = "[[ask]]";
const CLOSE_ASK: &str = "[[/ask]]";

const WORKER_SYSTEM_PROMPT: &str = "You are a working session spun up by a \
human-interface agent to carry out one specific delegated task. You have full \
access to files, code execution, memory, and the rest of the harness's tools — \
use them freely to actually complete the work, not merely plan it.\n\
\n\
You have no voice of your own. You are not talking to the human, and nothing \
you write is spoken aloud — never try to address the person. The agent owns the \
voice: do NOT write to the thought, audio, or surface channels. Your job is to \
DO the task and then report the result: finish with a clear, self-contained \
summary of what you did and what came of it. That summary is handed back to the \
agent verbatim, so include everything it needs to act on or to relay — don't \
assume it can see your working notes.\n\
\n\
You ARE allowed to use hi-agent's own channels for perception and for the one \
non-voice output, the overlay. The server's base URL is in the \
`HI_AGENT_BASE_URL` environment variable, and your scene is `{scene}` — send it \
as the `X-HI-Scene` header on every request. Specifically:\n\
- Perceive input channels, e.g. live camera frames:\n\
    `GET $HI_AGENT_BASE_URL/api/vision` with header `X-HI-Scene: {scene}`\n\
  (one frame per response; re-request for the next). Process the raw bytes \
however the task needs — detection, CV, etc. is your job.\n\
- Drive the overlay (a continuous, non-voice visual channel — e.g. face rects):\n\
    `POST $HI_AGENT_BASE_URL/api/overlay` with header `X-HI-Scene: {scene}` and \
a JSON body (one frame per POST; the UI repaints each line).\n\
The overlay is the ONLY thing you may write to the person's screen, and it is \
not speech — never use it to talk. Speaking stays the agent's job.\n\
\n\
If you hit something genuinely ambiguous, do not stall waiting for an answer. \
Make the most reasonable assumption, note it, and keep going — the agent can \
correct course later. If you must surface a question, wrap it in `[[ask]] … \
[[/ask]]` markers and then proceed on your best assumption anyway. Work to \
completion.";

/// The worker's system prompt, with its scene interpolated so it can tag every
/// channel request with the right `X-HI-Scene`. The server base URL is delivered
/// out-of-band in the subprocess env ([`crate::config::ENV_SERVER_BASE_URL`]),
/// which the prompt references as `$HI_AGENT_BASE_URL`.
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

/// One live working session. The registry holds it only to inspect its
/// transcript and to know when its drive task has finished; the drive task owns
/// the session itself and closes it on completion.
struct Worker {
    task: String,
    /// The worker's accumulated (channel-stripped) output, grown by its drive
    /// task and read by [`WorkerRegistry::render_status`].
    transcript: Arc<Mutex<String>>,
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
                    SessionOpts {
                        system_prompt: Some(worker_system_prompt(&self.scene)),
                        cwd: None,
                    },
                )
                .await?,
        );

        let transcript = Arc::new(Mutex::new(String::new()));
        let drive = tokio::spawn(drive_worker(
            id,
            task.clone(),
            session,
            transcript.clone(),
            self.inbound.clone(),
        ));

        self.workers.insert(
            id,
            Worker {
                task,
                transcript,
                drive,
            },
        );
        tracing::info!(scene = %self.scene, worker = id, "spawned working session");
        Ok(id)
    }

    /// Forget workers whose drive task has finished, so the map doesn't grow.
    /// Their result already rode back as a report; this just drops the handle.
    pub(super) fn reap(&mut self) {
        self.workers.retain(|_, w| !w.drive.is_finished());
    }

    /// A compact, stable-ordered view of every running worker — its task and a
    /// short tail of its transcript — for injection into the reactor's prompt.
    /// Empty string when nothing is delegated.
    pub(super) async fn render_status(&self) -> String {
        if self.workers.is_empty() {
            return String::new();
        }
        let mut ids: Vec<&WorkerId> = self.workers.keys().collect();
        ids.sort();

        let mut s = String::from("## Working sessions (delegated, running now)\n");
        for id in ids {
            let w = &self.workers[id];
            let tail = {
                let t = w.transcript.lock().await;
                tail_chars(&t, 240)
            };
            let _ = write!(s, "- worker {id}: \"{}\"", w.task);
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

/// Drive one worker to completion, then post a terminal report. Runs as its own
/// task so the reactor stays free; the session is closed on the way out.
async fn drive_worker(
    id: WorkerId,
    task: String,
    session: Arc<AcpSession>,
    transcript: Arc<Mutex<String>>,
    inbound: mpsc::Sender<LoopInput>,
) {
    let kind = match run_worker(id, &task, &session, &transcript, &inbound).await {
        Ok(answer) => WorkerReportKind::Done(answer),
        Err(err) => WorkerReportKind::Failed(err.to_string()),
    };
    let report = WorkerReport { id, task, kind };
    if let Err(err) = inbound.send(LoopInput::Worker(report)).await {
        tracing::warn!(worker = id, error = %err, "worker report dropped; scene loop gone");
    }
}

/// Prompt the worker session with the task, stream its output into the
/// transcript while lifting out `[[ask]]` questions (posted as they appear),
/// and return the full reply as the task's result.
async fn run_worker(
    id: WorkerId,
    task: &str,
    session: &AcpSession,
    transcript: &Arc<Mutex<String>>,
    inbound: &mpsc::Sender<LoopInput>,
) -> anyhow::Result<String> {
    let mut run = session.prompt(task.to_string()).await?;
    let mut asks = MarkerExtractor::new(OPEN_ASK, CLOSE_ASK);
    let mut full = String::new();

    loop {
        match run.next_update().await {
            Some(SessionUpdate::Text(text)) => {
                let (clean, questions) = asks.push(&text);
                if !clean.is_empty() {
                    full.push_str(&clean);
                    transcript.lock().await.push_str(&clean);
                }
                for q in questions {
                    let report = WorkerReport {
                        id,
                        task: task.to_string(),
                        kind: WorkerReportKind::Question(q),
                    };
                    let _ = inbound.send(LoopInput::Worker(report)).await;
                }
            }
            // Thoughts, tool calls, and unmodelled events don't enter the
            // transcript — only the worker's text output does.
            Some(_) => {}
            None => break,
        }
    }

    let tail = asks.flush();
    if !tail.is_empty() {
        full.push_str(&tail);
        transcript.lock().await.push_str(&tail);
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
