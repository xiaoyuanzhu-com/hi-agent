//! Segmentation — aggregating a continuous signal into the coarser grain a
//! downstream consumer accepts. One mechanism, swappable cut policy.
//!
//! # One aggregator, two policies
//!
//! Both places we segment are the *same* operation: take a fine, continuous
//! stream and cut it into the coarser units the next stage wants. They differ
//! only in *where to cut* and in how the input behaves:
//!
//! - **[`Speech`]** — STT word-stream → sentences for the agent. The upstream
//!   revises its tail (a rolling partial), so the cut must balance punctuation,
//!   size, and time, and must not guillotine a partial before its period lands.
//! - **[`Terminator`]** — the agent's reply stream → sentences for TTS. The
//!   source is append-only (an LLM never un-says a token), so the cut is purely
//!   structural: a sentence terminator, mirroring the frontend `sentences.ts`.
//!
//! The buffer machinery is shared; the [`CutPolicy`] supplies the boundary rule.
//!
//! # The buffer model
//!
//! The stream is held in two parts and one pointer:
//!
//! ```text
//!   locked            partial
//!   ┌──────────────┐  ┌─────────────┐
//!   │ committed,    │  │ in-progress, │     full = locked + partial
//!   │ never revised │  │ may change   │
//!   └──────────────┴──┴─────────────┘
//!                  ▲dispatched (chars already emitted)
//!                  └──────── tail (undispatched) ────────┘
//! ```
//!
//! - `observe(text, is_final=false)` replaces `partial` with the latest rolling
//!   text (a revisable source may rewrite the tail of an utterance).
//! - `observe(text, is_final=true)` — also reachable as [`Segmenter::commit`] —
//!   appends finalized text to `locked` and clears `partial`. Locked text is
//!   stable. Append-only sources only ever commit, so `partial` stays empty and
//!   the time-based rules never fire.
//! - A cut advances `dispatched` past the emitted prefix; whatever follows stays
//!   buffered for the next segment. Incomplete trailing words are never flushed
//!   just because an earlier part of the buffer was emitted.
//!
//! Time is injected into every method so a policy that uses it is
//! deterministically unit-testable with a synthetic clock. Policies that ignore
//! time (e.g. [`Terminator`]) are unaffected by the clock entirely.

use std::time::{Duration, Instant};

// -----------------------------------------------------------------------------
// CutPolicy — where to cut the tail
// -----------------------------------------------------------------------------

/// The undispatched tail and the facts a policy needs to judge a cut. `tail` is
/// already split into chars (all boundaries are char-aligned); a policy returns
/// the char count to emit, or `None` to keep waiting.
pub struct Cut<'a> {
    /// The undispatched suffix, char-aligned.
    pub tail: &'a [char],
    /// How many leading chars of `tail` are committed (definite) text, which can
    /// never be revised — punctuation there is settled the instant it is seen.
    pub committed: usize,
    /// How long the tail has been unchanged.
    pub since_change: Duration,
    /// How long the current undispatched segment has been accumulating.
    pub since_start: Duration,
}

/// Decides where (if anywhere) to cut the current tail into a finished unit.
pub trait CutPolicy {
    fn boundary(&self, cut: &Cut) -> Option<usize>;
}

// -----------------------------------------------------------------------------
// Shared punctuation helpers
// -----------------------------------------------------------------------------

fn is_sentence_end(c: char) -> bool {
    matches!(c, '。' | '！' | '？' | '!' | '?' | '.' | '…')
}

/// Clause-level marks — not sentence ends, but safe places to break a run-on so
/// a forced cut lands on a phrase boundary instead of mid-word.
fn is_clause_boundary(c: char) -> bool {
    matches!(c, '，' | '、' | '；' | '：' | ',' | ';' | ':')
}

/// When a size/time guard forces a cut, snap back to the last punctuation
/// (sentence end or clause mark) before `hard`, emitting up to there and leaving
/// the incomplete remainder buffered. Falls back to `hard` only when the tail
/// has no punctuation at all to break on.
fn snap_back(chars: &[char], hard: usize) -> usize {
    (0..hard)
        .rev()
        .find(|&i| is_sentence_end(chars[i]) || is_clause_boundary(chars[i]))
        .map(|i| i + 1)
        .unwrap_or(hard)
}

// -----------------------------------------------------------------------------
// Speech — STT word-stream → sentences (revisable source, time-aware)
// -----------------------------------------------------------------------------

/// Tunable thresholds for the [`Speech`] cut levers. Defaults target
/// conversational Mandarin/English speech: punctuation carries most cuts; size
/// and time are guards for the run-on / monologue / trailing-fragment cases.
#[derive(Debug, Clone, Copy)]
pub struct SegmenterConfig {
    /// Hard cap on undispatched chars before a forced cut (run-on guard).
    pub max_chars: usize,
    /// Tail must sit unchanged this long before a punctuation cut is trusted —
    /// keeps us from cutting on a mark that the upstream is still revising.
    pub punct_stable: Duration,
    /// A *committed* (definite) fragment with no terminal punctuation, unchanged
    /// this long, is flushed anyway (e.g. a bare "嗯"). Only applies to locked
    /// text — never to a revisable partial — so it can't split a sentence from
    /// its trailing period.
    pub stable: Duration,
    /// A segment older than this is force-cut even mid-sentence (monologue
    /// guard; restores the cap the old client VAD used to enforce).
    pub max_segment: Duration,
}

impl Default for SegmenterConfig {
    fn default() -> Self {
        Self {
            max_chars: 64,
            punct_stable: Duration::from_millis(300),
            stable: Duration::from_millis(500),
            max_segment: Duration::from_secs(10),
        }
    }
}

/// Cut policy for a revisable speech transcript. We deliberately do NOT cut on
/// the upstream's silence/`definite` signal: it is laggy and erratic. The cut
/// policy lives here instead, explicit and tunable. The upstream's `definite`
/// flag is still read (it commits stable text), but never by itself ends a
/// sentence.
///
/// The rules, in priority order over the tail:
///
/// 1. **Punctuation** — cut just after the first sentence-ending mark
///    (`。！？!?.…`), once it is *settled*: it sits in committed text, OR more
///    text already follows it, OR the tail has been unchanged for `punct_stable`.
///    The low-latency common case. The settle check stops us splitting "你好。"
///    into "你好" + "。" when the upstream appends the period a beat after the words.
/// 2. **Size** (guard) — once the tail reaches `max_chars`, force a cut, but
///    `snap_back` to the last punctuation/clause mark so we emit a clean clause
///    and keep the remainder buffered (only chop mid-word with no punctuation).
/// 3. **Stability** (guard) — a *committed* fragment with no terminal punctuation
///    (e.g. a bare "嗯") unchanged for `stable` is flushed whole. Restricted to
///    fully-locked text so it can never guillotine a still-revisable partial.
/// 4. **Max-segment age** (guard) — a segment older than `max_segment` is
///    force-cut even mid-sentence (snapping like rule 2).
#[derive(Debug, Clone, Copy, Default)]
pub struct Speech {
    cfg: SegmenterConfig,
}

impl Speech {
    pub fn new(cfg: SegmenterConfig) -> Self {
        Self { cfg }
    }
}

impl CutPolicy for Speech {
    fn boundary(&self, cut: &Cut) -> Option<usize> {
        let chars = cut.tail;
        let n = chars.len();
        let committed = cut.committed;

        // 1. Punctuation — cut just after the first sentence-ending mark, once
        //    it's settled: it sits in committed text, OR more text already
        //    follows it, OR the (revisable) tail has been stable long enough
        //    that the mark won't move.
        if let Some(i) = chars.iter().position(|&c| is_sentence_end(c)) {
            let settled = i < committed
                || chars[i + 1..].iter().any(|c| !c.is_whitespace())
                || cut.since_change >= self.cfg.punct_stable;
            if settled {
                return Some(i + 1);
            }
        }

        // 2. Size — run-on guard. Snap back to the last phrase boundary so we
        //    emit a clean clause and keep the rest buffered, e.g.
        //    "…明天的天气。来决定" → emit "…明天的天气。", buffer "来决定".
        if n >= self.cfg.max_chars {
            return Some(snap_back(chars, n));
        }

        // 3. Stability — a *committed* fragment with no terminal punctuation that
        //    has gone quiet (e.g. "嗯", "对"). We require the whole tail to be
        //    locked: a still-revisable partial must NOT be flushed here, or we'd
        //    guillotine a sentence's words a beat before the upstream appends its
        //    period — splitting "你好。" into "你好" + "。". A committed fragment is a
        //    finished unit, so we emit it whole rather than snapping.
        if committed >= n && cut.since_change >= self.cfg.stable {
            return Some(n);
        }

        // 4. Max segment age — force-cut a non-stop monologue, snapping to the
        //    last phrase boundary and buffering the remainder.
        if cut.since_start >= self.cfg.max_segment {
            return Some(snap_back(chars, n));
        }

        None
    }
}

// -----------------------------------------------------------------------------
// Terminator — append-only reply stream → sentences (structural, time-free)
// -----------------------------------------------------------------------------

/// Cut policy for an append-only token stream (the agent's reply → TTS),
/// mirroring the frontend `sentences.ts`: CJK terminators (。！？) cut
/// immediately; Latin terminators (.!?…) cut only when followed by whitespace,
/// so decimals and abbreviations aren't broken. A terminator at the very end of
/// the tail waits for more text (or [`Segmenter::flush`]). Time is ignored.
///
/// This MUST stay in agreement with the frontend splitter: the spec requires the
/// text-fade and the TTS to cut at the same places.
#[derive(Debug, Clone, Copy, Default)]
pub struct Terminator;

impl CutPolicy for Terminator {
    fn boundary(&self, cut: &Cut) -> Option<usize> {
        let chars = cut.tail;
        for (i, &c) in chars.iter().enumerate() {
            if matches!(c, '。' | '！' | '？') {
                return Some(i + 1);
            }
            if matches!(c, '.' | '!' | '?' | '…') {
                if let Some(&next) = chars.get(i + 1) {
                    if next.is_whitespace() {
                        return Some(i + 1);
                    }
                }
            }
        }
        None
    }
}

// -----------------------------------------------------------------------------
// Segmenter — the shared buffer machinery
// -----------------------------------------------------------------------------

/// Stateful segmenter. Feed it rolling stream updates; it returns completed
/// units to dispatch, cut by its [`CutPolicy`]. Time is injected so a
/// time-aware policy is deterministically testable.
pub struct Segmenter<P> {
    policy: P,
    /// Finalized (definite) text — stable, never revised.
    locked: String,
    /// Current in-progress text — may still be revised by the source.
    partial: String,
    /// Chars of `locked + partial` already emitted.
    dispatched: usize,
    /// When the current undispatched segment started accumulating.
    seg_start: Instant,
    /// When the undispatched tail last changed.
    last_change: Instant,
    /// Snapshot of the tail at `last_change`, to detect changes.
    last_tail: String,
}

impl<P: CutPolicy> Segmenter<P> {
    pub fn new(policy: P, now: Instant) -> Self {
        Self {
            policy,
            locked: String::new(),
            partial: String::new(),
            dispatched: 0,
            seg_start: now,
            last_change: now,
            last_tail: String::new(),
        }
    }

    /// Apply a stream update. `is_final` commits the text into the stable prefix;
    /// it does not itself cause a cut. Returns any units completed by this update.
    pub fn observe(&mut self, text: &str, is_final: bool, now: Instant) -> Vec<String> {
        if is_final {
            self.locked.push_str(text);
            self.partial.clear();
        } else {
            self.partial = text.to_string();
        }
        self.cut(now)
    }

    /// Append finalized text — for sources that never revise their tail (e.g. an
    /// LLM token stream). Sugar for `observe(text, is_final = true, now)`.
    pub fn commit(&mut self, text: &str, now: Instant) -> Vec<String> {
        self.observe(text, true, now)
    }

    /// Time-driven check with no new text — drives the stability and max-segment
    /// cuts when the source has gone quiet. Call on a periodic tick.
    pub fn tick(&mut self, now: Instant) -> Vec<String> {
        self.cut(now)
    }

    /// Flush whatever undispatched text remains as a final unit (stream end).
    pub fn flush(&mut self) -> Option<String> {
        let tail = self.tail();
        let seg = tail.trim().to_string();
        if seg.is_empty() {
            return None;
        }
        self.dispatched += tail.chars().count();
        Some(seg)
    }

    /// The undispatched suffix of `locked + partial`.
    fn tail(&self) -> String {
        self.locked
            .chars()
            .chain(self.partial.chars())
            .skip(self.dispatched)
            .collect()
    }

    fn cut(&mut self, now: Instant) -> Vec<String> {
        let mut out = Vec::new();
        loop {
            let tail = self.tail();
            if tail.trim().is_empty() {
                // Nothing pending; reset the segment clock so the next unit is
                // timed from when it actually begins.
                if tail != self.last_tail {
                    self.last_tail = tail;
                    self.seg_start = now;
                    self.last_change = now;
                }
                break;
            }
            if tail != self.last_tail {
                if self.last_tail.trim().is_empty() {
                    self.seg_start = now; // a fresh segment just began
                }
                self.last_tail = tail.clone();
                self.last_change = now;
            }

            let chars: Vec<char> = tail.chars().collect();
            let ctx = Cut {
                committed: self.locked.chars().count().saturating_sub(self.dispatched),
                since_change: now.duration_since(self.last_change),
                since_start: now.duration_since(self.seg_start),
                tail: &chars,
            };
            match self.policy.boundary(&ctx) {
                Some(b) if b > 0 => {
                    let seg: String = chars.iter().take(b).collect();
                    let seg = seg.trim().to_string();
                    self.dispatched += b;
                    self.seg_start = now;
                    self.last_change = now;
                    self.last_tail = self.tail();
                    if !seg.is_empty() {
                        out.push(seg);
                    }
                    // Loop again: a backlog may hold more than one unit.
                }
                _ => break,
            }
        }
        out
    }
}

// -----------------------------------------------------------------------------
// Tests
// -----------------------------------------------------------------------------

#[cfg(test)]
mod speech_tests {
    use super::*;

    fn seg() -> (Segmenter<Speech>, Instant) {
        let t0 = Instant::now();
        (Segmenter::new(Speech::default(), t0), t0)
    }

    #[test]
    fn cuts_on_sentence_punctuation() {
        let (mut s, t0) = seg();
        // Rolling partial gains a sentence mark with more text after it → settled.
        assert!(s.observe("你好", false, t0).is_empty());
        let out = s.observe("你好。在", false, t0 + Duration::from_millis(100));
        assert_eq!(out, vec!["你好。"]);
        // The remainder stays pending until it too completes.
        assert!(s.observe("你好。在", false, t0 + Duration::from_millis(150)).is_empty());
    }

    #[test]
    fn trailing_punctuation_waits_for_stability_then_cuts() {
        let (mut s, t0) = seg();
        // Mark is at the very end with nothing after → not settled yet.
        assert!(s.observe("好的。", false, t0).is_empty());
        // After punct_stable with no change, it cuts.
        let out = s.tick(t0 + Duration::from_millis(350));
        assert_eq!(out, vec!["好的。"]);
    }

    #[test]
    fn size_guard_cuts_runon_without_punctuation() {
        let (mut s, t0) = seg();
        let long: String = "字".repeat(70); // exceeds max_chars (64), no punctuation
        let out = s.observe(&long, false, t0);
        assert_eq!(out.len(), 1);
        assert_eq!(out[0].chars().count(), 70); // nothing to snap to → chop whole
    }

    #[test]
    fn emits_complete_sentence_and_buffers_remainder() {
        // The user's example: a complete sentence followed by incomplete words.
        let (mut s, t0) = seg();
        let out = s.observe("你好，我想看一下明天的天气。来决定", false, t0);
        assert_eq!(out, vec!["你好，我想看一下明天的天气。"]);
        // "来决定" stays buffered, then completes on its own later.
        assert!(s.observe("你好，我想看一下明天的天气。来决定", false, t0 + Duration::from_millis(50)).is_empty());
        // It grows into its own sentence; the trailing 。 settles after punct_stable.
        assert!(s
            .observe("你好，我想看一下明天的天气。来决定看天气。", false, t0 + Duration::from_millis(100))
            .is_empty());
        let out = s.tick(t0 + Duration::from_millis(450));
        assert_eq!(out, vec!["来决定看天气。"]);
    }

    #[test]
    fn size_force_cut_snaps_to_last_clause_and_buffers() {
        // A long comma-run-on with no sentence end: the size guard snaps to the
        // last clause boundary and keeps the tail buffered (not a mid-word chop).
        let (mut s, t0) = seg();
        let text = format!("{}，{}", "啊".repeat(30), "哦".repeat(40)); // 71 chars
        let out = s.observe(&text, false, t0);
        assert_eq!(out.len(), 1);
        assert!(out[0].ends_with('，'));
        assert_eq!(out[0].chars().count(), 31); // 30 + the comma
        // The 40-char remainder is buffered, not emitted.
        assert!(s.observe(&text, false, t0 + Duration::from_millis(50)).is_empty());
    }

    #[test]
    fn stability_flushes_committed_punctuationless_fragment() {
        let (mut s, t0) = seg();
        // Committed (definite) fragment with no punctuation.
        assert!(s.observe("嗯对", true, t0).is_empty());
        // No change for `stable` → flush the fragment.
        let out = s.tick(t0 + Duration::from_millis(500));
        assert_eq!(out, vec!["嗯对"]);
    }

    #[test]
    fn partial_words_are_not_split_from_their_late_period() {
        let (mut s, t0) = seg();
        // Upstream recognizes the words first (revisable partial)...
        assert!(s.observe("你好", false, t0).is_empty());
        // ...and even after a long quiet, a partial is NOT flushed (no premature
        // "你好" cut), because its period may still be coming.
        assert!(s.tick(t0 + Duration::from_millis(1500)).is_empty());
        // The period arrives and the upstream commits it → one clean cut.
        let out = s.observe("你好。", true, t0 + Duration::from_millis(1600));
        assert_eq!(out, vec!["你好。"]);
    }

    #[test]
    fn max_segment_force_cuts_monologue() {
        let (mut s, t0) = seg();
        // Keep changing the tail so stability never triggers.
        for i in 0..30 {
            let now = t0 + Duration::from_millis(i * 400);
            let text = "说".repeat((i as usize) + 1);
            let out = s.observe(&text, false, now);
            if !out.is_empty() {
                // Fired once the 10s max-segment age was crossed.
                assert!(now.duration_since(t0) >= Duration::from_secs(10));
                return;
            }
        }
        panic!("monologue was never force-cut");
    }

    #[test]
    fn definite_commits_then_next_utterance_dispatches_separately() {
        let (mut s, t0) = seg();
        // First utterance finalizes (definite) and cuts on its punctuation.
        let out = s.observe("你好。", true, t0);
        assert_eq!(out, vec!["你好。"]);
        // A new utterance with the SAME-looking text still dispatches.
        assert!(s.observe("你好", false, t0 + Duration::from_millis(50)).is_empty());
        let out = s.observe("你好。吗", false, t0 + Duration::from_millis(100));
        assert_eq!(out, vec!["你好。"]);
    }

    #[test]
    fn multibyte_boundaries_are_char_aligned() {
        let (mut s, t0) = seg();
        // Mixed CJK + ASCII; cut must land on a char boundary, not a byte.
        let out = s.observe("OK。next", false, t0);
        assert_eq!(out, vec!["OK。"]);
    }

    #[test]
    fn backlog_emits_one_sentence_per_iteration() {
        // Several complete sentences arrive in a single update → each is emitted
        // as its own segment (the cut loop drains the backlog).
        let (mut s, t0) = seg();
        let out = s.observe("你好。再见。好", false, t0);
        assert_eq!(out, vec!["你好。", "再见。"]);
        // "好" stays buffered.
        assert!(s.observe("你好。再见。好", false, t0 + Duration::from_millis(50)).is_empty());
    }

    #[test]
    fn committed_multiple_sentences_drain_in_order() {
        // A bulk `definite` commit of two sentences emits both, in order.
        let (mut s, t0) = seg();
        let out = s.observe("甲。乙。", true, t0);
        assert_eq!(out, vec!["甲。", "乙。"]);
    }

    #[test]
    fn committed_sentence_cuts_immediately_without_stability_wait() {
        // Committed text is stable, so its terminal punctuation cuts at once —
        // no punct_stable delay (contrast trailing_punctuation_waits…).
        let (mut s, t0) = seg();
        let out = s.observe("好的。", true, t0);
        assert_eq!(out, vec!["好的。"]);
    }

    #[test]
    fn ascii_and_ellipsis_are_sentence_ends() {
        let (mut s, t0) = seg();
        assert_eq!(s.observe("Hello. World", false, t0), vec!["Hello."]);
        let (mut s2, t0) = seg();
        assert_eq!(s2.observe("等等…好", false, t0), vec!["等等…"]);
    }

    #[test]
    fn size_snap_lands_on_the_last_clause_not_the_first() {
        // Two clause marks, no sentence end: the size guard snaps to the LAST
        // one so as much complete text as possible is emitted.
        let (mut s, t0) = seg();
        let text = format!("{}，{}，{}", "啊".repeat(20), "哦".repeat(20), "呢".repeat(25)); // 67
        let out = s.observe(&text, false, t0);
        assert_eq!(out.len(), 1);
        assert_eq!(out[0].chars().count(), 42); // 20 + ， + 20 + ，
        assert!(out[0].ends_with('，'));
    }

    #[test]
    fn size_boundary_is_exact() {
        let (mut s, t0) = seg();
        // 63 chars, no punctuation → under the cap, nothing emitted.
        assert!(s.observe(&"字".repeat(63), false, t0).is_empty());
        // 64 chars → hits the cap and flushes.
        let out = s.observe(&"字".repeat(64), false, t0 + Duration::from_millis(10));
        assert_eq!(out.len(), 1);
        assert_eq!(out[0].chars().count(), 64);
    }

    #[test]
    fn partial_revision_emits_only_the_corrected_text() {
        // The upstream first mis-hears, then revises the partial before it
        // completes. We only ever emit the final corrected sentence.
        let (mut s, t0) = seg();
        assert!(s.observe("你号", false, t0).is_empty());
        let out = s.observe("你好。在", false, t0 + Duration::from_millis(80));
        assert_eq!(out, vec!["你好。"]);
    }

    #[test]
    fn whitespace_only_input_never_cuts() {
        let (mut s, t0) = seg();
        assert!(s.observe("   ", false, t0).is_empty());
        assert!(s.tick(t0 + Duration::from_secs(5)).is_empty());
    }

    #[test]
    fn flush_emits_pending_tail_then_nothing() {
        let (mut s, t0) = seg();
        // An incomplete fragment with no boundary is held...
        assert!(s.observe("还没说完", false, t0).is_empty());
        // ...until the session ends, when flush hands it over.
        assert_eq!(s.flush(), Some("还没说完".to_string()));
        // Nothing left afterwards.
        assert_eq!(s.flush(), None);
    }

    #[test]
    fn flush_is_none_when_empty() {
        let (mut s, _t0) = seg();
        assert_eq!(s.flush(), None);
    }
}

#[cfg(test)]
mod terminator_tests {
    use super::*;

    fn seg() -> (Segmenter<Terminator>, Instant) {
        let t0 = Instant::now();
        (Segmenter::new(Terminator, t0), t0)
    }

    #[test]
    fn splits_latin_on_terminator_plus_space() {
        let (mut s, t0) = seg();
        assert_eq!(s.commit("Hello world", t0), Vec::<String>::new());
        assert_eq!(s.commit(". How are", t0), vec!["Hello world.".to_string()]);
        assert_eq!(s.flush(), Some("How are".to_string()));
    }

    #[test]
    fn does_not_split_decimals() {
        let (mut s, t0) = seg();
        assert_eq!(s.commit("pi is 3.14 today", t0), Vec::<String>::new());
    }

    #[test]
    fn splits_cjk_immediately() {
        let (mut s, t0) = seg();
        assert_eq!(
            s.commit("你好。最近怎么样？", t0),
            vec!["你好。".to_string(), "最近怎么样？".to_string()]
        );
    }
}
