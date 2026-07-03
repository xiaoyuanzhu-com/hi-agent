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
use std::sync::atomic::{AtomicBool, AtomicU32, AtomicU64, Ordering};
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
use crate::mind::memory::{Memory, build_for_scene};
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

/// Default recovery-probe cadence while in vendor-down mode. The probe only
/// fires when a scene has pending mail, so an idle outage costs nothing.
const DEFAULT_VENDOR_PROBE: Duration = Duration::from_secs(30);
/// Default consecutive terminal failures before flipping to vendor-down. Each
/// terminal failure is already 3 failed model calls, so 2 = 6 failures across
/// two turns — absorbs blips, catches real outages.
const DEFAULT_VENDOR_DOWN_AFTER: u32 = 2;

/// The recovery-probe interval. `vendor_probe` in alarm-delay grammar;
/// `off`/`0`/unset/unparseable → default. Probes only fire when a scene has
/// pending mail, so an idle outage costs no vendor calls — there's no need to
/// disable them.
fn vendor_probe_interval() -> Duration {
    duration_tunable(config::tunables::get(config::KEY_VENDOR_PROBE), DEFAULT_VENDOR_PROBE)
        .unwrap_or(DEFAULT_VENDOR_PROBE)
}

/// The consecutive terminal-failure count that flips the reactor into
/// vendor-down mode. `vendor_down_after`; `0`/unparseable → default.
fn vendor_down_after() -> u32 {
    config::tunables::get(config::KEY_VENDOR_DOWN_AFTER)
        .and_then(|v| v.parse::<u32>().ok())
        .filter(|n| *n > 0)
        .unwrap_or(DEFAULT_VENDOR_DOWN_AFTER)
}

/// Shared, process-wide view of whether the upstream LLM vendor is reachable.
/// All scene loops read it; `run_turn`'s terminal-failure path writes it. The
/// vendor is a shared resource, so one scene detecting an outage flips the flag
/// for every scene — the others stop burning retries too. Per-scene queues
/// drain independently on recovery.
///
/// `record_failure` returns `true` only on the Up→Down transition (so the caller
/// emits the canned "holding your mail" line exactly once); `record_success`
/// returns `true` only on Down→Up.
struct VendorHealth {
    down: AtomicBool,
    consecutive_failures: AtomicU32,
    down_after: u32,
}

impl VendorHealth {
    fn new(down_after: u32) -> Self {
        Self {
            down: AtomicBool::new(false),
            consecutive_failures: AtomicU32::new(0),
            down_after,
        }
    }

    fn is_down(&self) -> bool {
        self.down.load(Ordering::Relaxed)
    }

    /// Record a terminal turn failure. Returns `true` if this call flipped the
    /// state Up→Down (the caller emits the canned "holding your mail" line).
    /// Returns `false` for failures that don't cross the threshold or that land
    /// while already Down (probe failures).
    fn record_failure(&self) -> bool {
        let n = self.consecutive_failures.fetch_add(1, Ordering::Relaxed) + 1;
        if n >= self.down_after {
            !self.down.swap(true, Ordering::Relaxed)
        } else {
            false
        }
    }

    /// Record a successful turn. Returns `true` if this call flipped the state
    /// Down→Up. Always resets the failure counter.
    fn record_success(&self) -> bool {
        self.consecutive_failures.store(0, Ordering::Relaxed);
        self.down.swap(false, Ordering::Relaxed)
    }

    /// Force the Down state immediately — for a *definite* outage like
    /// out-of-energy (a 402), which shouldn't wait out `down_after` flaky retries.
    /// Returns `true` on the Up→Down flip (so the caller emits the notice once).
    fn force_down(&self) -> bool {
        self.consecutive_failures.store(self.down_after, Ordering::Relaxed);
        !self.down.swap(true, Ordering::Relaxed)
    }
}

/// Whether a terminal turn error is the gateway reporting "out of energy"
/// (songguo 402) rather than a generic outage. The ACP adapter collapses the
/// upstream HTTP status into an opaque string, so we match the markers it carries.
/// A miss just degrades to the generic vendor-down message — input is still
/// stashed and caught up either way.
fn is_out_of_energy(err: &anyhow::Error) -> bool {
    let s = format!("{err:#}").to_ascii_lowercase();
    s.contains("songguo_budget_exceeded")
        || s.contains("budget exceeded")
        || s.contains("payment required")
}

#[cfg(test)]
mod vendor_health_tests {
    use super::*;

    fn fresh() -> VendorHealth {
        VendorHealth::new(2)
    }

    #[test]
    fn force_down_flips_immediately_and_once() {
        let h = fresh();
        assert!(h.force_down(), "Up -> Down flips on first force");
        assert!(h.is_down());
        assert!(!h.force_down(), "already Down -> no second flip");
        assert!(h.record_success(), "Down -> Up on success");
        assert!(!h.is_down());
    }

    #[test]
    fn detects_out_of_energy() {
        assert!(super::is_out_of_energy(&anyhow::anyhow!(
            "session/prompt failed: 402 Payment Required songguo_budget_exceeded"
        )));
        assert!(!super::is_out_of_energy(&anyhow::anyhow!("connection reset by peer")));
    }

    #[test]
    fn starts_up() {
        let h = fresh();
        assert!(!h.is_down());
    }

    #[test]
    fn first_failure_does_not_flip_at_threshold_two() {
        let h = fresh();
        assert!(!h.record_failure());
        assert!(!h.is_down());
    }

    #[test]
    fn second_consecutive_failure_flips_down() {
        let h = fresh();
        h.record_failure();
        assert!(h.record_failure(), "the failure that crosses the threshold must report the flip");
        assert!(h.is_down());
    }

    #[test]
    fn success_resets_the_counter() {
        let h = fresh();
        h.record_failure();
        h.record_success();
        // Counter reset: one failure after a success must not flip.
        assert!(!h.record_failure());
        assert!(!h.is_down());
    }

    #[test]
    fn success_after_down_flips_up() {
        let h = fresh();
        h.record_failure();
        h.record_failure();
        assert!(h.is_down());
        assert!(h.record_success(), "the success that ends an outage must report the recovery");
        assert!(!h.is_down());
    }

    #[test]
    fn failure_while_already_down_does_not_re_flip() {
        let h = fresh();
        h.record_failure();
        h.record_failure();
        assert!(h.is_down());
        // A probe failure landing while already Down must not re-report a flip.
        assert!(!h.record_failure());
        assert!(h.is_down());
    }

    #[test]
    fn threshold_one_flips_on_first_failure() {
        let h = VendorHealth::new(1);
        assert!(h.record_failure());
        assert!(h.is_down());
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
    fn is_auth_error_matches_the_gateway_401() {
        // The real shape from a live run: the adapter's `errorKind` is stringified
        // into the anyhow message. The heal path keys off exactly this.
        let e = anyhow::anyhow!(
            "session/prompt failed: Internal error: Failed to authenticate. \
             API Error: 401 invalid user key: {{ \"errorKind\": \"authentication_failed\" }}"
        );
        assert!(is_auth_error(&e));
        // An ordinary failure must not trigger a broker refresh + restart.
        assert!(!is_auth_error(&anyhow::anyhow!("session/prompt failed: connection reset")));
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

/// How far back a scene's raw memory may date and still be re-warmed at startup.
/// Re-warm gives recently-active scenes a live loop again so their pulses can
/// fire — a standing commitment must not need a client connection to be checked.
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
    /// Shared, process-wide LLM-vendor reachability flag. Read by every scene
    /// loop to decide whether to hold mail or drive a turn; written by
    /// `run_turn`'s terminal-failure / success paths. See [`VendorHealth`].
    vendor_health: Arc<VendorHealth>,
    /// Scene→live-subscriber counts, shared with the HTTP front. Rendered into
    /// each turn as one human-model presence sentence, so the mind knows which
    /// channels actually reach the person right now.
    presence: crate::body::presence::Presence,
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
    views_dir: PathBuf,
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
            views_dir,
            turn_seq: AtomicU64::new(0),
            scenes: Mutex::new(HashMap::new()),
            vendor_health: Arc::new(VendorHealth::new(vendor_down_after())),
            last_signal_at: std::sync::Mutex::new(Instant::now()),
            reflect_wake: tokio::sync::Notify::new(),
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

    // Re-warm recently-active scenes, so a standing commitment has a live loop —
    // and therefore pulses — without waiting for a client to connect. Bounded by
    // [`REWARM_WINDOW`]; a long-idle scene stays cold. Boot is not a special
    // case: this merely stands the loops up, and each one's first pulse carries
    // the "host process started Xm ago" fact like any other.
    let rewarm_reactor = reactor.clone();
    tokio::spawn(async move {
        for scene in recent_scenes(rewarm_reactor.inner.memory.data_dir(), REWARM_WINDOW) {
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
            tokio::select! {
                _ = tokio::time::sleep(at.saturating_duration_since(now)) => {}
                _ = reactor.inner.reflect_wake.notified() => continue,
            }
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

/// Scenes whose raw memory saw activity within `window`: the directories under
/// `<data_dir>/memory/raw/` with a recently-modified day folder. Errors read as
/// "no scenes" — re-warm is best-effort.
fn recent_scenes(data_dir: &std::path::Path, window: Duration) -> Vec<Scene> {
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
        let recent = std::fs::read_dir(&path)
            .map(|days| {
                days.flatten().any(|d| {
                    d.metadata()
                        .and_then(|m| m.modified())
                        .map(|t| t >= cutoff)
                        .unwrap_or(false)
                })
            })
            .unwrap_or(false);
        if recent && let Some(name) = path.file_name().and_then(|n| n.to_str()) {
            scenes.push(Scene(name.to_owned()));
        }
    }
    scenes
}

/// Whether a failed prompt is an upstream authentication failure (a 401 from the
/// LLM gateway), which the retry loop heals by refreshing broker credentials. The
/// ACP adapter reports it as `errorKind: "authentication_failed"`, and that kind is
/// stringified into the `anyhow` error message today — so we match on it rather than
/// a typed variant (centralized here to harden later if the acp crate exposes the
/// structured `data`).
fn is_auth_error(err: &anyhow::Error) -> bool {
    err.to_string().contains("authentication_failed")
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
}

/// Why one reorganization pass's prompt drive ended. A pass runs one
/// `session.prompt()`; either it finishes on its own, or new human input lands
/// mid-flight and we cancel it to re-prompt with that input folded in.
enum DriveOutcome {
    /// The prompt ran to completion. Carries the stop-reason string for the log.
    Completed(Option<String>),
    /// New human input arrived while the prompt was still in flight. The turn was
    /// marked for flush and the prompt cancelled; carries the fresh human burst to
    /// fold into the next pass. `session_ok` is false if the post-cancel drain
    /// timed out — the session is wedged and must be discarded before re-prompting.
    Reorganized { burst: Vec<Signal>, session_ok: bool },
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
                Some(id) => workers.follow_up(reactor, id, task).await,
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
    // utterance). Pull the cold-start forward now — spawn the subprocess, open the
    // persistent ACP session, and pre-send the system prompt to warm the backend —
    // so that work is off the first real turn's critical path. The journal snapshot
    // is still delivered by the first real turn (which sees an open, system-prompted
    // but unseeded session). Best-effort; on failure the first turn cold-opens as
    // before.
    warm_up(&reactor, &scene, &mut reactor_session).await;

    // Pulse bookkeeping: the host's recurring self-attention timer. `last_activity`
    // resets on every turn, so pulses only fire into genuine quiet; the first pulse
    // after the loop stands up also carries how long ago the host process started,
    // which is all "wake on boot" amounts to.
    let pulse_every = pulse_interval();
    let loop_started = Instant::now();
    let mut last_activity = Instant::now();
    let mut pulsed_once = false;

    // Pending turn-driving items, hoisted out of the main loop so the batch
    // survives across iterations while in vendor-down mode — a failed probe
    // must not drop the mail it was attempting to deliver. Cleared on a
    // successful turn (the mail went out) and on a normal-path failure (the
    // apology was emitted; re-attempting would re-apologize). Retained on a
    // probe failure so the next probe retries the same mail plus any new arrivals.
    let mut batch: Vec<LoopInput> = Vec::new();
    // Worker reports pulled off `inbound` while a prior turn was reorganizing
    // (cancelling to re-prompt with new human input). They don't drive a reorg
    // themselves — fix-forward, like before — so they're held here and folded into
    // `batch` after the turn returns.
    let mut carryover: Vec<LoopInput> = Vec::new();
    // Next recovery-probe deadline while in vendor-down mode. `None` whenever
    // the vendor is Up (re-armed on the next Down observation) or right after a
    // probe fires (re-armed on the next iteration).
    let mut next_probe: Option<Instant> = None;
    let probe_interval = vendor_probe_interval();

    loop {
        // Wait for a turn-driving reason: a new signal, a fired alarm, a due
        // host pulse, a worker question, or — while the vendor is Down — a
        // recovery probe. Tool control commands (delegate/alarm) are pure
        // side-effects applied without a turn; only a worker `ask` becomes a
        // turn-driving item. The soonest of the mind's alarms, the host pulse,
        // and (while Down) the probe wakes the loop.
        'wait: loop {
            // A batch pre-seeded by carryover (a worker report pulled off the queue
            // while a prior turn reorganized) needs no fresh signal to act on —
            // drive it now while Up. While Down the probe cadence still governs, so
            // fall through to the timer logic below.
            if !batch.is_empty() && !reactor.inner.vendor_health.is_down() {
                break 'wait;
            }
            let down = reactor.inner.vendor_health.is_down();
            // While Down, suppress pulses — they call the model and would just fail.
            // The probe cadence replaces the pulse as the turn-driving timer.
            let pulse_at = if down { None } else { pulse_every.map(|d| last_activity + d) };
            // While Down, arm the recovery probe. Lazy-init on first Down
            // observation so the first probe is a full interval after the
            // outage begins, not at loop start; cleared on Up.
            let probe_at = if down {
                Some(*next_probe.get_or_insert(Instant::now() + probe_interval))
            } else {
                next_probe = None;
                None
            };
            let deadline = [alarms.next_deadline(), pulse_at, probe_at]
                .into_iter()
                .flatten()
                .min();
            let woke = match deadline {
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
                        // Alarms still fire and queue while Down — the mind asked
                        // to be woken, and the note isn't lost — but they don't
                        // alone drive a turn; the probe does.
                        for fired in alarms.take_due(now) {
                            reactor
                                .inner
                                .observatory
                                .record(&scene, EventKind::AlarmFired { note: fired.note.clone() })
                                .await;
                            batch.push(LoopInput::Alarm(fired));
                        }
                        // Recovery probe: attempt catch-up only if mail is
                        // pending. An idle outage wakes here, finds the mailbox
                        // empty, and goes back to sleep — no vendor call spent.
                        if let Some(at) = probe_at
                            && at <= now
                            && !batch.is_empty()
                        {
                            next_probe = None;
                            tracing::info!(scene = %scene, mail = batch.len(), "vendor-down probe firing");
                            break 'wait;
                        }
                        // Probe fired with no mail, or another (already-consumed)
                        // timer woke us — re-arm so the next probe is a full
                        // interval out, not stacked behind the missed one.
                        if probe_at.is_some_and(|at| at <= now) {
                            next_probe = None;
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

        let was_down = reactor.inner.vendor_health.is_down();

        // Commit-after-quiet: wait for things to settle before replying. Skipped
        // while Down — a probe should attempt catch-up ASAP rather than wait for
        // more mail to settle (the probe cadence already coalesces arrivals).
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

        // Probe turns don't apologize — the user already heard the "holding your
        // mail" line when the reactor went Down. Normal turns do.
        let apologize = !was_down;
        match run_turn(&reactor, &scene, &batch, &mut reactor_session, &mut seeded, &mut budget, &mut workers, &beats, apologize, &mut inbound, &mut carryover).await {
            Ok(()) => {
                // The turn delivered the mail; clear the backlog. (If this was a
                // probe, run_turn already flipped the vendor Up via record_success.)
                batch.clear();
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
                                seeded = false;
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
                // The fresh session that replaces it must re-ingest the snapshot.
                seeded = false;
                budget.reset();
                reactor.inner.observatory.set_budget(&scene, 0).await;
                if was_down {
                    // Probe failed: the vendor is still Down. Keep the mail for
                    // the next probe — the session is already discarded above,
                    // and the next probe cold-opens a fresh one and re-seeds.
                    tracing::info!(scene = %scene, mail = batch.len(), "probe failed; holding mail");
                } else {
                    // Normal failure: the apology (or the Down-flip "holding your
                    // mail" line) was emitted inside run_turn. Drop the batch so
                    // it isn't re-attempted on the next turn.
                    batch.clear();
                }
            }
        }

        // Any completed turn is activity: the pulse clock restarts, so pulses
        // only ever fire into genuine quiet.
        last_activity = Instant::now();

        // Worker reports pulled off the queue while this turn reorganized fold into
        // the next batch (fix-forward), just as they would have without reorg. A
        // non-empty carryover (while Up) skips the wait and drives a turn next pass.
        batch.append(&mut carryover);
    }
}

/// One-time scene warm-up, run once before the per-scene loop blocks on its first
/// input. Opens the scene's persistent reactor session — so the subprocess spawn
/// and ACP `session/new` are off the first reply's critical path — then pre-sends
/// the system prompt (the soul plus memory core) on its own, so the backend
/// processes it and the upstream prompt cache populates before any user input
/// arrives. The first real turn then runs against an already-warm session and
/// delivers only the journal snapshot and new signals.
///
/// The warm prompt carries no transcript and no signals, so the model has nothing
/// to act on; any `say`/`show_view` it might still emit is dropped, since the
/// sequencer ignores beats until the first `TurnStart`. The journal snapshot is
/// not sent here — the session stays unseeded, so the first real turn delivers it.
///
/// Best-effort: any failure is logged and leaves the session open-but-unseeded (or
/// unopened), so the first real turn proceeds as it did before.
async fn warm_up(reactor: &Reactor, scene: &Scene, reactor_session: &mut Option<Arc<AcpSession>>) {
    // Defensive: the prologue runs once on a fresh loop, so the session is always
    // cold here, but never re-open an already-open session.
    if reactor_session.is_some() {
        return;
    }
    match open_session(reactor, scene).await {
        Ok(session) => {
            // Drive the warm prompt to completion so the session's prompt slot is
            // parked back for the first real turn. Best-effort: a failed warm just
            // leaves the session open-but-unseeded, as before.
            match session.warm().await {
                Ok(Some(run)) => {
                    if let Err(err) = run.wait().await {
                        tracing::warn!(scene = %scene, error = %err, "reactor warm-up prompt failed");
                    }
                }
                Ok(None) => {}
                Err(err) => {
                    tracing::warn!(scene = %scene, error = %err, "reactor warm-up prompt failed");
                }
            }
            *reactor_session = Some(session);
            tracing::info!(scene = %scene, "reactor session warmed up (system prompt pre-sent)");
        }
        Err(err) => {
            tracing::warn!(scene = %scene, error = %err, "scene warm-up failed; first turn will cold-start");
        }
    }
}

/// Open a fresh persistent reactor session for `scene`, carrying the soul as its
/// system prompt, and record the lifecycle event. The soul references `self.md`,
/// `commitments.md`, and `hot.md` by path, so the session reads the current duties
/// and digest rather than a stale copy. The session consumes the system prompt on
/// its first `prompt()` and never re-sends it. Shared by the warm-up prologue and
/// the cold path of [`run_turn`].
async fn open_session(reactor: &Reactor, scene: &Scene) -> anyhow::Result<Arc<AcpSession>> {
    let system_prompt = reactor.inner.soul.clone();
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

/// Discard the live reactor session after a failed or wedged prompt: record the
/// close, drop it so the next turn cold-opens, and reset the seed/budget so the
/// fresh session re-ingests the journal snapshot. Idempotent — a no-op on the
/// observatory side if the slot is already empty.
async fn discard_reactor_session(
    reactor: &Reactor,
    scene: &Scene,
    reactor_session: &mut Option<Arc<AcpSession>>,
    seeded: &mut bool,
    budget: &mut heartbeat::ContextBudget,
) {
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
}

/// Render a burst of human signals (folded in by a reorganization) as transcript
/// lines — the same shape `render_batch` produces for its `Human` arm.
fn render_human_signals(burst: &[Signal]) -> String {
    use crate::mind::memory::snapshot::{Speaker, transcript_line};
    use std::fmt::Write as _;
    let mut s = String::new();
    for sig in burst {
        let chan = sig.channel.with_stream(sig.stream.as_deref());
        let _ = writeln!(s, "{}", transcript_line(Speaker::Them, &chan, &sig.body));
    }
    s
}

/// Drive one prompt to completion, but race it against new inbound signals so the
/// mind can reorganize mid-reply like a person. The model's `say`/`show_view` tool
/// calls reach the sequencer out of band; this only watches the prompt's update
/// stream and the scene queue.
///
/// - A **human** signal arriving mid-flight is the cue to reconsider: we collect
///   the rest of their burst (a short settle), mark the turn for flush so its stale
///   tail isn't spoken, cancel the prompt, drain+park the session, and hand the
///   burst back as [`DriveOutcome::Reorganized`] for the caller to fold into a fresh
///   pass.
/// - A **worker** report (the only other thing on `inbound` mid-turn) folds into the
///   next batch via `carryover`, exactly as fix-forward did before — it doesn't make
///   the mind reconsider what it's saying.
/// - Otherwise the prompt runs to completion → [`DriveOutcome::Completed`].
///
/// Owns the `SessionRun` end to end (so `wait()`'s by-value consumption stays legal)
/// and does the `prompt()` itself, so an open failure is a retriable `Err` for the
/// caller's attempt loop.
async fn drive_racing_inbound(
    reactor: &Reactor,
    scene: &Scene,
    session: &Arc<AcpSession>,
    prompt_text: String,
    inbound: &mut mpsc::Receiver<LoopInput>,
    carryover: &mut Vec<LoopInput>,
    turn_id: u64,
) -> anyhow::Result<DriveOutcome> {
    // Why the race ended. Kept tiny so the `select!` arms never touch `run` except
    // through `next_update` — `run` is consumed (via `wait()`) only after the loop,
    // sidestepping cross-arm borrow friction.
    enum Ended {
        Completed,
        InboundClosed,
        HumanBurst(Vec<Signal>),
    }

    let mut run = session.prompt(prompt_text).await?;
    let ended = loop {
        tokio::select! {
            upd = run.next_update() => match upd {
                Some(SessionUpdate::ToolCall(stub)) => {
                    tracing::debug!(scene = %scene, variant = stub.raw_variant, "tool call");
                }
                Some(SessionUpdate::Text(_)) => {} // narration, not for saying; drop
                Some(_) => {} // thoughts and unmodelled updates
                None => break Ended::Completed,
            },
            item = inbound.recv() => match item {
                None => break Ended::InboundClosed,
                // They spoke while we were still working on this reply — reorganize.
                Some(LoopInput::Human(sig)) => {
                    let mut burst = vec![sig];
                    // Settle the rest of their burst; workers spill to carryover.
                    loop {
                        match timeout(RESPONSE_SETTLE, inbound.recv()).await {
                            Ok(Some(LoopInput::Human(s))) => burst.push(s),
                            Ok(Some(other)) => carryover.push(other),
                            Ok(None) => break, // inbound closed mid-settle
                            Err(_) => break,   // quiet → burst complete
                        }
                    }
                    break Ended::HumanBurst(burst);
                }
                // A worker report folds into the next batch (fix-forward), as before.
                Some(other) => carryover.push(other),
            },
        }
    };

    match ended {
        Ended::Completed | Ended::InboundClosed => {
            let result = run.wait().await?;
            tracing::debug!(scene = %scene, stop = ?result.stop_reason, "turn finished");
            Ok(DriveOutcome::Completed(Some(format!("{:?}", result.stop_reason))))
        }
        Ended::HumanBurst(burst) => {
            tracing::info!(scene = %scene, turn = turn_id, added = burst.len(), "human spoke mid-reply; reorganizing");
            // Stop the stale tail so any say/show beats still queued for this turn
            // are dropped rather than spoken.
            reactor.inner.interrupts.mark_flush(scene, turn_id).await;
            // Best-effort cancel to free the model call, but we do NOT wait for it:
            // this ACP adapter doesn't honour `session/cancel` promptly (observed —
            // an in-flight prompt never resolved `Cancelled` even after 60s), so
            // draining to reuse the warm session is not viable. Discard the session
            // instead (dropping it kills the subprocess and the orphaned call); the
            // next pass cold-opens a fresh one.
            let _ = session.cancel().await;
            Ok(DriveOutcome::Reorganized { burst, session_ok: false })
        }
    }
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
    // Whether to emit the canned apology on terminal failure. `true` for normal
    // turns (driven by a human/worker/alarm/pulse); `false` for probe turns
    // while in vendor-down mode — those failures are silent, the user already
    // heard the "holding your mail" line when the reactor went Down.
    apologize: bool,
    // The scene inbound queue and a carryover buffer, so a turn can notice human
    // input arriving while it's still working and reorganize — cancel the
    // in-flight reply and re-prompt with the new input folded in — instead of
    // speaking a now-stale reply. Worker reports pulled mid-turn spill to
    // `carryover`, which the caller folds into the next batch.
    inbound: &mut mpsc::Receiver<LoopInput>,
    carryover: &mut Vec<LoopInput>,
) -> anyhow::Result<()> {
    // Computed once, shared by every reorganization pass.
    let worker_status = workers.render_status().await;
    let presence_note =
        format!("## Presence\n{}", reactor.inner.presence.render(scene));

    // If the human barged into a *previous* reply's playback, tell the mind what
    // went unheard — taken once, ahead of all passes. Facts only; how to fold the
    // tail forward is core.md's job.
    let interrupted = reactor
        .inner
        .interrupts
        .take_pending(scene)
        .await
        .map(|i| interrupts::render_interruption(&i))
        .unwrap_or_default();

    // Accumulated across passes: the human's words seen so far (the original batch
    // plus anything a reorganization folds in), and what we'd already spoken before
    // being reorganized (empty in the thinking phase — nothing was said yet).
    let mut new_signals_body = render_batch(batch);
    let mut spoken_so_far = String::new();

    'reorg: loop {
        let turn_id = reactor.inner.turn_seq.fetch_add(1, Ordering::Relaxed);
        let new_signals = format!("## New signals\n{new_signals_body}");
        let reorg_note = if spoken_so_far.trim().is_empty() {
            String::new()
        } else {
            interrupts::render_reorg(spoken_so_far.trim())
        };
        reactor
            .inner
            .observatory
            .record(
                scene,
                EventKind::TurnStarted { turn: turn_id, input: preview(&new_signals_body) },
            )
            .await;

        // Bracket this pass on the sequencer (it renders say()/show_view() that
        // arrive out-of-band as tool calls between these two beats). Each pass is
        // its own turn id and TTS span; the next pass opens a fresh bracket (the
        // sequencer resets per TurnStart), so an abandoned pass's flushed tail can't
        // bleed into it. Sent once per pass, before the retry loop, so every attempt
        // of this pass lives inside one bracket.
        let _ = beats.send(sequencer::Beat::TurnStart { turn: turn_id }).await;

        // Drive the prompt to completion, retrying a failed attempt on a freshly
        // restarted ACP session with exponential backoff. The drive races the prompt
        // against new human input; a mid-flight human burst comes back as
        // `Reorganized` (not an error) and is handled below, outside the retry loop.
        const MAX_ATTEMPTS: u32 = 3;
        let mut attempt: u32 = 0;
        let mut healed = false;
        let mut prompt_chars = 0usize;
        let drive: anyhow::Result<DriveOutcome> = loop {
            attempt += 1;

            // Build the prompt and acquire the session *inside* the attempt, so a
            // failure to open (or to build the snapshot) is itself retriable. An
            // unseeded session — fresh after a discard — re-seeds with the snapshot.
            let attempt_result: anyhow::Result<DriveOutcome> = async {
                let prompt_text = if *seeded {
                    join_sections(&[&worker_status, &presence_note, &interrupted, &reorg_note, &new_signals])
                } else {
                    let snap = build_for_scene(&reactor.inner.memory, scene).await?;
                    join_sections(&[
                        &snap.render_for_prompt(),
                        &worker_status,
                        &presence_note,
                        &interrupted,
                        &reorg_note,
                        &new_signals,
                    ])
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

                drive_racing_inbound(reactor, scene, &session, prompt_text, inbound, carryover, turn_id).await
            }
            .await;

            match attempt_result {
                Ok(outcome) => break Ok(outcome),
                Err(err) => {
                    tracing::warn!(scene = %scene, attempt, error = %err, "prompt attempt failed");
                    // Discard the possibly-wedged session so the next attempt
                    // restarts from cold and rebuilds context from the snapshot.
                    discard_reactor_session(reactor, scene, reactor_session, seeded, budget).await;
                    if attempt >= MAX_ATTEMPTS {
                        break Err(err);
                    }
                    // A 401 means the upstream key the child was spawned with is
                    // stale (broker re-mint / revocation). Re-fetch from the broker
                    // once per turn so the store holds a fresh key; the next attempt
                    // spawns a session that re-resolves it (see AgentLayer::session).
                    // No-op in BYOK, so safe in every mode; a genuinely bad BYOK key
                    // just exhausts the remaining attempts and surfaces, as before.
                    if is_auth_error(&err) && !healed {
                        healed = true;
                        tracing::info!(scene = %scene, "auth failure; refreshing broker credentials before restart");
                        crate::foundation::broker::refresh(reactor.inner.memory.data_dir(), None).await;
                    }
                    // Exponential backoff before the restart: 250ms, then 500ms.
                    let backoff = Duration::from_millis(250u64 << (attempt - 1));
                    tracing::info!(scene = %scene, attempt, ?backoff, "restarting ACP session after backoff");
                    tokio::time::sleep(backoff).await;
                }
            }
        };

        // A successful drive (completed or reorganized) delivered a prompt, so the
        // session is now seeded; a reorganized-but-wedged pass resets this when it
        // discards the session below.
        if drive.is_ok() {
            *seeded = true;
        }

        // Vendor health + apology apply only to terminal outcomes. A reorganization
        // is neither success nor failure — the vendor answered fine, we chose to
        // re-ask — so it touches neither.
        match &drive {
            Ok(DriveOutcome::Completed(_)) => {
                let recovered = reactor.inner.vendor_health.record_success();
                if recovered {
                    tracing::info!(scene = %scene, "vendor recovered; resuming normal turns");
                }
            }
            Err(err) => {
                // Out-of-energy (gateway 402) is a *definite* outage: flip to the
                // mailbox immediately rather than waiting out `down_after`, and
                // nudge toward sub/byok. Either way the existing down/probe path
                // stashes input and catches up when it recovers (energy resets).
                let out_of_energy = is_out_of_energy(err);
                let flipped = if out_of_energy {
                    reactor.inner.vendor_health.force_down()
                } else {
                    reactor.inner.vendor_health.record_failure()
                };
                if flipped {
                    let line = if out_of_energy {
                        "能量用完了。我先把你说的记下来，等能量恢复就接着处理。想现在就继续的话，点菜单栏的 hi 图标就能订阅，或换用自己的 API key。".to_string()
                    } else {
                        "我暂时连不上模型，先攒着你的消息，等恢复了一起处理。".to_string()
                    };
                    let _ = beats.send(sequencer::Beat::Say(line)).await;
                } else if apologize && !out_of_energy {
                    let _ = beats
                        .send(sequencer::Beat::Say(format!(
                            "抱歉，我这边出了点问题，没能完成这次回应。({err})"
                        )))
                        .await;
                }
            }
            Ok(DriveOutcome::Reorganized { .. }) => {}
        }

        // Always close this pass's bracket, even on error, so any open audio span
        // ends and the /thought utterance closes. It hands back what this pass
        // actually spoke, accumulated from its say() calls.
        let (done_tx, done_rx) = oneshot::channel();
        let _ = beats.send(sequencer::Beat::TurnEnd { done: done_tx }).await;
        let reply = done_rx.await.unwrap_or_default();
        // Close the pass on the interrupt registry: clears the live marker, caches
        // the reply for barge-in resolution, back-fills an interrupt that hit it.
        reactor.inner.interrupts.end_turn(scene, turn_id, &reply).await;

        match drive {
            Ok(DriveOutcome::Completed(stop_reason)) => {
                budget.record_turn(prompt_chars, reply.chars().count());
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
                reactor.inner.observatory.set_budget(scene, budget.chars()).await;
                return Ok(());
            }
            Ok(DriveOutcome::Reorganized { burst, session_ok }) => {
                budget.record_turn(prompt_chars, reply.chars().count());
                reactor
                    .inner
                    .observatory
                    .record(
                        scene,
                        EventKind::TurnFinished {
                            turn: turn_id,
                            stop_reason: Some("Reorganized".to_string()),
                            reply_chars: reply.chars().count(),
                            reply: preview(&reply),
                        },
                    )
                    .await;
                // Carry what we'd spoken (if anything) into the reorg note, and fold
                // the new human words into the next pass's signals.
                if !reply.trim().is_empty() {
                    if !spoken_so_far.is_empty() {
                        spoken_so_far.push(' ');
                    }
                    spoken_so_far.push_str(reply.trim());
                }
                new_signals_body.push_str(&render_human_signals(&burst));
                // A speaking-phase reorg (the human talked over live playback) also
                // sets a barge-in note via note_speech; we've folded that fact into
                // the reorg note already, so drain it to avoid a stale duplicate.
                let _ = reactor.inner.interrupts.take_pending(scene).await;
                if !session_ok {
                    // The cancelled prompt's session is wedged — discard it so the
                    // next pass cold-opens a fresh one and re-seeds.
                    discard_reactor_session(reactor, scene, reactor_session, seeded, budget).await;
                }
                reactor.inner.observatory.set_budget(scene, budget.chars()).await;
                continue 'reorg;
            }
            Err(err) => {
                // A pass that failed every attempt propagates, so the caller's error
                // arm runs (the session is already discarded above).
                return Err(err);
            }
        }
    }
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
