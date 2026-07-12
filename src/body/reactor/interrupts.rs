//! Voice barge-in: notice — without being told — that the human talked over
//! your voice, and remember what they didn't hear.
//!
//! There is no interrupt signal anywhere on the wire, and the mind is never
//! cancelled — fix-forward holds with no exceptions. The client ducks its own
//! speaker reflexively when speech is recognized (it watches the `final:false`
//! partials on its observe stream); the human's words then buffer and fold into
//! the next turn like any other signal. What this module adds is the speaker's
//! *self-knowledge*: a human knows "I'd been talking about ten seconds when she
//! cut in" from their own internal clock, not from a receipt. Same here — the
//! backend knows when a turn's voice started sounding and roughly how long the
//! reply takes to say, so when recognized speech arrives mid-sound it records a
//! pending note: which turn was cut, about how far in, and what the full reply
//! had been. The next turn's prompt carries that note as plain fact; how to fold
//! the unheard tail forward (drop, revise, mention) is the soul's judgment, not
//! the mechanism's (see `speaking.md`). The same barge-in also marks the cut turn
//! for flush ([`InterruptRegistry::should_skip`]) so the output sequencer stops
//! speaking and typing its unheard tail rather than draining it over the human.
//!
//! Everything here is estimate-grade on purpose: playback truth lives only in
//! the client, and we deliberately don't ask for it. The note says "about Ns
//! in" and hands over the full text — the mind judges, the way a person isn't
//! quite sure you caught their last sentence.

use std::collections::{HashMap, VecDeque};
use std::sync::Arc;
use std::time::Duration;

use tokio::sync::Mutex;
use tokio::time::Instant;

use crate::types::Scene;

/// How many finished turns' replies a scene retains for note resolution.
/// Playback lags synthesis by at most a reply or so; a barged turn is almost
/// always the latest finished one.
const RECENT_REPLIES: usize = 2;

/// Rough speaking rate for estimating how long a reply takes to sound —
/// Mandarin TTS runs ~5 chars/s and that's the dominant register here; mixed
/// or Latin text says more per char, which only makes the estimate generous
/// (we'd over-believe "still sounding", never under).
const SPEECH_MS_PER_CHAR: u64 = 200;

/// Playback starts a beat after synthesis begins (network + decode), so the
/// heard-portion estimate is shifted back by this much.
const PLAYBACK_START_LATENCY: Duration = Duration::from_millis(400);

/// Grace past the estimated reply duration during which incoming speech still
/// counts as a barge-in rather than a normal post-listen reply.
const STILL_SOUNDING_SLACK: Duration = Duration::from_secs(2);

/// One inferred barge-in, held until the next turn folds it into its prompt.
#[derive(Debug)]
pub struct Interruption {
    pub turn: u64,
    /// Estimated portion of the reply that had sounded when they broke in.
    pub heard_ms: u64,
    /// The cut turn's full spoken reply, when known. `None` while the turn is
    /// still in progress (back-filled by [`InterruptRegistry::end_turn`]).
    pub reply: Option<String>,
}

#[derive(Default)]
struct SceneState {
    /// The scene's latest voice span: which turn, and when its audio started
    /// flowing. Stamped by the sequencer when it opens a turn's TTS.
    audio: Option<(u64, Instant)>,
    /// Last few finished turns' replies, newest last: `(turn, reply)`.
    recent: VecDeque<(u64, String)>,
    /// The unconsumed barge-in note, if any. First wins; duplicates are noise.
    pending: Option<Interruption>,
    /// Turn marked for flush by a barge-in: the sequencer drops this turn's
    /// remaining `say`/`show` output so the unheard tail isn't spoken or typed
    /// out. Monotonic turn ids mean a stale value never matches a later turn, so
    /// it's simply overwritten by the next barge-in — no explicit reset.
    flush_turn: Option<u64>,
}

/// Shared scene→barge-in state. Created once in `lib.rs`, cloned into the HTTP
/// front (whose STT relay reports recognized speech) and the reactor (whose
/// sequencer stamps voice spans and whose turns drain pending notes).
#[derive(Clone, Default)]
pub struct InterruptRegistry {
    inner: Arc<Mutex<HashMap<Scene, SceneState>>>,
}

impl InterruptRegistry {
    pub fn new() -> Self {
        Self::default()
    }

    /// A turn's voice just started flowing. Called by the sequencer as it opens
    /// the turn's TTS span; the latest span wins.
    pub async fn audio_began(&self, scene: &Scene, turn: u64, now: Instant) {
        let mut inner = self.inner.lock().await;
        inner.entry(scene.clone()).or_default().audio = Some((turn, now));
    }

    /// The turn closed with `reply` as its full spoken text. Caches the reply
    /// for note resolution and back-fills a pending note that cut into this
    /// very turn.
    pub async fn end_turn(&self, scene: &Scene, turn: u64, reply: &str) {
        let mut inner = self.inner.lock().await;
        let state = inner.entry(scene.clone()).or_default();
        let reply = reply.trim();
        if reply.is_empty() {
            return; // a silent turn can't be barged into
        }
        if let Some(p) = state.pending.as_mut() {
            if p.turn == turn && p.reply.is_none() {
                p.reply = Some(reply.to_owned());
            }
        }
        state.recent.push_back((turn, reply.to_owned()));
        while state.recent.len() > RECENT_REPLIES {
            state.recent.pop_front();
        }
    }

    /// Recognized human speech just arrived on this scene (any rolling
    /// partial). If our own clock says the last reply is probably still
    /// sounding, record the barge-in note; otherwise this is a normal
    /// post-listen reply and nothing is recorded. Cheap and idempotent —
    /// called for every partial, only the first mid-sound one lands.
    pub async fn note_speech(&self, scene: &Scene, now: Instant) {
        let mut inner = self.inner.lock().await;
        let Some(state) = inner.get_mut(scene) else { return };
        if state.pending.is_some() {
            return; // first barge-in wins
        }
        let Some((turn, began)) = state.audio else { return };
        let elapsed = now.saturating_duration_since(began);

        let reply = state.recent.iter().find(|(t, _)| *t == turn).map(|(_, r)| r.clone());
        let est = reply.as_deref().map(estimated_speech_duration);
        // A turn with no cached reply hasn't closed yet — it is mid-speech by
        // definition. A closed turn is "still sounding" while our clock sits
        // inside its estimated spoken length (plus slack).
        let still_sounding = match est {
            None => true,
            Some(d) => elapsed < d + STILL_SOUNDING_SLACK,
        };
        if !still_sounding {
            return;
        }

        let mut heard = elapsed.saturating_sub(PLAYBACK_START_LATENCY);
        if let Some(d) = est {
            heard = heard.min(d);
        }
        state.flush_turn = Some(turn);
        state.pending = Some(Interruption { turn, heard_ms: heard.as_millis() as u64, reply });
        tracing::info!(scene = %scene, turn, heard_ms = heard.as_millis() as u64, "barge-in inferred (speech while voice sounding)");
    }

    /// Mark `turn` for flush directly, without an audio span. Used when the mind
    /// is reorganized mid-turn (new human input lands while the prompt is still in
    /// flight): in the thinking phase no TTS has started, so `note_speech` never
    /// fired and nothing marked the turn — but we still want the sequencer to drop
    /// any say/show beats this now-abandoned turn emits before it's cancelled.
    /// `flush_turn` is monotonic-per-turn, so this never collides with a later
    /// reorganized pass's id (self-clearing, no reset).
    pub async fn mark_flush(&self, scene: &Scene, turn: u64) {
        self.inner.lock().await.entry(scene.clone()).or_default().flush_turn = Some(turn);
    }

    /// Take (and clear) the scene's pending note, for the next prompt.
    pub async fn take_pending(&self, scene: &Scene) -> Option<Interruption> {
        self.inner.lock().await.get_mut(scene)?.pending.take()
    }

    /// Whether the sequencer should abandon `turn`'s remaining output — a
    /// barge-in landed on it. Unlike [`take_pending`] (consumed once by the next
    /// prompt), this stays set so every trailing beat of the cut turn is skipped.
    pub async fn should_skip(&self, scene: &Scene, turn: u64) -> bool {
        let inner = self.inner.lock().await;
        inner.get(scene).and_then(|s| s.flush_turn) == Some(turn)
    }
}

/// Rough wall-clock length of `reply` spoken aloud. Estimate-grade by design.
fn estimated_speech_duration(reply: &str) -> Duration {
    Duration::from_millis(reply.chars().count() as u64 * SPEECH_MS_PER_CHAR)
}

/// Render a pending note as a prompt section. Facts only — how to fold the
/// unheard tail forward is the soul's guidance, not the mechanism's.
pub(super) fn render_interruption(i: &Interruption) -> String {
    let secs = (i.heard_ms as f64 / 1000.0).round() as u64;
    match &i.reply {
        Some(reply) => format!(
            "## Interrupted\nThey started speaking about {secs}s into your last reply, and your \
             voice cut out there. You had been saying: \"{reply}\" — assume what came after \
             roughly that point went unheard."
        ),
        None => format!(
            "## Interrupted\nThey started speaking about {secs}s into your last reply, and your \
             voice cut out there; the rest of what you were saying went unheard."
        ),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn scene() -> Scene {
        Scene("test".to_string())
    }

    fn secs(s: u64) -> Duration {
        Duration::from_secs(s)
    }

    #[tokio::test]
    async fn speech_mid_reply_records_a_note_with_text() {
        let reg = InterruptRegistry::new();
        let t0 = Instant::now();
        reg.audio_began(&scene(), 7, t0).await;
        // 100 chars ≈ 20s spoken; turn closed (synthesis done) while audio plays on.
        reg.end_turn(&scene(), 7, &"字".repeat(100)).await;
        reg.note_speech(&scene(), t0 + secs(5)).await;
        let p = reg.take_pending(&scene()).await.expect("note recorded");
        assert_eq!(p.turn, 7);
        assert!(p.reply.is_some());
        // ~5s elapsed minus the playback-start beat.
        assert!((4_000..=5_000).contains(&p.heard_ms), "heard_ms = {}", p.heard_ms);
        assert!(reg.take_pending(&scene()).await.is_none(), "take drains");
    }

    #[tokio::test]
    async fn speech_after_reply_finished_is_not_a_barge_in() {
        let reg = InterruptRegistry::new();
        let t0 = Instant::now();
        reg.audio_began(&scene(), 7, t0).await;
        reg.end_turn(&scene(), 7, &"字".repeat(10)).await; // ≈2s spoken
        reg.note_speech(&scene(), t0 + secs(10)).await; // long after it finished
        assert!(reg.take_pending(&scene()).await.is_none());
    }

    #[tokio::test]
    async fn turn_still_in_progress_counts_as_sounding_and_backfills() {
        let reg = InterruptRegistry::new();
        let t0 = Instant::now();
        reg.audio_began(&scene(), 3, t0).await;
        // No end_turn yet — mid-generation. Speech arrives:
        reg.note_speech(&scene(), t0 + secs(2)).await;
        // The turn then closes with its full reply.
        reg.end_turn(&scene(), 3, "what was said in full").await;
        let p = reg.take_pending(&scene()).await.expect("note recorded");
        assert_eq!(p.turn, 3);
        assert_eq!(p.reply.as_deref(), Some("what was said in full"));
    }

    #[tokio::test]
    async fn first_barge_in_wins() {
        let reg = InterruptRegistry::new();
        let t0 = Instant::now();
        reg.audio_began(&scene(), 1, t0).await;
        reg.note_speech(&scene(), t0 + secs(1)).await;
        reg.note_speech(&scene(), t0 + secs(3)).await; // later partials are noise
        let p = reg.take_pending(&scene()).await.expect("note recorded");
        assert!(p.heard_ms <= 1_000, "heard_ms = {}", p.heard_ms);
    }

    #[tokio::test]
    async fn speech_with_no_voice_span_records_nothing() {
        let reg = InterruptRegistry::new();
        reg.note_speech(&scene(), Instant::now()).await;
        assert!(reg.take_pending(&scene()).await.is_none());
    }

    #[tokio::test]
    async fn heard_estimate_is_capped_at_reply_length() {
        let reg = InterruptRegistry::new();
        let t0 = Instant::now();
        reg.audio_began(&scene(), 5, t0).await;
        reg.end_turn(&scene(), 5, &"字".repeat(10)).await; // ≈2s spoken
        // Speech lands inside the slack window, past the estimated end.
        reg.note_speech(&scene(), t0 + secs(3)).await;
        let p = reg.take_pending(&scene()).await.expect("note recorded");
        assert!(p.heard_ms <= 2_000, "heard_ms = {}", p.heard_ms);
    }

    #[tokio::test]
    async fn barge_in_marks_its_turn_for_skip() {
        let reg = InterruptRegistry::new();
        let t0 = Instant::now();
        reg.audio_began(&scene(), 4, t0).await;
        reg.note_speech(&scene(), t0 + secs(1)).await;
        assert!(reg.should_skip(&scene(), 4).await);
        // A later turn is unaffected — monotonic ids never collide with a stale flag.
        assert!(!reg.should_skip(&scene(), 5).await);
        // Draining the note for the prompt does not un-flush the turn.
        let _ = reg.take_pending(&scene()).await;
        assert!(reg.should_skip(&scene(), 4).await);
    }

    #[tokio::test]
    async fn no_barge_in_no_skip() {
        let reg = InterruptRegistry::new();
        let t0 = Instant::now();
        reg.audio_began(&scene(), 4, t0).await;
        assert!(!reg.should_skip(&scene(), 4).await);
    }

    #[tokio::test]
    async fn mark_flush_marks_turn_for_skip_without_an_audio_span() {
        let reg = InterruptRegistry::new();
        // No audio_began / note_speech — the thinking-phase reorg case.
        reg.mark_flush(&scene(), 9).await;
        assert!(reg.should_skip(&scene(), 9).await);
        // A later (reorganized) pass with a fresh id is unaffected.
        assert!(!reg.should_skip(&scene(), 10).await);
    }
}
