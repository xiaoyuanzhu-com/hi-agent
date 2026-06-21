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
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::sync::atomic::{AtomicU64, Ordering};
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

use crate::acp::{AcpSession, SessionOpts, SessionUpdate};
use crate::agent::{AgentLayer, SessionRole};
use crate::memory::{Memory, build_for_scene};
use crate::observatory::{EventKind, Observatory, SessionKind};
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
/// conclude with nothing to do or say. Override via `HI_AGENT_PULSE`; `0`/`off`
/// disables. Boot is not a special case — the first pulse after the host starts
/// simply carries that fact.
const DEFAULT_PULSE: Duration = Duration::from_secs(1800);

/// Resolve the pulse interval: `HI_AGENT_PULSE` in alarm-delay grammar if set
/// (`None` for `0`/`off` — pulses disabled), else [`DEFAULT_PULSE`].
fn pulse_interval() -> Option<Duration> {
    match std::env::var(crate::config::ENV_PULSE) {
        Ok(v) => {
            let v = v.trim().to_owned();
            if v.is_empty() {
                return Some(DEFAULT_PULSE);
            }
            if v.eq_ignore_ascii_case("off") {
                return None;
            }
            match parse_delay(&v) {
                Some(d) if d.is_zero() => None,
                Some(d) => Some(d),
                None => Some(DEFAULT_PULSE),
            }
        }
        Err(_) => Some(DEFAULT_PULSE),
    }
}

/// Whether the reflection ("sleep") pass runs at all. On unless `HI_AGENT_REFLECT`
/// is `off` — a master escape hatch to disable consolidation without touching the
/// cadence (see [`reflect_interval`]).
fn reflect_enabled() -> bool {
    !std::env::var(crate::config::ENV_REFLECT)
        .map(|v| v.trim().eq_ignore_ascii_case("off"))
        .unwrap_or(false)
}

/// Default base reflection cadence — how often a scene with fresh input
/// consolidates ([`reflect_interval`]). The idle backoff grows from here.
const DEFAULT_REFLECT_EVERY: Duration = Duration::from_secs(60);
/// Default ceiling on the idle backoff ([`reflect_max_interval`]): a long-quiet
/// scene re-checks at most this often.
const DEFAULT_REFLECT_MAX: Duration = Duration::from_secs(8 * 3600);

/// Resolve a duration env in alarm-delay grammar (`90s`/`30m`/`1h`; bare integer =
/// seconds): `None` for `off`/`0` (disabled), the parsed value, or `default` when
/// unset / blank / unparseable.
fn duration_env(var: &str, default: Duration) -> Option<Duration> {
    match std::env::var(var) {
        Ok(v) => {
            let v = v.trim();
            if v.is_empty() {
                return Some(default);
            }
            if v.eq_ignore_ascii_case("off") {
                return None;
            }
            match parse_delay(v) {
                Some(d) if d.is_zero() => None,
                Some(d) => Some(d),
                None => Some(default),
            }
        }
        Err(_) => Some(default),
    }
}

/// The base reflection cadence, or `None` if reflection is off
/// (`HI_AGENT_REFLECT=off`) or `HI_AGENT_REFLECT_EVERY` is `0`/`off`. A scene with
/// fresh input consolidates this often; once it goes quiet the gap backs off from
/// here up to [`reflect_max_interval`].
fn reflect_interval() -> Option<Duration> {
    reflect_enabled()
        .then(|| duration_env(crate::config::ENV_REFLECT_EVERY, DEFAULT_REFLECT_EVERY))
        .flatten()
}

/// The ceiling on the idle backoff: a caught-up, quiet scene doubles its gap from
/// the base each pass but never past this. Always returns a value (no `off`); a
/// `0`/blank `HI_AGENT_REFLECT_MAX` falls back to the default.
fn reflect_max_interval() -> Duration {
    duration_env(crate::config::ENV_REFLECT_MAX, DEFAULT_REFLECT_MAX).unwrap_or(DEFAULT_REFLECT_MAX)
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

/// How far back a scene's raw memory may date and still be re-warmed at startup.
/// Re-warm gives recently-active scenes a live loop again so their pulses can
/// fire — a standing commitment must not need a client connection to be checked.
const REWARM_WINDOW: Duration = Duration::from_secs(7 * 24 * 3600);

/// Built-in base prompts, embedded at compile time and materialised to disk by
/// [`install_prompts`]. Most are authored as files an agent *reads*, not text inlined
/// into context: the *mind* is handed `core.md` — who it is and the machinery
/// (talking, presenting by ref, delegating) — `speaking.md` — the rhythm of
/// conversation, when to speak and how much — and `meaning.md` — that its purpose is
/// its own to find — by [`load_soul`]'s seed, and Reads them itself. `appearance.md`
/// and `aesthetic.md` are the *view builder's* guides — the mechanics of
/// authoring/saving a view, and the taste it has to clear — read off disk by a build
/// sub-agent. `reflection.md` is the exception: it is the consolidation session's
/// whole instruction set, so it is **inlined** as that session's system prompt (see
/// [`reflection_prompt`]) rather than Read — the session needs it to know what to do
/// at all. All ship in the binary and refresh on every build.
const CORE_BASE: &str = include_str!("core.md");
const SPEAKING_BASE: &str = include_str!("speaking.md");
const MEANING_BASE: &str = include_str!("meaning.md");
const APPEARANCE_BASE: &str = include_str!("appearance.md");
const AESTHETIC_BASE: &str = include_str!("aesthetic.md");
const REFLECTION_BASE: &str = include_str!("reflection.md");

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
/// (`core.md`, `speaking.md`, `meaning.md`, `appearance.md`, `aesthetic.md`,
/// `reflection.md`) are rewritten every boot so they stay current; operator edits
/// live in the never-touched `*.local.md` siblings. `core.md`/`speaking.md`/`meaning.md`
/// (the mind, via [`load_soul`]'s seed) and `appearance.md`/`aesthetic.md` (the
/// view-builder sub-agent) are Read off disk by an agent; `reflection.md` is instead
/// inlined as the reflection session's system prompt ([`reflection_prompt`]). So each
/// follows one workflow: ship embedded → materialise here → consumed from disk at runtime.
pub fn install_prompts(data_dir: &Path) -> std::io::Result<()> {
    let dir = data_dir.join("prompts");
    std::fs::create_dir_all(&dir)?;
    std::fs::write(dir.join("core.md"), compose_prompt(CORE_BASE, &dir, "core.local.md"))?;
    std::fs::write(dir.join("speaking.md"), compose_prompt(SPEAKING_BASE, &dir, "speaking.local.md"))?;
    std::fs::write(dir.join("meaning.md"), compose_prompt(MEANING_BASE, &dir, "meaning.local.md"))?;
    std::fs::write(dir.join("appearance.md"), compose_prompt(APPEARANCE_BASE, &dir, "appearance.local.md"))?;
    std::fs::write(dir.join("aesthetic.md"), compose_prompt(AESTHETIC_BASE, &dir, "aesthetic.local.md"))?;
    std::fs::write(dir.join("reflection.md"), compose_prompt(REFLECTION_BASE, &dir, "reflection.local.md"))?;
    tracing::info!(dir = %dir.display(), "installed bundled prompts (core.md, speaking.md, meaning.md, appearance.md, aesthetic.md, reflection.md)");
    Ok(())
}

/// The reflection ("sleep") session's system prompt: the materialised
/// `<data_dir>/prompts/reflection.md` (operator-overridable via `reflection.local.md`),
/// or the embedded [`REFLECTION_BASE`] when that file is missing or empty. Unlike
/// `core.md`/`speaking.md`, this is **inlined** as the reflection session's system
/// prompt rather than Read by the agent — it *is* the task's instructions, so it must
/// be present before the session can act. Read fresh each round, so an operator edit
/// takes effect without a restart.
pub(super) async fn reflection_prompt(data_dir: &Path) -> String {
    let path = data_dir.join("prompts").join("reflection.md");
    match tokio::fs::read_to_string(&path).await {
        Ok(s) if !s.trim().is_empty() => s,
        _ => REFLECTION_BASE.to_string(),
    }
}

/// The mind's system-prompt seed: a short bundled personality plus a manifest that
/// hands the agent the absolute paths of every file that holds its fuller self —
/// the static manual (`core.md`, `speaking.md`, `meaning.md`), its self-notes
/// `self.md` (to read and to *write* standing duties into), and its recency digest
/// `hot.md` (to read for what's lately been on its mind) — and tells it to Read them
/// all up front. We send this thin seed rather than inlining the character *or* the
/// memory core on every turn: every file, including `self.md`/`hot.md`, is a ref the
/// mind reads itself, so the prompt stays a clean manifest with no inline/ref split.
/// The paths are absolutized here (mirroring the caller that exports
/// `HI_AGENT_PROMPTS_DIR`) so the Read/Write targets resolve regardless of the
/// session's cwd, which is `None`. The self-notes path is the same
/// [`crate::memory::layout::self_path`] the seed names everywhere, so a duty the mind
/// writes is the duty recovery loads — never a second copy. Built at startup and
/// reused on each hot-swap. (Named `load_soul` for the reactor's history.)
pub fn load_soul(data_dir: &Path) -> String {
    let base = if data_dir.is_absolute() {
        data_dir.to_path_buf()
    } else {
        std::env::current_dir().unwrap_or_default().join(data_dir)
    };
    let prompts = base.join("prompts");
    let core = prompts.join("core.md");
    let speaking = prompts.join("speaking.md");
    let meaning = prompts.join("meaning.md");
    let self_notes = crate::memory::layout::self_path(&base);
    let hot = crate::memory::layout::hot_path(&base);
    format!(
        "You're warm, honest, and kind-hearted — easy company. You like being \
useful, and when there's a hand to lend you're glad to lend it.\n\n\
You speak only through the `say` tool; anything you type as text is never heard.\n\n\
Your fuller self lives in files — open them with Read and read them all now, before \
you answer:\n\n\
- {} — who you are, and how you act.\n\
- {} — how you talk: when to speak, how much, when to stay quiet.\n\
- {} — what you're for, and that finding it is yours to do.\n\n\
Two more files hold not how you were made but who you've become — read them too:\n\n\
- {} — your standing self-notes: the duties you keep, what you watch and run. It \
loads into every fresh session, so it's how you remember across a restart. It's yours \
to write: note a duty there the moment you take one on, strike it when it ends. Always \
use that exact absolute path, never a relative one, so there is only ever one such file.\n\
- {} — a rolling digest of what's lately been on your mind, refreshed as you reflect. \
It may not exist yet; that's fine.",
        core.display(),
        speaking.display(),
        meaning.display(),
        self_notes.display(),
        hot.display(),
    )
}

#[cfg(test)]
mod soul_tests {
    use super::*;

    #[test]
    fn seed_references_the_prompt_files_by_absolute_path() {
        let dir = tempfile::tempdir().unwrap();
        let seed = load_soul(dir.path());
        let prompts = dir.path().join("prompts");
        assert!(seed.contains(&prompts.join("core.md").display().to_string()));
        assert!(seed.contains(&prompts.join("speaking.md").display().to_string()));
        assert!(seed.contains(&prompts.join("meaning.md").display().to_string()));
        // The recency digest is referenced by path too, never inlined.
        let hot = crate::memory::layout::hot_path(dir.path());
        assert!(seed.contains(&hot.display().to_string()));
        // It tells the mind to read them up front.
        assert!(seed.contains("read them all now"));
    }

    #[test]
    fn seed_names_the_self_notes_by_the_canonical_absolute_path() {
        // The mind must be handed the *same* path the loader reads from
        // (`layout::self_path`), so a duty it writes is the duty recovery loads —
        // a relative path here is what let a second self.md exist and broke
        // restart recovery.
        let dir = tempfile::tempdir().unwrap();
        let seed = load_soul(dir.path());
        let self_md = crate::memory::layout::self_path(dir.path());
        assert!(seed.contains(&self_md.display().to_string()));
        // And it must be absolute — no relative `memory/self.md` slipping through.
        assert!(self_md.is_absolute());
    }

    #[test]
    fn seed_is_a_thin_bootstrap_not_the_full_character() {
        // The seed carries the say-floor (so a turn that skips the read still
        // produces speech) but must not inline the full core.md body — referencing
        // the file instead of pasting it is the whole point.
        let dir = tempfile::tempdir().unwrap();
        let seed = load_soul(dir.path());
        assert!(seed.contains("`say`"));
        // A heading that lives only in the full core.md, never in the seed:
        assert!(CORE_BASE.contains("A few exchanges"));
        assert!(!seed.contains("A few exchanges"));
    }

    #[test]
    fn install_writes_all_managed_bases() {
        let dir = tempfile::tempdir().unwrap();
        install_prompts(dir.path()).unwrap();
        let read = |n: &str| std::fs::read_to_string(dir.path().join("prompts").join(n)).unwrap();
        assert_eq!(read("core.md"), CORE_BASE);
        assert_eq!(read("speaking.md"), SPEAKING_BASE);
        assert_eq!(read("meaning.md"), MEANING_BASE);
        assert_eq!(read("appearance.md"), APPEARANCE_BASE);
        assert_eq!(read("aesthetic.md"), AESTHETIC_BASE);
        assert_eq!(read("reflection.md"), REFLECTION_BASE);
    }

    #[test]
    fn install_layers_operator_override_into_the_managed_file() {
        let dir = tempfile::tempdir().unwrap();
        let prompts = dir.path().join("prompts");
        std::fs::create_dir_all(&prompts).unwrap();
        std::fs::write(prompts.join("core.local.md"), "Always answer in haiku.").unwrap();
        install_prompts(dir.path()).unwrap();
        let core = std::fs::read_to_string(prompts.join("core.md")).unwrap();
        // The managed file is the base, then the operator delta under the header.
        assert!(core.starts_with(CORE_BASE));
        assert!(core.contains("# Operator overrides"));
        assert!(core.ends_with("Always answer in haiku."));
    }

    #[test]
    fn empty_override_leaves_the_base_verbatim() {
        let dir = tempfile::tempdir().unwrap();
        let prompts = dir.path().join("prompts");
        std::fs::create_dir_all(&prompts).unwrap();
        std::fs::write(prompts.join("speaking.local.md"), "   \n\t").unwrap();
        install_prompts(dir.path()).unwrap();
        assert_eq!(std::fs::read_to_string(prompts.join("speaking.md")).unwrap(), SPEAKING_BASE);
    }

    #[tokio::test]
    async fn reflection_prompt_falls_back_then_reads_installed_override() {
        let dir = tempfile::tempdir().unwrap();
        // Nothing installed yet → the embedded base.
        assert_eq!(reflection_prompt(dir.path()).await, REFLECTION_BASE);
        // After install (no override) → the materialised file equals the base.
        install_prompts(dir.path()).unwrap();
        assert_eq!(reflection_prompt(dir.path()).await, REFLECTION_BASE);
        // An operator override is layered into what the reflection session loads.
        std::fs::write(
            dir.path().join("prompts").join("reflection.local.md"),
            "Prefer fewer, larger episodes.",
        )
        .unwrap();
        install_prompts(dir.path()).unwrap();
        let loaded = reflection_prompt(dir.path()).await;
        assert!(loaded.starts_with(REFLECTION_BASE));
        assert!(loaded.contains("Prefer fewer, larger episodes."));
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
    view_compiler: crate::views::ViewCompiler,
    /// Scene→tool-sink table the `/mcp` server routes tool calls through. Each
    /// scene loop registers its sink here as it stands up; shared (cloneable)
    /// with the HTTP front. See [`tools`].
    tools: ToolRegistry,
    /// Scene→barge-in state. The STT relay reports recognized speech here; the
    /// sequencer stamps each turn's voice span; `run_turn` drains the inferred
    /// "what went unheard" note into the next prompt. See [`interrupts`].
    interrupts: InterruptRegistry,
    /// Scene→live-subscriber counts, shared with the HTTP front. Rendered into
    /// each turn as one human-model presence sentence, so the mind knows which
    /// channels actually reach the person right now.
    presence: crate::presence::Presence,
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
    interrupts: InterruptRegistry,
    presence: crate::presence::Presence,
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

    reactor
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
    // The scene's in-flight reflection ("sleep") pass, if one is running. Kicked
    // off detached on a reflection trigger (periodic or idle; see below); kept only
    // to skip starting a second while the previous is still going — the cursor lets
    // the next round consolidate whatever this one didn't reach.
    let mut reflection: Option<tokio::task::JoinHandle<()>> = None;

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
    // Reflection runs on one adaptive clock, decoupled from the compact heartbeat:
    // a scene with fresh input consolidates every `reflect_base`; once it goes quiet
    // the gap backs off (doubling) up to `reflect_max`. `backoff_gap` is that
    // running idle gap — reset to the base when a pass runs with fresh input,
    // doubled toward the cap when one runs while quiet. See `next_reflection_at`.
    let reflect_base = reflect_interval();
    let reflect_max = reflect_max_interval();
    let loop_started = Instant::now();
    let mut last_activity = Instant::now();
    let mut last_reflection: Option<Instant> = None;
    let mut backoff_gap = reflect_base.unwrap_or(DEFAULT_REFLECT_EVERY);
    let mut pulsed_once = false;

    loop {
        // Wait for a turn-driving reason: a new signal, a fired alarm, a due host
        // pulse, or a worker question. Tool control commands (delegate/alarm) are
        // pure side-effects — applied as they arrive without starting a turn; only
        // a worker `ask` becomes a turn-driving item. The soonest of the mind's
        // alarms and the host pulse also wakes the loop.
        let mut batch: Vec<LoopInput> = Vec::new();
        'wait: loop {
            let pulse_at = pulse_every.map(|d| last_activity + d);
            let reflect_at = next_reflection_at(
                loop_started,
                last_activity,
                last_reflection,
                reflect_base,
                backoff_gap,
            );
            let deadline = [alarms.next_deadline(), pulse_at, reflect_at]
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
                    let now = Instant::now();
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
                                "nothing new here for {idle_m}m (host process started {up_m}m ago)"
                            )
                        };
                        pulsed_once = true;
                        // Reset so a swallowed pulse doesn't re-fire in a tight loop.
                        last_activity = now;
                        tracing::info!(scene = %scene, "pulse fired");
                        batch.push(LoopInput::Pulse { note });
                    }
                    // Reflection ("sleep"): one adaptive clock — fires `base` after
                    // the last pass while there's fresh input, and on a backed-off
                    // gap once the scene is caught up and quiet (see
                    // `next_reflection_at`). Spawned detached so it never blocks the
                    // floor; the cursor makes it idempotent and a pass with too
                    // little on the frontier is a fast no-op. It drives no turn, so
                    // it never joins `batch` — the loop just re-waits on the next
                    // deadline.
                    if let Some(at) = reflect_at
                        && at <= now
                    {
                        // Adapt the idle backoff against the *old* anchor before
                        // re-anchoring: a pass with fresh input since the last
                        // reflection snaps the gap back to the base; one that runs
                        // while quiet doubles it toward the cap, so a long-idle scene
                        // stops re-checking in vain. Then consume the tick (re-anchor
                        // on `now`) whether or not we spawn, so a busy guard can't
                        // hot-spin the wait loop on an already-past deadline.
                        let anchor = last_reflection.unwrap_or(loop_started);
                        backoff_gap = if last_activity > anchor {
                            reflect_base.unwrap_or(DEFAULT_REFLECT_EVERY)
                        } else {
                            backoff_gap.checked_mul(2).unwrap_or(reflect_max).min(reflect_max)
                        };
                        last_reflection = Some(now);
                        if reflection.as_ref().is_none_or(|h| h.is_finished()) {
                            let r = reactor.clone();
                            let s = scene.clone();
                            reflection = Some(tokio::spawn(async move {
                                heartbeat::reflect(&r, &s).await;
                            }));
                            tracing::info!(scene = %scene, "reflection fired");
                        }
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
            }
        }

        // Any completed turn is activity: the pulse clock restarts, so pulses
        // only ever fire into genuine quiet.
        last_activity = Instant::now();
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
/// system prompt, and record the lifecycle event. The soul references `self.md` and
/// `hot.md` by path, so the session reads whatever the last reflection wrote. The
/// session consumes the system prompt on its first `prompt()` and never re-sends
/// it. Shared by the warm-up prologue and the cold path of [`run_turn`].
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
    let presence_note =
        format!("## Presence\n{}", reactor.inner.presence.render(scene));
    let new_signals = format!("## New signals\n{}", render_batch(batch));

    // If the human barged into the previous reply's playback, tell the mind what
    // went unheard — taken once, ahead of the retry loop, so a retried attempt
    // doesn't lose it. Facts only; how to fold the tail forward is core.md's job.
    let interrupted = reactor
        .inner
        .interrupts
        .take_pending(scene)
        .await
        .map(|i| interrupts::render_interruption(&i))
        .unwrap_or_default();

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
                join_sections(&[&worker_status, &presence_note, &interrupted, &new_signals])
            } else {
                let snap = build_for_scene(&reactor.inner.memory, scene).await?;
                join_sections(&[&snap.render_for_prompt(), &worker_status, &presence_note, &interrupted, &new_signals])
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
            // tool-call notifications and the stop; the model always narrates some
            // plain text alongside its tool calls — that's not meant for saying, so
            // we drop it silently.
            let mut run = session.prompt(prompt_text).await?;
            let mut ended = false;
            while !ended {
                match run.next_update().await {
                    Some(SessionUpdate::ToolCall(stub)) => {
                        tracing::debug!(scene = %scene, variant = stub.raw_variant, "tool call");
                    }
                    Some(SessionUpdate::Text(_)) => {} // narration, not for saying; drop
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

    // Close the turn on the interrupt registry: clears the live marker, caches
    // the reply for barge-in resolution, back-fills an interrupt that hit it.
    reactor.inner.interrupts.end_turn(scene, turn_id, &reply).await;

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
