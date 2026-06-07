//! Observatory — structured visibility into the ACP session lifecycle.
//!
//! ACP sessions are otherwise invisible: a scene's persistent reactor session,
//! ephemeral worker sessions (each on its own subprocess), in-flight prompts,
//! heartbeat hot-swaps and self-alarms all live only as scattered `tracing`
//! lines. The observatory is an additive, cloneable handle (like [`Memory`] or
//! [`TextBus`]) that the reactor, workers and heartbeat feed as
//! those things happen. It keeps two things:
//!
//! - a **live mirror** — the current state per scene (reactor session, workers,
//!   context budget, pending alarms, last turn), for `GET /api/sessions`;
//! - an **event history** — a bounded ring of lifecycle [`SessionEvent`]s plus a
//!   live `broadcast`, streamed verbatim over SSE on `GET /api/sessions/events`,
//!   and best-effort appended to `<data_dir>/sessions.jsonl` for durable replay.
//!
//! Recording an event mutates the mirror, pushes to the ring, appends to the
//! journal and broadcasts it — all under one lock so an SSE subscriber that
//! snapshots-then-subscribes can neither miss an event nor see a duplicate.
//!
//! [`Memory`]: crate::memory::Memory
//! [`TextBus`]: crate::server::TextBus

use std::collections::{HashMap, VecDeque};
use std::convert::Infallible;
use std::path::PathBuf;
use std::sync::Arc;

use chrono::{DateTime, Utc};
use serde::Serialize;
use tokio::sync::{Mutex, RwLock, broadcast};

use crate::types::Scene;

/// How many recent events the in-memory ring retains for SSE replay-on-connect.
const HISTORY_CAP: usize = 1000;
/// Broadcast backlog; a subscriber that lags past this misses events (logged on
/// the wire as a gap, never blocks the producer).
const BROADCAST_CAP: usize = 512;
/// Characters of a worker's transcript kept as a live tail for the mirror.
const TRANSCRIPT_TAIL: usize = 240;

/// Which kind of ACP session this is — the reactor's persistent mind, an
/// ephemeral worker, or the throwaway summarizer a hot-swap briefs from.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum SessionKind {
    Reactor,
    Worker,
    Summarizer,
}

/// Live state of one ACP session in the mirror.
#[derive(Debug, Clone, Serialize)]
pub struct SessionView {
    pub id: String,
    pub kind: SessionKind,
    pub opened_at: DateTime<Utc>,
    /// True while a prompt is mid-flight on this session.
    pub in_flight: bool,
    /// Completed turns/prompts driven on this session.
    pub turns: u64,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum WorkerState {
    Running,
    Done,
    Failed,
}

/// Live state of one working session in the mirror.
#[derive(Debug, Clone, Serialize)]
pub struct WorkerView {
    pub id: u64,
    pub task: String,
    pub state: WorkerState,
    pub started_at: DateTime<Utc>,
    /// Most recent `[[ask]]` the worker raised, if any (it kept going regardless).
    pub last_question: Option<String>,
    /// A short tail of the worker's transcript, for an at-a-glance "what's it doing".
    pub transcript_tail: String,
}

/// A pending self-alarm the mind scheduled, shown until it fires.
#[derive(Debug, Clone, Serialize)]
pub struct AlarmView {
    pub note: String,
    pub fires_at: DateTime<Utc>,
}

/// The most recent turn on a scene's reactor session.
#[derive(Debug, Clone, Serialize)]
pub struct TurnView {
    pub turn: u64,
    pub started_at: DateTime<Utc>,
    pub finished_at: Option<DateTime<Utc>>,
    pub stop_reason: Option<String>,
    pub reply_chars: Option<usize>,
}

/// The full live picture of one scene, served by `GET /api/sessions`.
#[derive(Debug, Clone, Serialize)]
pub struct SceneView {
    pub scene: Scene,
    pub reactor_session: Option<SessionView>,
    pub workers: Vec<WorkerView>,
    /// Accumulated prompt+reply chars since the live session was last opened/swapped.
    pub budget_chars: usize,
    /// The soft ceiling at which the heartbeat hot-swaps (mirrors `SWAP_AFTER_CHARS`).
    pub swap_after_chars: usize,
    pub swap_count: u64,
    pub last_swap_at: Option<DateTime<Utc>>,
    pub pending_alarms: Vec<AlarmView>,
    pub last_turn: Option<TurnView>,
    pub turns_total: u64,
}

impl SceneView {
    fn new(scene: Scene, swap_after_chars: usize) -> Self {
        Self {
            scene,
            reactor_session: None,
            workers: Vec::new(),
            budget_chars: 0,
            swap_after_chars,
            swap_count: 0,
            last_swap_at: None,
            pending_alarms: Vec::new(),
            last_turn: None,
            turns_total: 0,
        }
    }
}

/// One lifecycle event — the unit of the SSE stream, the ring, and `sessions.jsonl`.
#[derive(Debug, Clone, Serialize)]
pub struct SessionEvent {
    /// Monotonic, gap-free sequence number assigned at record time.
    pub seq: u64,
    pub ts: DateTime<Utc>,
    pub scene: Scene,
    #[serde(flatten)]
    pub kind: EventKind,
}

/// The shape of each lifecycle event. Serialized with an `"event"` tag so the
/// wire form is `{ "seq", "ts", "scene", "event": "...", ...fields }`.
#[derive(Debug, Clone, Serialize)]
#[serde(tag = "event", rename_all = "snake_case")]
pub enum EventKind {
    SessionOpened { kind: SessionKind, id: String },
    SessionClosed { kind: SessionKind, id: String },
    /// `input` is the human-readable incoming message(s) for this turn — the new
    /// signals batch (human utterances, worker reports, fired alarms), not the
    /// full seeded prompt.
    TurnStarted { turn: u64, input: String },
    /// `reply` is the agent's spoken text for this turn (markers stripped).
    TurnFinished { turn: u64, stop_reason: Option<String>, reply_chars: usize, reply: String },
    HotSwap { old_id: String, new_id: String, briefing_chars: usize },
    WorkerSpawned { id: u64, task: String },
    WorkerFinished { id: u64, state: WorkerState, summary_chars: usize },
    WorkerQuestion { id: u64, question: String },
    AlarmScheduled { note: String, delay_s: u64 },
    AlarmFired { note: String },
}

/// Cloneable handle over the shared observatory state.
#[derive(Clone)]
pub struct Observatory {
    inner: Arc<Inner>,
}

struct Inner {
    scenes: RwLock<HashMap<Scene, SceneView>>,
    history: Mutex<History>,
    tx: broadcast::Sender<SessionEvent>,
    /// Where to append the durable event log, or `None` to skip persistence.
    jsonl: Option<PathBuf>,
    swap_after_chars: usize,
}

struct History {
    seq: u64,
    ring: VecDeque<SessionEvent>,
}

impl Observatory {
    /// Build an observatory. `jsonl` is where to append durable events (created
    /// lazily on first append); pass `None` to keep history in-memory only.
    /// `swap_after_chars` is the heartbeat's swap ceiling, surfaced per scene so
    /// the dashboard can render the budget as a fraction.
    pub fn new(jsonl: Option<PathBuf>, swap_after_chars: usize) -> Self {
        let (tx, _) = broadcast::channel(BROADCAST_CAP);
        Self {
            inner: Arc::new(Inner {
                scenes: RwLock::new(HashMap::new()),
                history: Mutex::new(History { seq: 0, ring: VecDeque::new() }),
                tx,
                jsonl,
                swap_after_chars,
            }),
        }
    }

    /// Record one lifecycle event: assign a seq, mutate the live mirror, push to
    /// the ring, append to the journal, and broadcast — the ring push and the
    /// broadcast both happen under the history lock so a concurrent
    /// [`subscribe`](Self::subscribe) sees a consistent, dup-free cut.
    pub async fn record(&self, scene: &Scene, kind: EventKind) {
        // Mirror first (its own lock), so a snapshot taken right after the event
        // lands reflects it.
        self.apply_to_mirror(scene, &kind).await;

        let mut hist = self.inner.history.lock().await;
        hist.seq += 1;
        let event = SessionEvent {
            seq: hist.seq,
            ts: Utc::now(),
            scene: scene.clone(),
            kind,
        };
        hist.ring.push_back(event.clone());
        while hist.ring.len() > HISTORY_CAP {
            hist.ring.pop_front();
        }
        if let Some(path) = &self.inner.jsonl {
            append_jsonl(path, &event).await;
        }
        // Ignore the error: no subscribers is fine. Held under the history lock
        // so `subscribe` cannot interleave between the ring push and this send.
        let _ = self.inner.tx.send(event);
    }

    /// A live snapshot of every scene, newest-process-first is not meaningful —
    /// scenes are returned in arbitrary map order; the dashboard sorts by name.
    pub async fn snapshot(&self) -> Vec<SceneView> {
        self.inner.scenes.read().await.values().cloned().collect()
    }

    /// Snapshot the event ring and subscribe to the live feed atomically. The
    /// returned `Vec` is everything recorded so far; the receiver yields every
    /// event recorded after this call. Because [`record`](Self::record)
    /// broadcasts under the same lock we hold here, the two never overlap — no
    /// event is both replayed and live.
    pub async fn subscribe(&self) -> (Vec<SessionEvent>, broadcast::Receiver<SessionEvent>) {
        let hist = self.inner.history.lock().await;
        let rx = self.inner.tx.subscribe();
        let replay = hist.ring.iter().cloned().collect();
        (replay, rx)
    }

    /// Update a scene's accumulated context budget (mirror-only; not an event —
    /// it changes every turn and matters as state, not as history).
    pub async fn set_budget(&self, scene: &Scene, chars: usize) {
        let mut scenes = self.inner.scenes.write().await;
        scenes.entry(scene.clone()).or_insert_with(|| self.fresh(scene)).budget_chars = chars;
    }

    /// Update a worker's transcript tail in the mirror (mirror-only; high-frequency).
    pub async fn worker_progress(&self, scene: &Scene, worker_id: u64, transcript: &str) {
        let mut scenes = self.inner.scenes.write().await;
        if let Some(view) = scenes.get_mut(scene)
            && let Some(w) = view.workers.iter_mut().find(|w| w.id == worker_id)
        {
            w.transcript_tail = tail_chars(transcript, TRANSCRIPT_TAIL);
        }
    }

    fn fresh(&self, scene: &Scene) -> SceneView {
        SceneView::new(scene.clone(), self.inner.swap_after_chars)
    }

    /// Fold an event into the live mirror. Pure state transition; no I/O.
    async fn apply_to_mirror(&self, scene: &Scene, kind: &EventKind) {
        let now = Utc::now();
        let mut scenes = self.inner.scenes.write().await;
        let view = scenes
            .entry(scene.clone())
            .or_insert_with(|| SceneView::new(scene.clone(), self.inner.swap_after_chars));

        match kind {
            EventKind::SessionOpened { kind, id } => match kind {
                SessionKind::Reactor => {
                    view.reactor_session = Some(SessionView {
                        id: id.clone(),
                        kind: SessionKind::Reactor,
                        opened_at: now,
                        in_flight: false,
                        turns: 0,
                    });
                }
                // Worker open is mirrored by WorkerSpawned; the summarizer is a
                // throwaway we don't surface as a standing session.
                SessionKind::Worker | SessionKind::Summarizer => {}
            },
            EventKind::SessionClosed { .. } => {
                // Reactor close is rare (only error teardown); workers are removed
                // on WorkerFinished. Nothing to do for the summarizer.
            }
            EventKind::TurnStarted { turn, .. } => {
                if let Some(s) = view.reactor_session.as_mut() {
                    s.in_flight = true;
                }
                view.last_turn = Some(TurnView {
                    turn: *turn,
                    started_at: now,
                    finished_at: None,
                    stop_reason: None,
                    reply_chars: None,
                });
            }
            EventKind::TurnFinished { turn, stop_reason, reply_chars, .. } => {
                if let Some(s) = view.reactor_session.as_mut() {
                    s.in_flight = false;
                    s.turns += 1;
                }
                view.turns_total += 1;
                view.last_turn = Some(TurnView {
                    turn: *turn,
                    started_at: view
                        .last_turn
                        .as_ref()
                        .filter(|t| t.turn == *turn)
                        .map(|t| t.started_at)
                        .unwrap_or(now),
                    finished_at: Some(now),
                    stop_reason: stop_reason.clone(),
                    reply_chars: Some(*reply_chars),
                });
            }
            EventKind::HotSwap { new_id, .. } => {
                view.swap_count += 1;
                view.last_swap_at = Some(now);
                view.budget_chars = 0;
                view.reactor_session = Some(SessionView {
                    id: new_id.clone(),
                    kind: SessionKind::Reactor,
                    opened_at: now,
                    in_flight: false,
                    turns: 0,
                });
            }
            EventKind::WorkerSpawned { id, task } => {
                view.workers.push(WorkerView {
                    id: *id,
                    task: task.clone(),
                    state: WorkerState::Running,
                    started_at: now,
                    last_question: None,
                    transcript_tail: String::new(),
                });
            }
            EventKind::WorkerFinished { id, state, .. } => {
                if let Some(w) = view.workers.iter_mut().find(|w| w.id == *id) {
                    w.state = *state;
                }
            }
            EventKind::WorkerQuestion { id, question } => {
                if let Some(w) = view.workers.iter_mut().find(|w| w.id == *id) {
                    w.last_question = Some(question.clone());
                }
            }
            EventKind::AlarmScheduled { note, delay_s } => {
                view.pending_alarms.push(AlarmView {
                    note: note.clone(),
                    fires_at: now + chrono::Duration::seconds(*delay_s as i64),
                });
            }
            EventKind::AlarmFired { note } => {
                // Drop the earliest pending alarm matching this note.
                if let Some(idx) = view.pending_alarms.iter().position(|a| &a.note == note) {
                    view.pending_alarms.remove(idx);
                }
            }
        }
    }

    /// Test/util: total events recorded so far.
    #[cfg(test)]
    pub async fn event_count(&self) -> u64 {
        self.inner.history.lock().await.seq
    }
}

/// Best-effort append of one event as a JSON line. Failures are logged and
/// swallowed — the durable log is a convenience, never load-bearing.
async fn append_jsonl(path: &PathBuf, event: &SessionEvent) {
    use tokio::io::AsyncWriteExt;
    let mut line = match serde_json::to_string(event) {
        Ok(s) => s,
        Err(err) => {
            tracing::warn!(%err, "observatory: serialize event failed");
            return;
        }
    };
    line.push('\n');
    let file = tokio::fs::OpenOptions::new()
        .create(true)
        .append(true)
        .open(path)
        .await;
    match file {
        Ok(mut f) => {
            if let Err(err) = f.write_all(line.as_bytes()).await {
                tracing::warn!(%err, "observatory: append to sessions.jsonl failed");
            }
        }
        Err(err) => {
            tracing::warn!(%err, path = %path.display(), "observatory: open sessions.jsonl failed");
        }
    }
}

/// Last `n` characters of `s`, flattened to a single line.
fn tail_chars(s: &str, n: usize) -> String {
    let trimmed = s.trim();
    let start = trimmed.chars().count().saturating_sub(n);
    let tail: String = trimmed.chars().skip(start).collect();
    tail.replace('\n', " ").trim().to_string()
}

/// Convenience for the SSE handler: turn a broadcast receiver into a stream of
/// events, skipping lag gaps and ending on close.
pub fn event_stream(
    rx: broadcast::Receiver<SessionEvent>,
) -> impl futures::Stream<Item = Result<SessionEvent, Infallible>> {
    futures::stream::unfold(rx, |mut rx| async move {
        loop {
            match rx.recv().await {
                Ok(ev) => return Some((Ok(ev), rx)),
                Err(broadcast::error::RecvError::Lagged(n)) => {
                    tracing::warn!(skipped = n, "observatory: SSE subscriber lagged");
                    continue;
                }
                Err(broadcast::error::RecvError::Closed) => return None,
            }
        }
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    fn scene() -> Scene {
        Scene("alice@phone".to_string())
    }

    #[tokio::test]
    async fn mirrors_reactor_session_and_turn() {
        let obs = Observatory::new(None, 48_000);
        let s = scene();
        obs.record(
            &s,
            EventKind::SessionOpened { kind: SessionKind::Reactor, id: "sess-1".into() },
        )
        .await;
        obs.record(&s, EventKind::TurnStarted { turn: 0, input: "hi".into() }).await;

        let snap = obs.snapshot().await;
        assert_eq!(snap.len(), 1);
        let v = &snap[0];
        let rs = v.reactor_session.as_ref().unwrap();
        assert_eq!(rs.id, "sess-1");
        assert!(rs.in_flight, "turn in flight");

        obs.record(
            &s,
            EventKind::TurnFinished {
                turn: 0,
                stop_reason: Some("end_turn".into()),
                reply_chars: 42,
                reply: "hello there".into(),
            },
        )
        .await;
        let v = &obs.snapshot().await[0];
        assert!(!v.reactor_session.as_ref().unwrap().in_flight);
        assert_eq!(v.turns_total, 1);
        assert_eq!(v.last_turn.as_ref().unwrap().reply_chars, Some(42));
    }

    #[tokio::test]
    async fn hot_swap_resets_budget_and_bumps_count() {
        let obs = Observatory::new(None, 48_000);
        let s = scene();
        obs.record(
            &s,
            EventKind::SessionOpened { kind: SessionKind::Reactor, id: "old".into() },
        )
        .await;
        obs.set_budget(&s, 50_000).await;
        obs.record(
            &s,
            EventKind::HotSwap { old_id: "old".into(), new_id: "new".into(), briefing_chars: 800 },
        )
        .await;
        let v = &obs.snapshot().await[0];
        assert_eq!(v.swap_count, 1);
        assert_eq!(v.budget_chars, 0);
        assert_eq!(v.reactor_session.as_ref().unwrap().id, "new");
    }

    #[tokio::test]
    async fn worker_lifecycle() {
        let obs = Observatory::new(None, 48_000);
        let s = scene();
        obs.record(&s, EventKind::WorkerSpawned { id: 1, task: "research X".into() }).await;
        obs.worker_progress(&s, 1, "looking into it...").await;
        obs.record(&s, EventKind::WorkerQuestion { id: 1, question: "which region?".into() }).await;
        let v = &obs.snapshot().await[0];
        assert_eq!(v.workers.len(), 1);
        assert_eq!(v.workers[0].state, WorkerState::Running);
        assert_eq!(v.workers[0].transcript_tail, "looking into it...");
        assert_eq!(v.workers[0].last_question.as_deref(), Some("which region?"));

        obs.record(
            &s,
            EventKind::WorkerFinished { id: 1, state: WorkerState::Done, summary_chars: 120 },
        )
        .await;
        assert_eq!(obs.snapshot().await[0].workers[0].state, WorkerState::Done);
    }

    #[tokio::test]
    async fn subscribe_replays_then_streams_live_without_dup() {
        let obs = Observatory::new(None, 48_000);
        let s = scene();
        obs.record(
            &s,
            EventKind::SessionOpened { kind: SessionKind::Reactor, id: "sess-1".into() },
        )
        .await;
        let (replay, mut rx) = obs.subscribe().await;
        assert_eq!(replay.len(), 1);
        assert_eq!(replay[0].seq, 1);

        obs.record(
            &s,
            EventKind::SessionClosed { kind: SessionKind::Reactor, id: "sess-1".into() },
        )
        .await;
        let live = rx.recv().await.unwrap();
        assert_eq!(live.seq, 2, "live event follows replay with no gap or dup");
    }

    #[tokio::test]
    async fn alarms_scheduled_and_fired() {
        let obs = Observatory::new(None, 48_000);
        let s = scene();
        obs.record(&s, EventKind::AlarmScheduled { note: "wake them".into(), delay_s: 60 }).await;
        assert_eq!(obs.snapshot().await[0].pending_alarms.len(), 1);
        obs.record(&s, EventKind::AlarmFired { note: "wake them".into() }).await;
        assert!(obs.snapshot().await[0].pending_alarms.is_empty());
    }
}
