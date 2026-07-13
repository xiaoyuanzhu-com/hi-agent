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
//!    A voice barge-in — the human talking over the agent's playback — is no
//!    exception: the client ducks on its own, the words buffer like any other
//!    signal, and the mind merely learns afterwards what went unheard. See
//!    [`interrupts`].
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
use std::path::PathBuf;
use std::sync::Arc;
use std::sync::atomic::{AtomicU32, AtomicU64, Ordering};
use std::time::Duration;

mod heartbeat;
mod interleave;
mod interrupts;
pub mod outbound;
mod sequencer;
mod tools;
mod workers;

pub use interrupts::InterruptRegistry;
pub use outbound::OutboundSignal;
pub use tools::{SceneControl, ToolRegistry, ToolSink};

/// The heartbeat's soft context-budget ceiling, surfaced so the observatory can
/// render each scene's budget as a fraction of where a hot-swap kicks in.
pub fn swap_budget_chars() -> usize {
    heartbeat::swap_after_chars()
}

use chrono::Utc;
use tokio::sync::{Mutex, mpsc, oneshot};
use tokio::time::{Instant, sleep_until, timeout};

use crate::foundation::acp::{AcpSession, SessionOpts, SessionUpdate};
use crate::foundation::agent::{AgentLayer, SessionRole};
use crate::foundation::config;
use crate::foundation::shutdown::Shutdown;
use crate::mind::memory::{Memory, build_for_scene, working_set};
use crate::foundation::observatory::{EventKind, Observatory, SessionKind};
use crate::types::{Channel, Geometry, JournalEntry, Origin, Scene, Signal, ViewEnvelope, ViewOp};
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

/// Ceiling on a between-turns hot-swap. The swap prompts the *live* session for a
/// self-briefing with unbounded awaits beneath it; if that session is wedged (a
/// pathological turn can leave the subprocess unresponsive), an un-capped swap
/// blocks the scene loop forever — signals keep queueing but no turn ever runs,
/// and the scene goes deaf until a restart. On expiry the session is discarded:
/// it ignored a prompt for this whole window, so the journal cold-open path is
/// strictly better than waiting.
const SWAP_TIMEOUT: Duration = Duration::from_secs(180);

/// Default idle interval between host pulses — the scene's recurring moment of
/// self-attention. A pulse is not a schedule of work: it injects bare situational
/// facts ("nothing new for 30m") and core.md tells the mind what such a moment is
/// for (review commitments, glance at setups it owns); most pulses should
/// conclude with nothing to do or say. Override via `pulse`; `0`/`off`
/// disables. Boot is not a special case — the first pulse after the host starts
/// simply carries that fact.
const DEFAULT_PULSE: Duration = Duration::from_secs(1800);

/// Resolve the pulse interval from the stored `pulse` tunable in alarm-delay grammar
/// if set (`None` for `0`/`off` — pulses disabled), else [`DEFAULT_PULSE`].
fn pulse_interval() -> Option<Duration> {
    duration_tunable(config::tunables::get(config::KEY_PULSE), DEFAULT_PULSE)
}

/// Whether the reflection ("sleep") pass runs at all. On unless the stored `reflect`
/// tunable is `off` — a master escape hatch to disable consolidation without
/// touching the cadence (see [`reflect_interval`]).
fn reflect_enabled() -> bool {
    !config::tunables::get(config::KEY_REFLECT)
        .map(|v| v.eq_ignore_ascii_case("off"))
        .unwrap_or(false)
}

/// Default base reflection cadence — how often a scene with fresh input
/// consolidates ([`reflect_interval`]). The idle backoff grows from here.
const DEFAULT_REFLECT_EVERY: Duration = Duration::from_secs(60);
/// Default ceiling on the idle backoff ([`reflect_max_interval`]): a long-quiet
/// scene re-checks at most this often.
const DEFAULT_REFLECT_MAX: Duration = Duration::from_secs(8 * 3600);

/// Resolve a stored duration tunable in alarm-delay grammar (`90s`/`30m`/`1h`; bare
/// integer = seconds): `None` for `off`/`0` (disabled), the parsed value, or
/// `default` when unset / unparseable. (The value is already trimmed / non-empty by
/// [`config::tunables::get`].)
fn duration_tunable(value: Option<String>, default: Duration) -> Option<Duration> {
    match value {
        None => Some(default),
        Some(v) if v.eq_ignore_ascii_case("off") => None,
        Some(v) => match parse_delay(&v) {
            Some(d) if d.is_zero() => None,
            Some(d) => Some(d),
            None => Some(default),
        },
    }
}

/// The base reflection cadence, or `None` if reflection is off
/// (`reflect=off`) or `reflect_every` is `0`/`off`. A scene with
/// fresh input consolidates this often; once it goes quiet the gap backs off from
/// here up to [`reflect_max_interval`].
fn reflect_interval() -> Option<Duration> {
    reflect_enabled()
        .then(|| duration_tunable(config::tunables::get(config::KEY_REFLECT_EVERY), DEFAULT_REFLECT_EVERY))
        .flatten()
}

/// The ceiling on the idle backoff: a caught-up, quiet scene doubles its gap from
/// the base each pass but never past this. Always returns a value (no `off`); a
/// `0`/blank `reflect_max` falls back to the default.
fn reflect_max_interval() -> Duration {
    duration_tunable(config::tunables::get(config::KEY_REFLECT_MAX), DEFAULT_REFLECT_MAX)
        .unwrap_or(DEFAULT_REFLECT_MAX)
}

/// Default base gap for a transient-outage retry (429 / generic). The gap doubles
/// on each failed retry toward [`BACKOFF_CAP`]; 30s is unobtrusive and won't hammer
/// a throttled gateway.
const DEFAULT_VENDOR_PROBE: Duration = Duration::from_secs(30);
/// Default consecutive *generic* terminal failures before flipping to an informed
/// backoff. Each terminal failure is already up to 3 model calls, so 2 = a real
/// outage, not a one-off blip. (402/429 bypass this — they flip immediately.)
const DEFAULT_VENDOR_DOWN_AFTER: u32 = 2;
/// A transient-outage retry never waits longer than this — the 1h ceiling.
const BACKOFF_CAP: Duration = Duration::from_secs(3600);

/// The base transient-outage retry gap. `vendor_probe` in alarm-delay grammar;
/// `off`/`0`/unset/unparseable → default. (Kept under the historical config key.)
fn backoff_base() -> Duration {
    duration_tunable(config::tunables::get(config::KEY_VENDOR_PROBE), DEFAULT_VENDOR_PROBE)
        .unwrap_or(DEFAULT_VENDOR_PROBE)
}

/// The consecutive generic-failure count that flips the reactor into an informed
/// backoff. `vendor_down_after`; `0`/unparseable → default.
fn vendor_down_after() -> u32 {
    config::tunables::get(config::KEY_VENDOR_DOWN_AFTER)
        .and_then(|v| v.parse::<u32>().ok())
        .filter(|n| *n > 0)
        .unwrap_or(DEFAULT_VENDOR_DOWN_AFTER)
}

/// How a scene loop should treat the vendor right now — the read side of [`Vendor`].
#[derive(Clone, Copy, Debug)]
enum SceneGate {
    /// Reachable: drive turns normally.
    Go,
    /// Transient outage (429 / generic): hold mail, and drive a catch-up turn once
    /// `at` (the current backoff deadline) passes. A failed retry grows the gap.
    Retry { at: Instant },
}

/// The vendor's reachability and, when down, how to recover from it.
#[derive(Clone, Copy, Debug)]
enum VendorState {
    Up,
    /// Transient backoff (429 / generic). `try_at` is the next retry deadline;
    /// `attempt` grows the gap toward [`BACKOFF_CAP`]; `silent` suppresses the user
    /// notice for a pure rate-limit (429), which the user needn't hear about.
    Backoff { try_at: Instant, attempt: u32, silent: bool },
}

/// Shared, process-wide view of the upstream LLM vendor and how to recover from an
/// outage. Every scene loop reads it (via [`Vendor::scene_gate`]) to decide whether
/// and when to drive a turn; `run_turn`'s terminal path writes it. The vendor is a
/// shared resource, so one scene detecting an outage steers all of them.
///
/// The `note_*` writers return whether the transition warrants a *one-time* user
/// notice (so the reactor announces "can't reach the model" exactly once),
/// mirroring the old flip-once contract.
struct Vendor {
    state: std::sync::Mutex<VendorState>,
    /// Consecutive *generic* (Unreachable) failures, to absorb a blip before an
    /// informed backoff. Accessed only under `state`'s lock, so effectively part of
    /// the same critical section. Reset on success.
    generic_failures: AtomicU32,
    down_after: u32,
    /// The transient-outage retry base; the gap is `base · 2^attempt`, capped at 1h.
    base: Duration,
}

impl Vendor {
    fn new(down_after: u32, base: Duration) -> Self {
        Self {
            state: std::sync::Mutex::new(VendorState::Up),
            generic_failures: AtomicU32::new(0),
            down_after,
            base,
        }
    }

    fn is_down(&self) -> bool {
        !matches!(*self.state.lock().unwrap(), VendorState::Up)
    }

    /// The retry gap for the given attempt: `base · 2^attempt`, capped at 1h.
    fn backoff(&self, attempt: u32) -> Duration {
        let base = self.base.as_secs().max(1);
        let secs = base.saturating_mul(1u64 << attempt.min(20));
        Duration::from_secs(secs.min(BACKOFF_CAP.as_secs()))
    }

    /// The scene loop's scheduling read: drive now (Go) or retry at a deadline (Retry).
    fn scene_gate(&self) -> SceneGate {
        match *self.state.lock().unwrap() {
            VendorState::Up => SceneGate::Go,
            VendorState::Backoff { try_at, .. } => SceneGate::Retry { at: try_at },
        }
    }

    /// Terminal generic outage. Absorb one blip via `down_after`, then flip to an
    /// *informed* backoff. Returns `true` exactly on that flip (announce once);
    /// `false` while still absorbing or already backing off.
    fn note_unreachable(&self) -> bool {
        let mut st = self.state.lock().unwrap();
        match *st {
            // Already backing off — a failed retry just grows the gap.
            VendorState::Backoff { attempt, silent, .. } => {
                let a = attempt.saturating_add(1);
                *st = VendorState::Backoff { try_at: Instant::now() + self.backoff(a), attempt: a, silent };
                false
            }
            VendorState::Up => {
                let n = self.generic_failures.fetch_add(1, Ordering::Relaxed) + 1;
                if n >= self.down_after {
                    *st = VendorState::Backoff { try_at: Instant::now() + self.backoff(0), attempt: 0, silent: false };
                    true
                } else {
                    false
                }
            }
        }
    }

    /// A turn (or retry) succeeded. Flip Up and reset the blip counter. Returns
    /// `true` if this ended an outage (so the caller logs the recovery).
    fn note_success(&self) -> bool {
        let mut st = self.state.lock().unwrap();
        self.generic_failures.store(0, Ordering::Relaxed);
        let was_down = !matches!(*st, VendorState::Up);
        *st = VendorState::Up;
        was_down
    }
}

/// A short human phrase for when the balance refills ("大约 3 小时后", "约 20 分钟后"),
/// or a vague fallback when the reset time is unknown.
pub(crate) fn humanize_until_reset(resets_at: &str) -> String {
    let Ok(reset) = chrono::DateTime::parse_from_rfc3339(resets_at.trim()) else {
        return "过一会儿".to_string();
    };
    let mins = (reset.with_timezone(&chrono::Utc) - chrono::Utc::now()).num_minutes();
    if mins <= 1 {
        "很快就".to_string()
    } else if mins < 60 {
        format!("约 {mins} 分钟后")
    } else {
        let hours = (mins as f64 / 60.0).round() as i64;
        format!("大约 {hours} 小时后")
    }
}

#[cfg(test)]
mod vendor_tests {
    use super::*;

    fn fresh() -> Vendor {
        Vendor::new(2, Duration::from_secs(30))
    }

    #[test]
    fn starts_up() {
        assert!(!fresh().is_down());
    }

    #[test]
    fn generic_outage_absorbs_a_blip_then_informs_once() {
        let v = fresh();
        assert!(!v.note_unreachable(), "first generic failure is absorbed (down_after = 2)");
        assert!(!v.is_down(), "still reachable after one blip");
        assert!(v.note_unreachable(), "the second flips to an informed backoff, announced once");
        assert!(v.is_down());
        assert!(matches!(v.scene_gate(), SceneGate::Retry { .. }));
        assert!(!v.note_unreachable(), "a failed retry grows the backoff without re-announcing");
    }

    #[test]
    fn success_resets_the_blip_counter() {
        let v = fresh();
        v.note_unreachable(); // one blip, still up
        v.note_success(); // resets the counter
        assert!(!v.note_unreachable(), "one blip after a reset must not flip");
        assert!(!v.is_down());
    }

    #[test]
    fn threshold_one_flips_on_first_generic_failure() {
        let v = Vendor::new(1, Duration::from_secs(30));
        assert!(v.note_unreachable());
        assert!(v.is_down());
    }

    #[test]
    fn backoff_grows_and_caps_at_one_hour() {
        let v = fresh(); // base 30s
        assert_eq!(v.backoff(0), Duration::from_secs(30));
        assert_eq!(v.backoff(1), Duration::from_secs(60));
        assert_eq!(v.backoff(2), Duration::from_secs(120));
        assert_eq!(v.backoff(100), BACKOFF_CAP, "never exceeds the 1h cap");
    }
}

/// The soonest a reflection may fire for a scene, or `None` when reflection is
/// disabled (`base` is `None`). One adaptive clock, anchored on the **last
/// reflection** (or `loop_started` before the first) so a never-idle scene still
/// fires every `base`:
/// - **fresh input** since the anchor (`last_activity > anchor`) → fire `base`
///   after the anchor — the active ~1/`base` cadence;
/// - **caught up and quiet** (`last_activity <= anchor`) → fire `backoff_gap`
///   after the anchor, where `backoff_gap` has been doubling toward the cap.
///
/// `backoff_gap` is the loop's running idle gap (reset to `base` whenever a pass
/// runs with fresh input, doubled toward the cap when one runs while quiet); this
/// function just reads it. Activity after a long idle re-anchors on the old
/// reflection, so the next pass is due immediately — fine, it's a detached session
/// and an under-`MIN_REFLECT_SIGNALS` frontier no-ops cheaply.
fn next_reflection_at(
    loop_started: Instant,
    last_activity: Instant,
    last_reflection: Option<Instant>,
    base: Option<Duration>,
    backoff_gap: Duration,
) -> Option<Instant> {
    let base = base?;
    let anchor = last_reflection.unwrap_or(loop_started);
    let gap = if last_activity > anchor { base } else { backoff_gap };
    Some(anchor + gap)
}

#[cfg(test)]
mod reflection_schedule_tests {
    use super::*;

    fn secs(n: u64) -> Duration {
        Duration::from_secs(n)
    }

    #[test]
    fn fresh_input_fires_at_base_after_the_anchor() {
        let t0 = Instant::now();
        // Never reflected; a turn landed at t0+30s. Anchor is loop_start (t0), and
        // fresh input since then → fire base (60s) after the anchor.
        let at = next_reflection_at(t0, t0 + secs(30), None, Some(secs(60)), secs(60));
        assert_eq!(at, Some(t0 + secs(60)));
    }

    #[test]
    fn busy_scene_fires_base_after_the_last_reflection() {
        let t0 = Instant::now();
        // Reflected at t0+60s; a later turn keeps activity ahead of the anchor, so
        // the next pass is base after the *reflection*, not pushed out by activity.
        let last_reflection = t0 + secs(60);
        let at = next_reflection_at(t0, t0 + secs(90), Some(last_reflection), Some(secs(60)), secs(60));
        assert_eq!(at, Some(last_reflection + secs(60)));
    }

    #[test]
    fn quiet_scene_uses_the_backed_off_gap() {
        let t0 = Instant::now();
        // Reflected at t0+60s, nothing since (activity at t0 < anchor) → fire the
        // backoff gap (already doubled to 240s) after the anchor.
        let last_reflection = t0 + secs(60);
        let at = next_reflection_at(t0, t0, Some(last_reflection), Some(secs(60)), secs(240));
        assert_eq!(at, Some(last_reflection + secs(240)));
    }

    #[test]
    fn new_input_after_long_idle_is_due_immediately() {
        let t0 = Instant::now();
        // Long idle: anchor is an hour-old reflection, gap backed off to 8h. A turn
        // just landed → fresh input → due `base` after the *old* anchor, i.e. in the
        // past, so the loop fires it on the next tick.
        let last_reflection = t0;
        let at = next_reflection_at(t0, t0 + secs(3600), Some(last_reflection), Some(secs(60)), secs(8 * 3600));
        assert_eq!(at, Some(last_reflection + secs(60)));
        assert!(at.unwrap() < t0 + secs(3600));
    }

    #[test]
    fn disabled_when_base_is_off() {
        let t0 = Instant::now();
        assert_eq!(next_reflection_at(t0, t0, None, None, secs(60)), None);
    }
}

/// How far back a scene's raw memory may date and still count as "recently active"
/// for the consolidated reflection pass. (Re-warming at startup uses the tighter
/// [`REWARM_MAX_IDLE`] gate instead — see [`scenes_to_rewarm`].)
const REWARM_WINDOW: Duration = Duration::from_secs(7 * 24 * 3600);

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
    /// A host pulse firing — the recurring moment of self-attention. Carries
    /// bare situational facts; what to do with such a moment is core.md's job.
    Pulse { note: String },
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
    /// The bootstrap seed opening every scene's reactor-session system prompt: a
    /// short personality plus a manifest pointing the mind at `core.md`/`speaking.md`
    /// to Read (see [`load_soul`]). Built once at startup, shared read-only across
    /// scenes; the heartbeat re-seeds replacement sessions with it too, so a
    /// hot-swapped mind boots the same way.
    soul: String,
    /// The reactor's single outbound seam: every channel signal it produces —
    /// text, synthesized speech, views — goes out here in transport-free form
    /// (see [`outbound`]). A transport adapter binds these to a wire. The reactor
    /// has no knowledge of HTTP, `Content-Type`, or response framing.
    out: mpsc::Sender<OutboundSignal>,
    /// Structured visibility into the session lifecycle. Turn, session, swap,
    /// worker and alarm events feed it; the HTTP front serves it.
    observatory: Observatory,
    /// Compiles agent-authored `[[view]]` source into an ESM module the browser
    /// imports. Invoked just-in-time when a view segment is released, so the
    /// compiled module URL is what rides the /view channel.
    view_compiler: crate::mind::views::ViewCompiler,
    /// Scene→tool-sink table the `/mcp` server routes tool calls through. Each
    /// scene loop registers its sink here as it stands up; shared (cloneable)
    /// with the HTTP front. See [`tools`].
    tools: ToolRegistry,
    /// Scene→barge-in state. The STT relay reports recognized speech here; the
    /// sequencer stamps each turn's voice span; `run_turn` drains the inferred
    /// "what went unheard" note into the next prompt. See [`interrupts`].
    interrupts: InterruptRegistry,
    /// Shared, process-wide LLM-vendor reachability + recovery policy. Read by every
    /// scene loop (via [`Vendor::scene_gate`]) to decide whether and when to drive a
    /// turn; written by `run_turn`'s terminal-failure / success paths. See [`Vendor`].
    vendor: Arc<Vendor>,
    /// Scene→live-subscriber counts, shared with the HTTP front. Rendered into
    /// each turn as one human-model presence sentence, so the mind knows which
    /// channels actually reach the person right now.
    presence: crate::body::presence::Presence,
    /// The live per-scene appearance state, shared (a cloneable handle) with the
    /// HTTP front's view bus. Read into each turn as `## On screen now` so the agent
    /// can see what it has shown — the screen is its own presentation surface, and
    /// without this it dismisses/re-shows views by guessing ids from the transcript.
    /// Read-only here: views are still *emitted* via `show_view` → the binder →
    /// `ViewBus::apply`; this is purely the reactor observing that authoritative state.
    views: crate::foundation::server::ViewBus,
    /// Absolute path to the agent's view workshop (`<data_dir>/views`).
    /// Handed to every worker session as its `cwd`, so a build sub-agent works in a
    /// real project dir — `ls`-ing existing projects, writing source — like a human
    /// in their repo. Absolutized at startup (the child may run with a different cwd).
    views_dir: PathBuf,
    /// Monotonic cognition-turn counter. Each turn claims the next id;
    /// it tags audio spans and the channel logs so a reply is traceable end to
    /// end. (The client no longer needs it — turns are internal to the mind.)
    turn_seq: AtomicU64,
    scenes: Mutex<HashMap<Scene, SceneHandle>>,
    /// Wall-monotonic time of the most recent inbound human signal across **all**
    /// scenes — the global "fresh input" signal the single consolidated reflection
    /// clock reads to decide base-vs-backoff cadence (see [`consolidated_reflection_loop`]).
    /// Written in [`Reactor::deliver_to_scene`]; read each reflection tick.
    last_signal_at: std::sync::Mutex<Instant>,
    /// Wakes the consolidated reflection loop when fresh input lands, so a scene
    /// that goes active after a long quiet doesn't wait out the backed-off gap
    /// before its first pass — the loop re-derives its deadline on every notify.
    reflect_wake: tokio::sync::Notify,
    /// Process-wide shutdown signal, triggered by [`crate::run_with_shutdown`] the
    /// moment a SIGINT/SIGTERM or the tray's Quit is observed. Read by every scene
    /// loop, the reflection loop, and the drive retry path so that, once shutdown
    /// begins, an idle loop winds down promptly and a failed prompt does **not**
    /// restart an ACP session — the children just received the same signal, and a
    /// respawn here would race the subprocess reap and could orphan a child.
    shutdown: Shutdown,
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
    view_compiler: crate::mind::views::ViewCompiler,
    tools: ToolRegistry,
    interrupts: InterruptRegistry,
    presence: crate::body::presence::Presence,
    views: crate::foundation::server::ViewBus,
    views_dir: PathBuf,
    shutdown: Shutdown,
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
            interrupts,
            presence,
            views,
            views_dir,
            turn_seq: AtomicU64::new(0),
            scenes: Mutex::new(HashMap::new()),
            vendor: Arc::new(Vendor::new(vendor_down_after(), backoff_base())),
            last_signal_at: std::sync::Mutex::new(Instant::now()),
            reflect_wake: tokio::sync::Notify::new(),
            shutdown,
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

    // Re-warm scenes with a genuinely fresh, still-live conversation, so their loop
    // (and pulse) is up without waiting for a client to reconnect. Deliberately
    // conservative — see [`scenes_to_rewarm`]: each warm spawns a subprocess and an
    // LLM call, so warming a crowd at boot hurts startup UX and competes for our own
    // LLM rate limit right when the user wants to interact. Boot is not a special
    // case: this merely stands the loops up, and each one's first pulse carries the
    // "host process started Xm ago" fact like any other. Standing/scheduled work
    // (cron, serving) does not depend on this — it lives on the heartbeat, so a scene
    // going cold never drops a duty.
    let rewarm_reactor = reactor.clone();
    tokio::spawn(async move {
        for scene in scenes_to_rewarm(rewarm_reactor.inner.memory.data_dir()) {
            tracing::info!(scene = %scene, "re-warming recently-active scene");
            rewarm_reactor.ensure_scene(scene).await;
        }
    });

    // Consolidated reflection ("sleep"): one pass over every recently-active scene
    // on a single global clock, replacing the old per-scene timers. A single mind
    // settles the whole day across contexts at once — so it can link across scenes
    // and one writer (not N racing) touches the shared facet/people stores.
    let reflect_reactor = reactor.clone();
    tokio::spawn(async move {
        consolidated_reflection_loop(reflect_reactor).await;
    });

    reactor
}

/// The single consolidated reflection loop: one "sleep" pass over all
/// recently-active scenes, on one adaptive clock, never overlapping itself.
///
/// Anchored on the **last completed pass** (or loop start before the first), it
/// fires `base` after the anchor while any scene saw fresh input since (the active
/// cadence), else on a `backoff_gap` doubling toward `reflect_max` while the whole
/// system is quiet — the same rule the old per-scene loops used, now global (see
/// [`next_reflection_at`]). A fresh signal arriving mid-gap pokes [`reflect_wake`]
/// so the loop re-derives its deadline immediately rather than waiting out a long
/// backoff. Each tick consolidates only scenes with enough on their frontier; a
/// tick with nothing ready is a cheap no-op. Returns (the task ends) only when
/// reflection is disabled outright (`reflect=off` or `reflect_every=0`).
async fn consolidated_reflection_loop(reactor: Reactor) {
    let reflect_base = reflect_interval();
    let reflect_max = reflect_max_interval();
    if reflect_base.is_none() {
        tracing::info!("consolidated reflection disabled");
        return;
    }
    let loop_started = Instant::now();
    let mut last_reflection: Option<Instant> = None;
    let mut backoff_gap = reflect_base.unwrap_or(DEFAULT_REFLECT_EVERY);

    loop {
        let last_activity = *reactor.inner.last_signal_at.lock().unwrap();
        let Some(at) =
            next_reflection_at(loop_started, last_activity, last_reflection, reflect_base, backoff_gap)
        else {
            return;
        };
        let now = Instant::now();
        if at > now {
            // Sleep until due, but wake early if fresh input lands — then re-loop to
            // recompute the deadline (which only actually fires once it's past).
            // Shutdown ends the loop rather than starting a doomed "sleep" pass.
            tokio::select! {
                _ = tokio::time::sleep(at.saturating_duration_since(now)) => {}
                _ = reactor.inner.reflect_wake.notified() => continue,
                _ = reactor.inner.shutdown.cancelled() => {
                    tracing::info!("shutdown requested; ending consolidated reflection loop");
                    return;
                }
            }
        }

        // Shutdown may have arrived without the sleep above (deadline already past):
        // don't open a reflection subprocess into a dying process group.
        if reactor.inner.shutdown.is_triggered() {
            tracing::info!("shutdown requested; ending consolidated reflection loop");
            return;
        }

        // Due. Adapt the backoff against the *old* anchor before re-anchoring: fresh
        // input since the last pass snaps the gap back to base; a quiet pass doubles
        // it toward the cap. Re-anchor on `now` whether or not anything consolidates,
        // so a no-op tick can't hot-spin the clock.
        let now = Instant::now();
        let last_activity = *reactor.inner.last_signal_at.lock().unwrap();
        let anchor = last_reflection.unwrap_or(loop_started);
        backoff_gap = if last_activity > anchor {
            reflect_base.unwrap_or(DEFAULT_REFLECT_EVERY)
        } else {
            backoff_gap.checked_mul(2).unwrap_or(reflect_max).min(reflect_max)
        };
        last_reflection = Some(now);

        // The scenes to consider — the same source that decides which loops exist, so
        // we consolidate exactly the scenes that were reflecting under the old design.
        let scenes = recent_scenes(reactor.inner.memory.data_dir(), REWARM_WINDOW);
        heartbeat::consolidate(&reactor, &scenes).await;
    }
}

/// Scenes whose raw memory saw activity within `window`, each paired with the
/// newest modification time seen across its day folders (its last signal). The
/// directories are under `<data_dir>/memory/raw/`; errors read as "no scenes" —
/// re-warm is best-effort.
///
/// The mtime already ignores idle pulses: a pulse that concludes "nothing to do"
/// emits nothing and so writes nothing under `raw/`, so the newest mtime marks the
/// last *real* signal (an inbound utterance or an emitted reply), never a bare
/// self-attention tick. That is what lets the re-warm gate treat mtime as "last
/// input" without a separate journal scan.
fn scenes_with_activity(
    data_dir: &std::path::Path,
    window: Duration,
) -> Vec<(Scene, std::time::SystemTime)> {
    let raw = data_dir.join("memory").join("raw");
    let Some(cutoff) = std::time::SystemTime::now().checked_sub(window) else {
        return Vec::new();
    };
    let Ok(entries) = std::fs::read_dir(&raw) else {
        return Vec::new();
    };
    let mut scenes = Vec::new();
    for entry in entries.flatten() {
        let path = entry.path();
        if !path.is_dir() {
            continue;
        }
        // Newest day-folder mtime under this scene — the time of its last signal.
        let newest = std::fs::read_dir(&path).ok().and_then(|days| {
            days.flatten()
                .filter_map(|d| d.metadata().and_then(|m| m.modified()).ok())
                .max()
        });
        if let Some(newest) = newest
            && newest >= cutoff
            && let Some(name) = path.file_name().and_then(|n| n.to_str())
        {
            scenes.push((Scene(name.to_owned()), newest));
        }
    }
    scenes
}

/// Scenes whose raw memory saw activity within `window`. Thin projection of
/// [`scenes_with_activity`] for callers that only need the ids (the consolidated
/// reflection pass).
fn recent_scenes(data_dir: &std::path::Path, window: Duration) -> Vec<Scene> {
    scenes_with_activity(data_dir, window)
        .into_iter()
        .map(|(scene, _)| scene)
        .collect()
}

/// A scene whose last input is older than this is not re-warmed at startup: it has
/// gone quiet, and standing work no longer lives in a per-scene loop (cron/serving
/// run on the heartbeat), so there is nothing to keep alive by warming it.
const REWARM_MAX_IDLE: Duration = Duration::from_secs(24 * 3600);

/// Where the re-warm gate persists its per-scene bookkeeping (see
/// [`scenes_to_rewarm`]). Sits outside `raw/` so writing it never perturbs the
/// activity mtimes [`scenes_with_activity`] reads.
fn rewarm_state_path(data_dir: &std::path::Path) -> PathBuf {
    data_dir.join("memory").join("rewarm.json")
}

/// Scene id → the unix-seconds mtime of its newest raw signal at the moment we last
/// re-warmed it. A missing/corrupt file reads as empty: at worst one extra re-warm.
fn load_rewarm_state(data_dir: &std::path::Path) -> HashMap<String, u64> {
    std::fs::read(rewarm_state_path(data_dir))
        .ok()
        .and_then(|bytes| serde_json::from_slice(&bytes).ok())
        .unwrap_or_default()
}

fn save_rewarm_state(data_dir: &std::path::Path, state: &HashMap<String, u64>) {
    if let Ok(bytes) = serde_json::to_vec_pretty(state) {
        // Best-effort: a lost write just costs at most one extra re-warm next boot.
        let _ = std::fs::write(rewarm_state_path(data_dir), bytes);
    }
}

/// Which recently-active scenes to re-warm at startup — deliberately conservative.
///
/// Warming a scene is expensive: it spawns an ACP subprocess and its first pulse is
/// an LLM call. Warming many scenes at once slows startup and floods our own LLM
/// rate limit *exactly* when the user is trying to interact — so we warm only the
/// scenes that plausibly still have a live conversation, and never re-warm the same
/// quiet scene twice. A scene is re-warmed only when BOTH hold:
///   1. its last input is newer than [`REWARM_MAX_IDLE`] (a day quiet → stay cold),
///      enforced by the `scenes_with_activity` window; and
///   2. we have not already re-warmed it for that same, unchanged input — so
///      restarting the host repeatedly within a day doesn't re-warm a quiet scene
///      each time.
///
/// "Input" here is raw-memory activity, which already excludes pulses (an idle
/// pulse writes nothing — see [`scenes_with_activity`]). Standing/scheduled work
/// (cron, serving) does NOT rely on re-warming: it belongs to the global heartbeat
/// session, not a per-scene loop, so letting an idle scene go cold never drops a
/// duty.
fn scenes_to_rewarm(data_dir: &std::path::Path) -> Vec<Scene> {
    let prior = load_rewarm_state(data_dir);
    let mut warm = Vec::new();
    // Only scenes carried forward here (all within REWARM_MAX_IDLE) stay in the
    // map; scenes that have since gone quiet fall out, keeping it bounded.
    let mut next: HashMap<String, u64> = HashMap::new();
    for (scene, mtime) in scenes_with_activity(data_dir, REWARM_MAX_IDLE) {
        let epoch = mtime
            .duration_since(std::time::UNIX_EPOCH)
            .map(|d| d.as_secs())
            .unwrap_or(0);
        next.insert(scene.0.clone(), epoch);
        if prior.get(&scene.0) == Some(&epoch) {
            // Condition 2: already re-warmed for this exact input, nothing new
            // since — leave it cold and let a fresh signal wake it on demand.
            continue;
        }
        warm.push(scene);
    }
    save_rewarm_state(data_dir, &next);
    warm
}

impl Reactor {
    async fn deliver_to_scene(&self, scene: Scene, signal: Signal) {
        // Mark global activity and poke the consolidated reflection clock, so a scene
        // going active after a long quiet gets its first pass without waiting out the
        // backed-off gap.
        *self.inner.last_signal_at.lock().unwrap() = Instant::now();
        self.inner.reflect_wake.notify_one();

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
    /// Process shutdown began while this loop was idle — stop waiting and exit.
    Shutdown,
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
        SceneControl::Delegate { task, worker } => {
            let outcome = match worker {
                // An explicit `delegate` is a plain task worker, never cognition —
                // cognition is driven only through `cognize`, which tags its reports.
                Some(id) => workers.follow_up(reactor, id, task, false).await,
                None => workers.spawn(reactor, task).await,
            };
            if let Err(err) = outcome {
                tracing::warn!(scene = %scene, error = %err, "failed to delegate working session");
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
        SceneControl::WorkerSurface { id, message } => {
            reactor
                .inner
                .observatory
                .record(scene, EventKind::WorkerSurfaced { id, message: message.clone() })
                .await;
            // Return it as a turn-driving signal (like a worker question), so the loop
            // breaks 'wait and runs a turn — the voice gets to say it even with no human
            // input. This is the mechanism for cognition-initiated speech.
            Some(LoopInput::Worker(workers.surface_report(id, message)))
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
    // Retained for the observatory's budget readout; the reactor turn no longer
    // feeds it, so the hot-swap it gated never fires (the reactor re-opens cold on
    // failure instead). Left in place until the hot-swap path is fully retired.
    let mut budget = heartbeat::ContextBudget::new();
    // The scene's live working sessions. Heavy/tool-using work the reactor
    // delegates runs here; workers post progress and results back through
    // `worker_inbound` into this same loop.
    let mut workers = workers::WorkerRegistry::new(scene.clone(), worker_inbound);
    // Self-alarms the mind has scheduled. They give the loop a second reason to
    // wake — time passing — on top of an incoming signal; see the `select!` below.
    let mut alarms = Alarms::new();

    tracing::info!(scene = %scene, "reactor per-scene loop up");

    // No warm-up: the reactor session opens lazily on its first turn (a subprocess
    // spawn + system-prompt prime would only stall that first turn behind it). The
    // journal snapshot is delivered by that first turn's fresh-session branch.

    // Pulse bookkeeping: the host's recurring self-attention timer. `last_activity`
    // resets on every turn, so pulses only fire into genuine quiet; the first pulse
    // after the loop stands up also carries how long ago the host process started,
    // which is all "wake on boot" amounts to.
    let pulse_every = pulse_interval();
    let loop_started = Instant::now();
    let mut last_activity = Instant::now();
    let mut pulsed_once = false;

    // Pending turn-driving items, hoisted out of the main loop so the batch
    // survives across iterations while the vendor is down — a failed retry must not
    // drop the mail it was attempting to deliver. Cleared on a successful turn (the
    // mail went out) and on a reachable-but-failed blip (the apology was emitted);
    // held while down.
    let mut batch: Vec<LoopInput> = Vec::new();

    loop {
        // Wait for a turn-driving reason: a new signal, a fired alarm, a due host
        // pulse, a worker question, or — while the vendor is down — a backoff retry
        // (429/generic). Tool control commands (delegate/alarm) are pure side-effects
        // applied without a turn; only a worker `ask` becomes a turn-driving item.
        'wait: loop {
            let gate = reactor.inner.vendor.scene_gate();
            // Mail already sitting in `batch` (e.g. held while the vendor was down)
            // needs no fresh signal to act on — drive it now while reachable. While
            // down, fall through to the timer logic.
            if !batch.is_empty() && matches!(gate, SceneGate::Go) {
                break 'wait;
            }
            let down = !matches!(gate, SceneGate::Go);
            // While down, suppress pulses — they call the model and would just fail.
            let pulse_at = if down { None } else { pulse_every.map(|d| last_activity + d) };
            // While down, the recovery timer: the backoff retry deadline (429/generic).
            // Up → no such timer.
            let recover_at = match gate {
                SceneGate::Go => None,
                SceneGate::Retry { at } => Some(at),
            };
            let deadline = [alarms.next_deadline(), pulse_at, recover_at]
                .into_iter()
                .flatten()
                .min();
            let woke = match deadline {
                Some(deadline) => tokio::select! {
                    recvd = inbound.recv() => Woke::Inbound(recvd),
                    ctl = control.recv() => Woke::Control(ctl),
                    _ = sleep_until(deadline) => Woke::Timer,
                    _ = reactor.inner.shutdown.cancelled() => Woke::Shutdown,
                },
                None => tokio::select! {
                    recvd = inbound.recv() => Woke::Inbound(recvd),
                    ctl = control.recv() => Woke::Control(ctl),
                    _ = reactor.inner.shutdown.cancelled() => Woke::Shutdown,
                },
            };
            match woke {
                Woke::Inbound(Some(s)) => {
                    batch.push(s);
                    // While Down: collect mail without driving a turn. The
                    // probe cadence will attempt catch-up once the vendor
                    // recovers.
                    if !down {
                        break 'wait;
                    }
                }
                Woke::Inbound(None) => {
                    tracing::info!(scene = %scene, "per-scene inbound closed; exiting loop");
                    return;
                }
                Woke::Shutdown => {
                    tracing::info!(scene = %scene, "shutdown requested; exiting per-scene loop");
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
                        if !down {
                            break 'wait;
                        }
                    }
                    // A delegate/alarm side-effect was applied; keep waiting for a
                    // turn-driving reason rather than running an empty turn.
                }
                Woke::Timer => {
                    let now = Instant::now();
                    if down {
                        // Alarms still fire and queue while down — the mind asked to
                        // be woken, and the note isn't lost — but they don't alone
                        // drive a turn; a backoff retry does.
                        for fired in alarms.take_due(now) {
                            reactor
                                .inner
                                .observatory
                                .record(&scene, EventKind::AlarmFired { note: fired.note.clone() })
                                .await;
                            batch.push(LoopInput::Alarm(fired));
                        }
                        // Only a transient backoff drives a model retry, and only with
                        // mail to deliver. Out of energy holds instead — the shared
                        // poller flips us back Up and the top of 'wait then drains the
                        // mail without a doomed model call.
                        if let SceneGate::Retry { at } = gate
                            && at <= now
                            && !batch.is_empty()
                        {
                            tracing::info!(scene = %scene, mail = batch.len(), "backoff retry firing");
                            break 'wait;
                        }
                        continue 'wait;
                    }
                    for fired in alarms.take_due(now) {
                        reactor
                            .inner
                            .observatory
                            .record(&scene, EventKind::AlarmFired { note: fired.note.clone() })
                            .await;
                        batch.push(LoopInput::Alarm(fired));
                    }
                    if let Some(at) = pulse_at
                        && at <= now
                    {
                        let idle_m = (now - last_activity).as_secs() / 60;
                        let note = if pulsed_once {
                            format!("nothing new here for {idle_m}m")
                        } else {
                            let up_m = (now - loop_started).as_secs() / 60;
                            format!(
                                "nothing new here for {idle_m}m — you've just come back up (host process started {up_m}m ago)"
                            )
                        };
                        pulsed_once = true;
                        // Reset so a swallowed pulse doesn't re-fire in a tight loop.
                        last_activity = now;
                        tracing::info!(scene = %scene, "pulse fired");
                        batch.push(LoopInput::Pulse { note });
                    }
                    if !batch.is_empty() {
                        break 'wait;
                    }
                }
            }
        }

        // A timer can resolve with nothing actually due; don't run an empty turn.
        // (While Down, the probe only breaks 'wait with non-empty mail, so this
        // guard is for the Up path's pulse/alarm timers.)
        if batch.is_empty() {
            continue;
        }

        let was_down = reactor.inner.vendor.is_down();

        // Commit-after-quiet: wait for things to settle before replying. Skipped
        // while down — a backoff retry should attempt catch-up ASAP rather than wait
        // for more mail to settle (the retry cadence already coalesces arrivals).
        if !was_down {
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
        }

        // Forget any workers that have finished, so the registry doesn't grow.
        workers.reap();

        match run_reactor_turn(&reactor, &scene, &batch, &mut workers, &mut reactor_session, &beats).await {
            Ok(()) => {
                // The turn delivered the mail; clear the backlog. (If this was a
                // retry, the turn already flipped the vendor Up via note_success.)
                batch.clear();
                // A reply landed — stop the presence owed-reply clock (no-op if
                // nothing was owed, e.g. a pulse turn).
                reactor.inner.presence.note_delivered(&scene);
                // Between turns: if the live session has grown past budget, hot-swap
                // it now. The human is consuming the reply just delivered, so the
                // summarize-and-reopen happens in that natural gap — invisible, never
                // a cold restart. A swap failure leaves the warm session in place.
                // (Reflection is no longer kicked off here — it runs on its own
                // periodic clock in the wait loop above, decoupled from compaction.)
                if budget.should_swap() {
                    if let Some(current) = reactor_session.clone() {
                        match timeout(SWAP_TIMEOUT, heartbeat::swap(&reactor, &scene, &current)).await {
                            Ok(Ok(fresh)) => {
                                reactor_session = Some(fresh);
                                budget.reset();
                                tracing::info!(scene = %scene, "reactor session hot-swapped");
                            }
                            Ok(Err(err)) => {
                                tracing::warn!(scene = %scene, error = %err, "hot-swap failed; keeping warm session");
                            }
                            Err(_) => {
                                // The live session ignored the summarize prompt for the
                                // whole window — treat it as wedged and discard it, the
                                // same as a failed turn; the next turn cold-opens a fresh
                                // session from the journal snapshot.
                                tracing::warn!(scene = %scene, "hot-swap timed out; discarding unresponsive session");
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
                // Dropping the session means the next turn cold-opens and its
                // fresh-session branch re-ingests the journal snapshot.
                budget.reset();
                reactor.inner.observatory.set_budget(&scene, 0).await;
                // Key on the vendor state the turn just wrote, not the pre-turn one:
                // a turn that flipped the vendor down holds the mail — a backoff drives
                // it at the next retry deadline. Only a still-reachable blip (already
                // apologized inside run_turn) drops it.
                if reactor.inner.vendor.is_down() {
                    tracing::info!(scene = %scene, mail = batch.len(), "vendor down; holding mail for recovery");
                } else {
                    batch.clear();
                }
            }
        }

        // Any completed turn is activity: the pulse clock restarts, so pulses
        // only ever fire into genuine quiet.
        last_activity = Instant::now();

        // Coalesce mid-turn arrivals. Utterances that queued while this turn ran
        // (a generation is now seconds, not ~1s) are siblings of the thread we just
        // answered, not fresh threads — pull them all into one batch so they drive a
        // SINGLE next turn (the commit-after-quiet settle still applies on top),
        // instead of one redundant turn each. Without this, each nudge that landed
        // mid-turn ("好了吗?" → "准备好了吗?") pops alone on re-entry and re-answers.
        // Up only: while down, mail is held deliberately and the backoff path owns
        // catch-up, so leave the queue for it. `try_recv` never surfaces pulses or
        // alarms (those are generated inside `'wait`, not sent over `inbound`).
        if !reactor.inner.vendor.is_down() {
            while let Ok(extra) = inbound.try_recv() {
                batch.push(extra);
            }
        }
    }
}

/// Render just the human requests in a batch (skipping worker reports, alarms, and
/// pulses) — the text handed to cognition as the turn's task. Skipping reports is
/// what keeps cognition from re-ingesting its own prior output (a feedback loop).
fn render_human_from_batch(batch: &[LoopInput]) -> String {
    use crate::mind::memory::snapshot::{Speaker, transcript_line};
    use std::fmt::Write as _;
    let mut s = String::new();
    for input in batch {
        if let LoopInput::Human(sig) = input {
            let chan = sig.channel.with_stream(sig.stream.as_deref());
            let _ = writeln!(s, "{}", transcript_line(Speaker::Them, &chan, &sig.body));
        }
    }
    s
}


/// A reactor turn: the single fast conversational voice. An ACP session
/// ([`SessionRole::Reactor`]) on the small model, carrying `speaking.md` as its system
/// prompt and a minimal `show_view`-only `/mcp` surface. A turn is a single quick
/// generation: it speaks via the session's plain message text — [`SessionRun::wait`]
/// concatenates every `agent_message_chunk` into the reply — and may call `show_view`
/// to put a view a worker already built on screen; both feed the sequencer. The speed
/// comes from the small model + a single generation, not from bypassing the adapter.
///
/// Cognition — the agentic thinker/worker — runs in parallel: the turn's human request
/// is handed to a persistent cognition worker ([`workers::WorkerRegistry::cognize`]),
/// which works off the floor and reports back as an ordinary `LoopInput::Worker` the
/// reactor voices on a later turn. So the reactor stays the single fast voice.
///
/// v1 keeps it simple — no mid-turn reorganization. A turn is one fast generation, so a
/// human speaking during it just queues and the serial loop folds it into the next turn.
async fn run_reactor_turn(
    reactor: &Reactor,
    scene: &Scene,
    batch: &[LoopInput],
    workers: &mut workers::WorkerRegistry,
    reactor_session: &mut Option<Arc<AcpSession>>,
    beats: &mpsc::Sender<sequencer::Beat>,
) -> anyhow::Result<()> {
    let turn_id = reactor.inner.turn_seq.fetch_add(1, Ordering::Relaxed);

    // Assemble the turn context: recent conversation (so the voice reconciles with what
    // was already said rather than repeating it), live worker status (so it can surface
    // cognition's progress), presence, any barge-in note, and the new signals.
    let worker_status = workers.render_status().await;
    let presence_note = format!("## Presence\n{}", reactor.inner.presence.render(scene));
    let interrupted = reactor
        .inner
        .interrupts
        .take_pending(scene)
        .await
        .map(|i| interrupts::render_interruption(&i))
        .unwrap_or_default();
    let new_signals = format!("## New signals\n{}", render_batch(batch));
    // What the agent has on screen right now — its own presentation surface. Read
    // fresh every turn (it's a current fact, not durable memory), so a view dismissed
    // last turn is gone from this list now: the agent can see what's up and dismiss by
    // real id instead of guessing from the transcript.
    let on_screen = render_on_screen(&reactor.inner.views.on_screen(scene).await);

    // Open (or reuse) the persistent reactor session. `speaking.md` is prepended
    // to its first prompt; the session then remembers prior turns, so only a *fresh*
    // session is handed the durable working set and the memory snapshot — later turns
    // send just the delta, inheriting both from the session's own memory of the open.
    let (session, fresh) = match reactor_session {
        Some(s) => (s.clone(), false),
        None => {
            let opened = open_reactor_session(reactor, scene).await?;
            *reactor_session = Some(opened.clone());
            (opened, true)
        }
    };

    let context = if fresh {
        // The reactor is tools-off, so its durable memory — identity, standing
        // commitments, and what's lately been on its mind — has to be handed to it
        // here rather than Read on its own. Prepended on the fresh turn so the voice is
        // grounded from its very first word; the session retains it thereafter.
        let snap = build_for_scene(&reactor.inner.memory, scene).await?;
        join_sections(&[
            &working_set(reactor.inner.memory.data_dir()).await,
            &snap.render_for_prompt(),
            &worker_status,
            &on_screen,
            &presence_note,
            &interrupted,
            &new_signals,
        ])
    } else {
        join_sections(&[&worker_status, &on_screen, &presence_note, &interrupted, &new_signals])
    };

    tracing::info!(scene = %scene, ctx_chars = context.chars().count(), "reactor: prompting session");
    let _ = beats.send(sequencer::Beat::TurnStart { turn: turn_id }).await;

    let spoke = match drive_voice(&session, scene, context).await {
        Ok(text) => {
            tracing::info!(scene = %scene, reply_chars = text.chars().count(), "reactor: replied");
            if !text.trim().is_empty() {
                let _ = beats.send(sequencer::Beat::Say(text)).await;
            }
            true
        }
        Err(err) => {
            tracing::warn!(scene = %scene, error = %err, "reactor turn failed");
            // Drop the possibly-wedged session so the next turn re-opens cold.
            *reactor_session = None;
            if reactor.inner.vendor.note_unreachable() {
                let _ = beats
                    .send(sequencer::Beat::Say(
                        "我暂时连不上模型，先攒着你的消息，等恢复了一起处理。".to_string(),
                    ))
                    .await;
            }
            false
        }
    };

    // Close the bracket and record what was spoken (for barge-in resolution).
    let (done_tx, done_rx) = oneshot::channel();
    let _ = beats.send(sequencer::Beat::TurnEnd { done: done_tx }).await;
    let reply = done_rx.await.unwrap_or_default();
    reactor.inner.interrupts.end_turn(scene, turn_id, &reply).await;

    if spoke {
        let _ = reactor.inner.vendor.note_success();
        // Hand the turn's human request to cognition — the agentic thinker — so it works
        // off the floor while the voice moves on; its report rides back as a WorkerReport
        // the reactor voices on a later turn. Spawned once per scene, then followed up.
        // Nothing to hand off on a pure report/pulse turn.
        let task = render_human_from_batch(batch);
        if !task.trim().is_empty() {
            if let Err(e) = workers.cognize(reactor, task).await {
                tracing::warn!(scene = %scene, error = %e, "cognition spawn/follow-up failed");
            }
        }
    }
    Ok(())
}

/// Open a fresh **reactor** session for `scene`, carrying `speaking.md` as its system
/// prompt (prepended to the first prompt). It speaks via plain message text and gets a
/// minimal `show_view`-only `/mcp` surface, so a turn is a single quick generation that
/// may also put one already-built view on screen.
async fn open_reactor_session(reactor: &Reactor, scene: &Scene) -> anyhow::Result<Arc<AcpSession>> {
    let session = Arc::new(
        reactor
            .inner
            .agent
            .session(
                scene,
                SessionRole::Reactor,
                None,
                SessionOpts {
                    system_prompt: Some(crate::identity::reactor_system_prompt()),
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

/// Prompt the reactor session and return its spoken text (every `agent_message_chunk`
/// concatenated). Tool calls — the reactor's only tool is `show_view` — are dispatched
/// server-side through hi-agent's `/mcp` (which emits the `Beat::Show`), so the drive
/// loop just keeps streaming speech past them, exactly like a worker's loop; `wait()`
/// then parks the session and surfaces any real prompt error (a gateway 402/429, a
/// transport reset) to the caller's classifier.
async fn drive_voice(session: &AcpSession, scene: &Scene, context: String) -> anyhow::Result<String> {
    let mut run = session.prompt(context).await?;
    let mut text = String::new();
    while let Some(update) = run.next_update().await {
        match update {
            SessionUpdate::Text(t) => text.push_str(&t),
            SessionUpdate::Thought(t) => {
                tracing::debug!(scene = %scene, chars = t.chars().count(), "reactor: model is thinking");
            }
            // `show_view` dispatches server-side via `/mcp`; the reactor keeps speaking.
            // Its surface is `show_view`-only and the dispatch guard blocks any other
            // expression tool, so there is nothing to intercept here.
            SessionUpdate::ToolCall(_) => {}
            SessionUpdate::Other(_) => {}
        }
    }
    let result = run.wait().await?;
    tracing::info!(
        scene = %scene,
        stop = ?result.stop_reason,
        reply_chars = text.chars().count(),
        "reactor: turn complete"
    );
    Ok(text)
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

/// Carry one release action to its wire carrier: speech to TTS, a view to
/// /view. Thought mirroring and the once-per-turn reply log are handled inline
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
        interleave::Emit::ShowView { id, op, source, geometry } => {
            emit_view(reactor, scene, id, op, source, geometry).await
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

/// Render the agent's own screen as a prompt section: the ids currently displayed,
/// z-order top-most last. Always emitted (unlike the empty-dropping sections) — when
/// the screen is clear the agent needs to *know* it's clear so it stops firing blind
/// dismisses at ids that are already gone. Kept to bare ids: the reactor shows/dismisses
/// by id, and the id is all it needs to target one.
fn render_on_screen(ids: &[String]) -> String {
    use std::fmt::Write as _;
    let mut s = String::from("## On screen now\n");
    if ids.is_empty() {
        s.push_str("(nothing is on screen — the room is clear)");
    } else {
        for id in ids {
            let _ = writeln!(s, "- {id}");
        }
        s.push_str("(these are the views currently up, top-most last; dismiss one by its id)");
    }
    s
}

fn render_batch(batch: &[LoopInput]) -> String {
    use std::fmt::Write as _;
    let mut s = String::new();
    for input in batch {
        match input {
            LoopInput::Human(sig) => {
                use crate::mind::memory::snapshot::{Speaker, transcript_line};
                let chan = sig.channel.with_stream(sig.stream.as_deref());
                let _ = writeln!(s, "{}", transcript_line(Speaker::Them, &chan, &sig.body));
            }
            LoopInput::Worker(report) => {
                let _ = writeln!(s, "{}", workers::render_report(report));
            }
            LoopInput::Alarm(a) => {
                let _ = writeln!(s, "(alarm) \"{}\"", a.note);
            }
            LoopInput::Pulse { note } => {
                let _ = writeln!(s, "(pulse) {note}");
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
async fn emit_view(
    reactor: &Reactor,
    scene: &Scene,
    id: String,
    op: ViewOp,
    source: String,
    geometry: Option<Geometry>,
) {
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
            envelope: ViewEnvelope { id, op, module_url, geometry },
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
