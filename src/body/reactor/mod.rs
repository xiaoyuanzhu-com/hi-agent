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
mod voice;
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
/// The scene loop's cheap out-of-energy recheck cadence — how often a held scene wakes
/// to notice the shared poller flipped the vendor back Up. (The poller itself, below,
/// runs on [`OE_POLL_OPEN`]/[`OE_POLL_CLOSED`].)
const OE_POLL_FLOOR: Duration = Duration::from_secs(10);
/// Out-of-energy balance-poll cadence while the face window is on screen: tight, because
/// the user is looking and may have just topped up — recovery should feel immediate.
const OE_POLL_OPEN: Duration = Duration::from_secs(5);
/// …and while the window is shut: nobody is watching, so poll rarely. The closed→open
/// edge ([`crate::foundation::window_state::opened`]) forces an immediate check the
/// moment the window returns, so this only bounds the fully-background case.
const OE_POLL_CLOSED: Duration = Duration::from_secs(3600);

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

/// The classified recovery policy for a terminal turn error — the two states the
/// user asked for, plus the generic fallback.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
enum Outage {
    /// Gateway 402: the account is out of energy. Retrying model calls is pointless
    /// — the budget won't refill for a while — so we poll the balance and resume on
    /// refill (or an upgrade). Announced, with the reset time + upgrade page.
    OutOfEnergy,
    /// Gateway 429 / 529: throttled or overloaded. Transient and self-healing —
    /// back off and retry, silently (the user needn't hear about it).
    RateLimited,
    /// Anything else (5xx, transport, timeout, a missing response): a generic
    /// outage. Informed once (after a blip is absorbed), then retried with backoff.
    Unreachable,
}

/// Classify a terminal turn error by the HTTP status token the ACP adapter folds
/// into its opaque message — the adapter hands us no numeric status, so we scan the
/// flattened text for a standalone status token (same approach as [`has_status_token`]).
/// A miss falls through to [`Outage::Unreachable`], the safe generic path: mail is
/// stashed and retried either way.
fn classify_outage(err: &anyhow::Error) -> Outage {
    let s = format!("{err:#}");
    if has_status_token(&s, 402) {
        Outage::OutOfEnergy
    } else if has_status_token(&s, 429) || has_status_token(&s, 529) {
        Outage::RateLimited
    } else {
        Outage::Unreachable
    }
}

/// How a scene loop should treat the vendor right now — the read side of [`Vendor`].
#[derive(Clone, Copy, Debug)]
enum SceneGate {
    /// Reachable: drive turns normally.
    Go,
    /// Transient outage (429 / generic): hold mail, and drive a catch-up turn once
    /// `at` (the current backoff deadline) passes. A failed retry grows the gap.
    Retry { at: Instant },
    /// Out of energy (402): hold mail but do **not** drive a model turn — recovery
    /// is the shared energy poll, which flips the vendor back Up. `at` is a cheap
    /// recheck so the loop notices the refill and drains its mail.
    Hold { at: Instant },
}

/// The vendor's reachability and, when down, how to recover from it.
#[derive(Clone, Copy, Debug)]
enum VendorState {
    Up,
    /// Out of energy (402). `poll_at` is the next balance check; `resets_at` is the
    /// broker's predicted refill instant when known (paces the poll).
    OutOfEnergy { poll_at: Instant, resets_at: Option<Instant> },
    /// Transient backoff (429 / generic). `try_at` is the next retry deadline;
    /// `attempt` grows the gap toward [`BACKOFF_CAP`]; `silent` suppresses the user
    /// notice for a pure rate-limit (429), which the user needn't hear about.
    Backoff { try_at: Instant, attempt: u32, silent: bool },
}

/// Shared, process-wide view of the upstream LLM vendor and how to recover from an
/// outage. Every scene loop reads it (via [`Vendor::scene_gate`]) to decide whether
/// and when to drive a turn; `run_turn`'s terminal path writes it. The vendor is a
/// shared resource, so one scene detecting an outage steers all of them — and a
/// single [out-of-energy poller](out_of_energy_poller) owns balance polling so N
/// scenes don't each hammer `/energy`.
///
/// The three `note_*` writers return whether the transition warrants a *one-time*
/// user notice (so the reactor announces "out of energy" / "can't reach the model"
/// exactly once), mirroring the old flip-once contract.
struct Vendor {
    state: std::sync::Mutex<VendorState>,
    /// Consecutive *generic* (Unreachable) failures, to absorb a blip before an
    /// informed backoff. Accessed only under `state`'s lock, so effectively part of
    /// the same critical section. Reset on success and on the immediate 402/429 flips.
    generic_failures: AtomicU32,
    down_after: u32,
    /// The transient-outage retry base; the gap is `base · 2^attempt`, capped at 1h.
    base: Duration,
    /// Wakes the out-of-energy poller on entering [`VendorState::OutOfEnergy`].
    oe_signal: tokio::sync::Notify,
}

impl Vendor {
    fn new(down_after: u32, base: Duration) -> Self {
        Self {
            state: std::sync::Mutex::new(VendorState::Up),
            generic_failures: AtomicU32::new(0),
            down_after,
            base,
            oe_signal: tokio::sync::Notify::new(),
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

    /// The scene loop's scheduling read: drive now (Go), retry at a deadline
    /// (Retry), or hold mail without a model call (Hold, out of energy).
    fn scene_gate(&self) -> SceneGate {
        match *self.state.lock().unwrap() {
            VendorState::Up => SceneGate::Go,
            VendorState::Backoff { try_at, .. } => SceneGate::Retry { at: try_at },
            // A cheap recheck (not the poll cadence): the shared poller owns the
            // `/energy` call; the scene only needs to notice the resulting flip Up.
            VendorState::OutOfEnergy { .. } => SceneGate::Hold { at: Instant::now() + OE_POLL_FLOOR },
        }
    }

    /// Terminal 402. Flip to out-of-energy immediately (a 402 is definite, not a
    /// blip), kick the balance poll, and wake the poller; `resets_at` is kept only for
    /// the refill-time display. Returns `true` on the flip *into* out-of-energy so the
    /// caller announces it once.
    fn note_out_of_energy(&self, resets_at: Option<Instant>) -> bool {
        let mut st = self.state.lock().unwrap();
        self.generic_failures.store(0, Ordering::Relaxed);
        let entering = !matches!(*st, VendorState::OutOfEnergy { .. });
        if entering {
            *st = VendorState::OutOfEnergy { poll_at: oe_next_poll(), resets_at };
        }
        drop(st);
        if entering {
            self.oe_signal.notify_one();
        }
        entering
    }

    /// Terminal 429/529. Silent transient backoff — grow the gap if already backing
    /// off, else start at the base. A 402 in effect is the stronger state and stands.
    fn note_rate_limited(&self) {
        let mut st = self.state.lock().unwrap();
        self.generic_failures.store(0, Ordering::Relaxed);
        if matches!(*st, VendorState::OutOfEnergy { .. }) {
            return;
        }
        let attempt = match *st {
            VendorState::Backoff { attempt, .. } => attempt.saturating_add(1),
            _ => 0,
        };
        *st = VendorState::Backoff { try_at: Instant::now() + self.backoff(attempt), attempt, silent: true };
    }

    /// Terminal generic outage. Absorb one blip via `down_after`, then flip to an
    /// *informed* backoff. Returns `true` exactly on that flip (announce once);
    /// `false` while still absorbing, already backing off, or out of energy.
    fn note_unreachable(&self) -> bool {
        let mut st = self.state.lock().unwrap();
        match *st {
            // A 402's poll-based recovery is stronger; leave it (and its schedule).
            VendorState::OutOfEnergy { .. } => false,
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

    /// The next balance-poll instant while out of energy, or `None` once recovered
    /// (the poller then parks until re-signalled).
    fn oe_poll_at(&self) -> Option<Instant> {
        match *self.state.lock().unwrap() {
            VendorState::OutOfEnergy { poll_at, .. } => Some(poll_at),
            _ => None,
        }
    }

    /// Re-pace the balance poll after a check that found the account still empty.
    /// No-op if the vendor recovered concurrently. `resets_at` is stored only for the
    /// refill-time display; the poll cadence comes from the window state.
    fn oe_reschedule(&self, resets_at: Option<Instant>) {
        let mut st = self.state.lock().unwrap();
        if let VendorState::OutOfEnergy { poll_at, resets_at: slot } = &mut *st {
            *slot = resets_at;
            *poll_at = oe_next_poll();
        }
    }
}

/// The next out-of-energy balance poll: [`OE_POLL_OPEN`] while the face window is on
/// screen, [`OE_POLL_CLOSED`] while it's shut. This replaces the old pace-from-reset
/// math — an out-of-band top-up (the user paying) is exactly what the predicted reset
/// can't foresee, so we poll on a fixed cadence and let the fetched balance be the
/// ground truth. The closed→open edge additionally cuts the current sleep short (see
/// the poller), so reopening the window re-checks immediately regardless of this.
fn oe_next_poll() -> Instant {
    let gap = if crate::foundation::window_state::is_open() {
        OE_POLL_OPEN
    } else {
        OE_POLL_CLOSED
    };
    Instant::now() + gap
}

/// Parse the broker's RFC3339 `resets_at` into a monotonic [`Instant`], or `None`
/// if blank/unparseable/already past — bridging wall-clock (chrono) to the
/// monotonic clock the poll scheduler runs on.
fn resets_at_instant(resets_at: &str) -> Option<Instant> {
    let reset = chrono::DateTime::parse_from_rfc3339(resets_at.trim()).ok()?;
    let gap = reset.with_timezone(&chrono::Utc) - chrono::Utc::now();
    gap.to_std().ok().map(|d| Instant::now() + d)
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

/// Find `status` as a standalone status token in `text` — the digits bounded by
/// non-digits, so "API Error: 402 …" matches but a longer number that merely
/// contains those digits (a request id, a timestamp) does not.
fn has_status_token(text: &str, status: u16) -> bool {
    let needle = status.to_string();
    text.match_indices(&needle).any(|(i, _)| {
        let before = text[..i].chars().next_back();
        let after = text[i + needle.len()..].chars().next();
        before.is_none_or(|c| !c.is_ascii_digit()) && after.is_none_or(|c| !c.is_ascii_digit())
    })
}

#[cfg(test)]
mod vendor_tests {
    use super::*;

    fn fresh() -> Vendor {
        Vendor::new(2, Duration::from_secs(30))
    }

    #[test]
    fn classify_by_status_token() {
        use Outage::*;
        // A 402 anywhere in the ACP-translated error → out of energy, whatever body
        // the gateway attaches (we key on the status code, not a vendor slug).
        assert_eq!(
            classify_outage(&anyhow::anyhow!("session/prompt failed: API Error: 402 {{\"error\":\"quota\"}}")),
            OutOfEnergy
        );
        // The historical songguo shape still reads as out of energy.
        assert_eq!(
            classify_outage(&anyhow::anyhow!("402 Payment Required songguo_budget_exceeded")),
            OutOfEnergy
        );
        // 429 (throttle) and 529 (overload) are both transient rate limits.
        assert_eq!(classify_outage(&anyhow::anyhow!("API Error: 429 too many requests")), RateLimited);
        assert_eq!(classify_outage(&anyhow::anyhow!("API Error: 529 overloaded")), RateLimited);
        // No status code → generic outage.
        assert_eq!(classify_outage(&anyhow::anyhow!("connection reset by peer")), Unreachable);
        // A longer number that merely contains "402" must not trip it.
        assert_eq!(classify_outage(&anyhow::anyhow!("request id 1140228 timed out")), Unreachable);
    }

    #[test]
    fn starts_up() {
        assert!(!fresh().is_down());
    }

    #[test]
    fn out_of_energy_flips_immediately_and_announces_once() {
        let v = fresh();
        assert!(v.note_out_of_energy(None), "402 flips Up -> OutOfEnergy and announces");
        assert!(v.is_down());
        assert!(!v.note_out_of_energy(None), "already out of energy -> no second announce");
        // Out of energy holds mail without a model retry — recovery is the poller.
        assert!(matches!(v.scene_gate(), SceneGate::Hold { .. }));
        assert!(v.note_success(), "a refill ends the outage");
        assert!(!v.is_down());
    }

    #[test]
    fn rate_limited_is_silent_and_retries() {
        let v = fresh();
        v.note_rate_limited(); // silent: no announce return value at all
        assert!(v.is_down());
        assert!(matches!(v.scene_gate(), SceneGate::Retry { .. }), "429 backs off and retries");
        assert!(v.note_success());
        assert!(!v.is_down());
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

    #[test]
    fn out_of_energy_poll_tracks_the_window() {
        use crate::foundation::window_state;
        let now = Instant::now();
        // Window shut → poll rarely (the background cadence, so an idle machine nobody
        // is watching doesn't hammer the broker).
        window_state::set_open(false);
        let closed = oe_next_poll().saturating_duration_since(now);
        assert!(closed >= OE_POLL_CLOSED - Duration::from_secs(1), "hidden → slow");
        // Window on screen → poll tight, so a fresh top-up is noticed within seconds.
        window_state::set_open(true);
        let open = oe_next_poll().saturating_duration_since(now);
        assert!(open <= OE_POLL_OPEN + Duration::from_secs(1), "on screen → fast");
        window_state::set_open(false);
    }

    #[test]
    fn out_of_energy_outranks_a_concurrent_backoff() {
        let v = fresh();
        v.note_out_of_energy(None);
        // A 429/generic landing while out of energy must not downgrade the state.
        v.note_rate_limited();
        assert!(matches!(v.scene_gate(), SceneGate::Hold { .. }));
        assert!(!v.note_unreachable());
        assert!(matches!(v.scene_gate(), SceneGate::Hold { .. }));
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

    // Out-of-energy poller: while any scene has flipped the vendor out of energy
    // (402), poll the balance on an adaptive cadence and flip back Up the moment it
    // refills. Parks (no polling, no model calls) whenever the vendor is reachable —
    // one task for the whole process, so N scenes don't each hammer `/energy`.
    let energy_reactor = reactor.clone();
    tokio::spawn(async move {
        out_of_energy_poller(energy_reactor).await;
    });

    reactor
}

/// The single out-of-energy poller. While any scene has flipped the vendor to
/// [`VendorState::OutOfEnergy`], re-fetch the balance on the window-gated cadence
/// ([`OE_POLL_OPEN`] on screen / [`OE_POLL_CLOSED`] hidden) and flip the vendor back Up
/// the moment it refills; then the scene loops' held mail drains on their next pass.
/// The face window coming to the front cuts the current wait short and re-checks at
/// once (a payment made while it was hidden shouldn't wait out the schedule). Parks on
/// [`Vendor::oe_signal`] while the vendor is reachable, so an account in credit costs
/// nothing.
async fn out_of_energy_poller(reactor: Reactor) {
    let data_dir = reactor.inner.memory.data_dir().to_path_buf();
    loop {
        // Park until a scene reports out of energy (or act on a pending schedule).
        let poll_at = match reactor.inner.vendor.oe_poll_at() {
            Some(at) => at,
            None => {
                reactor.inner.vendor.oe_signal.notified().await;
                continue;
            }
        };
        let now = Instant::now();
        if poll_at > now {
            // Wait for the scheduled poll — but cut it short if the face window comes to
            // the front, since the user may have just paid on the web and wants to keep
            // going now. That edge polls immediately, then the loop resumes the (now
            // "open") cadence.
            tokio::select! {
                _ = tokio::time::sleep(poll_at - now) => {}
                _ = crate::foundation::window_state::opened().notified() => {}
            }
        }
        // A concurrent success may have cleared the outage while we slept.
        if reactor.inner.vendor.oe_poll_at().is_none() {
            continue;
        }
        match crate::foundation::broker::poll_energy_now(&data_dir).await {
            Some(en) if en.remaining > 0 => {
                if reactor.inner.vendor.note_success() {
                    tracing::info!(remaining = en.remaining, "energy refilled; resuming turns");
                }
            }
            Some(en) => {
                tracing::info!(remaining = en.remaining, resets_at = %en.resets_at, "still out of energy; re-pacing poll");
                reactor.inner.vendor.oe_reschedule(resets_at_instant(&en.resets_at));
            }
            // Poll failed (offline / no token / BYOK). Retry at the floor cadence.
            None => reactor.inner.vendor.oe_reschedule(None),
        }
    }
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
    /// Process shutdown began while this loop was idle — stop waiting and exit.
    Shutdown,
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
    //
    // Split mode's voice is a direct Messages call, so the reactor ACP session is
    // never driven — skip warming it. That warm is a subprocess spawn *plus* a full
    // LLM warm turn, `.await`ed here before the loop reads any input; in split mode it
    // is pure waste that also stalls the first real turn behind it. Cognition warms
    // lazily on its first turn instead. See docs/reactor-cognition-split.md.
    if !voice::split_enabled() {
        warm_up(&reactor, &scene, &mut reactor_session).await;
    }

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
    // drop the mail it was attempting to deliver, and out-of-energy mail waits here
    // for the refill. Cleared on a successful turn (the mail went out) and on a
    // reachable-but-failed blip (the apology was emitted); held while down.
    let mut batch: Vec<LoopInput> = Vec::new();
    // Worker reports pulled off `inbound` while a prior turn was reorganizing
    // (cancelling to re-prompt with new human input). They don't drive a reorg
    // themselves — fix-forward, like before — so they're held here and folded into
    // `batch` after the turn returns.
    let mut carryover: Vec<LoopInput> = Vec::new();

    loop {
        // Wait for a turn-driving reason: a new signal, a fired alarm, a due host
        // pulse, a worker question, or — while the vendor is down — a backoff retry
        // (429/generic) or a recheck that notices an out-of-energy refill. Tool
        // control commands (delegate/alarm) are pure side-effects applied without a
        // turn; only a worker `ask` becomes a turn-driving item.
        'wait: loop {
            let gate = reactor.inner.vendor.scene_gate();
            // A batch pre-seeded by carryover (a worker report pulled off the queue
            // while a prior turn reorganized) needs no fresh signal to act on — drive
            // it now while reachable. While down, fall through to the timer logic.
            if !batch.is_empty() && matches!(gate, SceneGate::Go) {
                break 'wait;
            }
            let down = !matches!(gate, SceneGate::Go);
            // While down, suppress pulses — they call the model and would just fail.
            let pulse_at = if down { None } else { pulse_every.map(|d| last_activity + d) };
            // While down, the recovery timer: a backoff retry deadline (429/generic),
            // or a cheap recheck to notice an out-of-energy refill (the shared poller
            // owns the actual `/energy` call). Up → no such timer.
            let recover_at = match gate {
                SceneGate::Go => None,
                SceneGate::Retry { at } | SceneGate::Hold { at } => Some(at),
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

        // Retry turns don't apologize — the user already heard the "holding your
        // mail" line when the vendor first went down. Normal turns do.
        let apologize = !was_down;
        match run_turn(&reactor, &scene, &batch, &mut reactor_session, &mut seeded, &mut budget, &mut workers, &beats, apologize, &mut inbound, &mut carryover).await {
            Ok(()) => {
                // The turn delivered the mail; clear the backlog. (If this was a
                // retry, run_turn already flipped the vendor Up via note_success.)
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
                // Key on the vendor state run_turn just wrote, not the pre-turn one:
                // a turn that hit a 402/429 (even the first, from Up) left the vendor
                // down, so hold the mail — out of energy drains it on refill; a
                // backoff drives it at the next retry deadline. Only a still-reachable
                // blip (already apologized inside run_turn) drops it.
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
                        note_warmup_outage(reactor, &err);
                    }
                }
                Ok(None) => {}
                Err(err) => {
                    tracing::warn!(scene = %scene, error = %err, "reactor warm-up prompt failed");
                    note_warmup_outage(reactor, &err);
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

/// A warm-up prompt failed. Warm-up is best-effort, so transient/generic failures
/// are left alone (the first real turn re-tries and classifies them). But a *definite*
/// out-of-energy (402) is worth acting on now: flip the vendor so the scene holds its
/// mail instead of hammering, the shared poller starts watching for the refill, and —
/// the point here — the web app raises the out-of-energy hint at boot, without waiting
/// for the user's first turn to trip the same 402.
fn note_warmup_outage(reactor: &Reactor, err: &anyhow::Error) {
    if matches!(classify_outage(err), Outage::OutOfEnergy) {
        let resets_at = crate::foundation::credentials::Credentials::load(reactor.inner.memory.data_dir())
            .energy
            .map(|e| e.resets_at)
            .unwrap_or_default();
        reactor.inner.vendor.note_out_of_energy(resets_at_instant(&resets_at));
        crate::foundation::energy_state::note_402(reactor.inner.memory.data_dir());
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
/// A split-mode turn: the fast **reactor** voice. One direct Anthropic Messages call
/// (`speaking.md` as the system prompt, the assembled turn context as the user
/// message) produces the spoken words, fed straight to the sequencer — no ACP
/// session, no tools, no agentic loop. This is the fast, speaking-rule-conformant
/// conversational voice of the reactor/cognition split.
///
/// Cognition — the agentic thinker/worker — runs in parallel: the turn's human
/// request is handed to a persistent cognition worker ([`workers::WorkerRegistry::cognize`]),
/// which thinks and works off the floor and reports back as an ordinary
/// `LoopInput::Worker` the reactor voices on a later turn. So the reactor stays the
/// single fast voice while the real work happens elsewhere. Still env-gated (default
/// off) pending build + measurement.
///
/// Mirrors [`run_turn`]'s reorganization/barge-in shape: a human burst arriving
/// mid-call drops the in-flight request (dropping the future cancels the HTTP call)
/// and re-asks with their words folded in; a worker report spills to `carryover`.
async fn run_reactor_turn(
    reactor: &Reactor,
    scene: &Scene,
    batch: &[LoopInput],
    workers: &mut workers::WorkerRegistry,
    beats: &mpsc::Sender<sequencer::Beat>,
    inbound: &mut mpsc::Receiver<LoopInput>,
    carryover: &mut Vec<LoopInput>,
) -> anyhow::Result<()> {
    // Resolve the fast-voice wire up front: an unconfigured or non-Claude wire can't
    // speak, and failing before any turn bracket opens keeps the sequencer clean.
    let system = crate::identity::reactor_system_prompt();
    let agent_cfg =
        crate::foundation::config::AgentConfig::resolve(reactor.inner.memory.data_dir());
    let msg_cfg = match voice::config_from(&agent_cfg) {
        Ok(c) => c,
        Err(e) => {
            tracing::warn!(scene = %scene, error = %e, "reactor voice: cannot configure; turn is mute");
            return Err(e);
        }
    };

    let presence_note = format!("## Presence\n{}", reactor.inner.presence.render(scene));
    let interrupted = reactor
        .inner
        .interrupts
        .take_pending(scene)
        .await
        .map(|i| interrupts::render_interruption(&i))
        .unwrap_or_default();

    // Accumulated across reorganizations: the human words seen so far, and what we had
    // already spoken before being reorganized (empty here — a reactor reorg cancels
    // the call before it speaks).
    let mut new_signals_body = render_batch(batch);
    let mut spoken_so_far = String::new();

    // One pass's race outcome between the Messages call and new inbound.
    enum Step {
        Spoke,
        Reorganize(Vec<Signal>),
        Failed(anyhow::Error),
    }

    loop {
        let turn_id = reactor.inner.turn_seq.fetch_add(1, Ordering::Relaxed);
        let new_signals = format!("## New signals\n{new_signals_body}");
        let reorg_note = if spoken_so_far.trim().is_empty() {
            String::new()
        } else {
            interrupts::render_reorg(spoken_so_far.trim())
        };

        // The reactor always reads the recent conversation (the memory snapshot), so
        // its voice reconciles with what was already said rather than repeating it.
        let snap = build_for_scene(&reactor.inner.memory, scene).await?;
        let context = join_sections(&[
            &snap.render_for_prompt(),
            &presence_note,
            &interrupted,
            &reorg_note,
            &new_signals,
        ]);

        let _ = beats.send(sequencer::Beat::TurnStart { turn: turn_id }).await;

        // Race the Messages call against new inbound. Box::pin so the same call stays
        // alive across worker reports (they only spill to carryover); break on
        // completion, error, or a human burst. Boxing makes the future Unpin, so it
        // can be re-polled in the select and awaited directly.
        tracing::info!(scene = %scene, ctx_chars = context.chars().count(), "reactor voice: calling model");
        let mut speak_fut = Box::pin(voice::speak(&msg_cfg, &system, &context));
        let step = loop {
            tokio::select! {
                res = &mut speak_fut => break match res {
                    Ok(text) => {
                        tracing::info!(scene = %scene, reply_chars = text.chars().count(), "reactor voice: replied");
                        if !text.trim().is_empty() {
                            let _ = beats.send(sequencer::Beat::Say(text)).await;
                        }
                        Step::Spoke
                    }
                    Err(e) => Step::Failed(e),
                },
                item = inbound.recv() => match item {
                    // Inbound closed (shutdown): stop racing and just finish the reply.
                    None => break match (&mut speak_fut).await {
                        Ok(text) => {
                            if !text.trim().is_empty() {
                                let _ = beats.send(sequencer::Beat::Say(text)).await;
                            }
                            Step::Spoke
                        }
                        Err(e) => Step::Failed(e),
                    },
                    // They spoke while we were composing — reorganize. Settle the rest
                    // of the burst; workers spill to carryover.
                    Some(LoopInput::Human(sig)) => {
                        let mut burst = vec![sig];
                        loop {
                            match timeout(RESPONSE_SETTLE, inbound.recv()).await {
                                Ok(Some(LoopInput::Human(s))) => burst.push(s),
                                Ok(Some(other)) => carryover.push(other),
                                Ok(None) => break,
                                Err(_) => break,
                            }
                        }
                        break Step::Reorganize(burst);
                    }
                    // A worker report folds into the next turn; keep composing.
                    Some(other) => carryover.push(other),
                },
            }
        };

        // On a terminal outage, tell the user once we're holding their mail — sent
        // inside the bracket so the line actually plays.
        if let Step::Failed(err) = &step {
            tracing::warn!(scene = %scene, error = %err, "reactor voice failed");
            if reactor.inner.vendor.note_unreachable() {
                let _ = beats
                    .send(sequencer::Beat::Say(
                        "我暂时连不上模型，先攒着你的消息，等恢复了一起处理。".to_string(),
                    ))
                    .await;
            }
        }

        // Close this pass's bracket and capture what was spoken.
        let (done_tx, done_rx) = oneshot::channel();
        let _ = beats.send(sequencer::Beat::TurnEnd { done: done_tx }).await;
        let reply = done_rx.await.unwrap_or_default();
        reactor.inner.interrupts.end_turn(scene, turn_id, &reply).await;

        match step {
            Step::Spoke => {
                let _ = reactor.inner.vendor.note_success();
                // Hand the turn's human request to cognition — the agentic thinker —
                // so it works off the floor while the voice moves on; its report rides
                // back as a WorkerReport the reactor voices next turn. Spawned once per
                // scene, then followed up. Nothing to hand off on a pure report/pulse turn.
                let task = render_human_from_batch(batch);
                if !task.trim().is_empty() {
                    if let Err(e) = workers.cognize(reactor, task).await {
                        tracing::warn!(scene = %scene, error = %e, "cognition spawn/follow-up failed");
                    }
                }
                return Ok(());
            }
            Step::Failed(err) => return Err(err),
            Step::Reorganize(burst) => {
                // The call was cancelled before it spoke, so there's no stale tail;
                // fold the burst in and re-ask.
                spoken_so_far.push_str(&reply);
                new_signals_body.push_str(&render_human_signals(&burst));
            }
        }
    }
}

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
    // Split mode (prototype, env-gated): the fast reactor voice handles the turn — one
    // direct Messages call, no ACP session or tools. Default off, so the agentic path
    // below is unchanged. See docs/reactor-cognition-split.md.
    if voice::split_enabled() {
        tracing::info!(scene = %scene, inputs = batch.len(), "split mode: reactor voice turn");
        return run_reactor_turn(reactor, scene, batch, workers, beats, inbound, carryover).await;
    }

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
                    // Shutdown in progress: the children just took the same SIGINT/
                    // SIGTERM this failure reflects. Don't respawn — a fresh session
                    // opened now would race the ACP reap and could orphan a child.
                    // Surface the failure and let the loop wind down.
                    if reactor.inner.shutdown.is_triggered() {
                        tracing::debug!(scene = %scene, "prompt failed during shutdown; not restarting");
                        break Err(err);
                    }
                    // A 402/429 is definite for this turn: retrying on a fresh session
                    // within a few hundred ms cannot succeed (the budget won't refill;
                    // the throttle won't lift). Surface it now and let the terminal
                    // handler below pick the recovery policy (poll energy / back off),
                    // instead of burning MAX_ATTEMPTS respawns against a doomed gateway.
                    if matches!(classify_outage(&err), Outage::OutOfEnergy | Outage::RateLimited) {
                        break Err(err);
                    }
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
                if reactor.inner.vendor.note_success() {
                    tracing::info!(scene = %scene, "vendor recovered; resuming normal turns");
                }
            }
            Err(err) => match classify_outage(err) {
                Outage::OutOfEnergy => {
                    // Definite: flip to out-of-energy immediately. That flip holds the
                    // scene's mail and wakes the balance poller; `note_402` raises the
                    // web app's out-of-energy hint at once (managed mode only). No more
                    // model calls until the poller sees the balance refill. `resets_at`
                    // paces the balance poll and feeds the hint (a 402 body carries none).
                    let resets_at = crate::foundation::credentials::Credentials::load(reactor.inner.memory.data_dir())
                        .energy
                        .map(|e| e.resets_at)
                        .unwrap_or_default();
                    reactor.inner.vendor.note_out_of_energy(resets_at_instant(&resets_at));
                    crate::foundation::energy_state::note_402(reactor.inner.memory.data_dir());
                }
                Outage::RateLimited => {
                    // Transient throttle: back off and retry, silently — a rate limit
                    // that resolves itself isn't worth interrupting the user over.
                    reactor.inner.vendor.note_rate_limited();
                }
                Outage::Unreachable => {
                    // Generic outage: absorb a blip, then tell the user once we're
                    // holding their mail; retry on the growing backoff until it clears.
                    if reactor.inner.vendor.note_unreachable() {
                        let _ = beats
                            .send(sequencer::Beat::Say(
                                "我暂时连不上模型，先攒着你的消息，等恢复了一起处理。".to_string(),
                            ))
                            .await;
                    } else if apologize {
                        let _ = beats
                            .send(sequencer::Beat::Say(format!(
                                "抱歉，我这边出了点问题，没能完成这次回应。({err})"
                            )))
                            .await;
                    }
                }
            },
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
