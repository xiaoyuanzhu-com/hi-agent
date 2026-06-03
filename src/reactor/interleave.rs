//! Turn output: parsed into ordered pieces, then paced onto the wire.
//!
//! A turn's reply is parsed into an ordered run of [`Segment`]s — plain spoken
//! text interleaved with completed `[[surface:*]]` blocks, in document order.
//! Keeping the pieces interleaved (rather than detaching all surfaces up front)
//! is the whole point: it lets a slide be paced to the sentence that narrates it.
//! [`surface_emits`] flushes the pending spoken text to TTS *before* showing the
//! slide, so the slide lands after the sentence before it and right as the
//! sentence after it begins synthesizing — never racing ahead of already-produced
//! narration.
//!
//! Both halves are pure and `Reactor`-free so the ordering is unit-testable:
//! [`Extractor`] turns the text stream into [`Segment`]s; [`speak_emits`] /
//! [`surface_emits`] turn one segment into ordered [`Emit`] actions. The reactor's
//! `run_turn` owns the orchestration — the sentence splitter, the delegate/alarm
//! side-effects, the TTS sender — and performs the emits.

use std::time::Instant;

use uuid::Uuid;

use crate::segment::{Segmenter, Terminator};
use crate::types::{SurfaceEnvelope, SurfaceMode, SurfaceOp};

const OPEN_CARD: &str = "[[surface:card]]";
const OPEN_FULL: &str = "[[surface:full]]";
const CLOSE: &str = "[[/surface]]";

/// One ordered piece of a turn's reply, in document order. Plain text the agent
/// speaks/displays is [`Spoken`](Segment::Spoken); a completed `[[surface:*]]`
/// block is [`Surface`](Segment::Surface). Keeping these interleaved is what lets
/// a slide be paced to the sentence that narrates it.
#[derive(Debug)]
pub(super) enum Segment {
    Spoken(String),
    Surface(SurfaceEnvelope),
}

/// One release action the policy decides on. `Speak` goes to TTS only (the raw
/// chunk is mirrored to /thought separately by the caller); `Show` goes to
/// /surface.
#[derive(Debug)]
pub(super) enum Emit {
    Speak(String),
    Show(SurfaceEnvelope),
}

/// Streaming extractor that turns the agent's text stream into an ordered run of
/// [`Segment`]s, pulling `[[surface:…]] … [[/surface]]` HTML blocks out as
/// [`Segment::Surface`] while the surrounding text passes through as
/// [`Segment::Spoken`]. A short tail that could be a partial opener is held back
/// so a marker split across chunks is still recognized. Mirrors the convention
/// taught in the soul.
pub(super) struct Extractor {
    buf: String,
    inside: Option<SurfaceMode>,
}

impl Extractor {
    pub(super) fn new() -> Self {
        Self { buf: String::new(), inside: None }
    }

    /// Feed a chunk; return the segments completed by it, in document order. An
    /// open block whose close has not yet arrived stays buffered.
    pub(super) fn push(&mut self, chunk: &str) -> Vec<Segment> {
        self.buf.push_str(chunk);
        let mut out = Vec::new();

        loop {
            match self.inside {
                None => {
                    let card = self.buf.find(OPEN_CARD);
                    let full = self.buf.find(OPEN_FULL);
                    let opener = match (card, full) {
                        (Some(c), Some(f)) if c <= f => Some((c, SurfaceMode::Card, OPEN_CARD.len())),
                        (Some(_), Some(f)) => Some((f, SurfaceMode::Full, OPEN_FULL.len())),
                        (Some(c), None) => Some((c, SurfaceMode::Card, OPEN_CARD.len())),
                        (None, Some(f)) => Some((f, SurfaceMode::Full, OPEN_FULL.len())),
                        (None, None) => None,
                    };
                    if let Some((idx, mode, tok_len)) = opener {
                        push_spoken(&mut out, &self.buf[..idx]);
                        self.buf = self.buf[idx + tok_len..].to_string();
                        self.inside = Some(mode);
                        continue;
                    }
                    // No opener: emit everything except a tail that could be the
                    // start of one continuing in the next chunk.
                    let keep = partial_open_suffix_len(&self.buf);
                    let emit_to = self.buf.len() - keep;
                    push_spoken(&mut out, &self.buf[..emit_to]);
                    self.buf = self.buf[emit_to..].to_string();
                    break;
                }
                Some(mode) => {
                    if let Some(idx) = self.buf.find(CLOSE) {
                        let html = self.buf[..idx].trim().to_string();
                        self.buf = self.buf[idx + CLOSE.len()..].to_string();
                        self.inside = None;
                        out.push(Segment::Surface(SurfaceEnvelope {
                            id: Uuid::now_v7().to_string(),
                            op: SurfaceOp::Show,
                            mode: Some(mode),
                            html: Some(html),
                            ttl_ms: None,
                        }));
                        continue;
                    }
                    break; // close not present yet; keep buffering the HTML
                }
            }
        }
        out
    }

    /// End of turn: hand back any held-back plain text as a final [`Spoken`]
    /// segment. An unterminated block is dropped.
    pub(super) fn flush(&mut self) -> Option<Segment> {
        let out = if self.inside.is_none() && !self.buf.trim().is_empty() {
            Some(Segment::Spoken(std::mem::take(&mut self.buf)))
        } else {
            None
        };
        self.buf.clear();
        self.inside = None;
        out
    }
}

/// Push a non-empty text run as a [`Segment::Spoken`]; skip empties so the
/// segment stream carries no blank units.
fn push_spoken(out: &mut Vec<Segment>, text: &str) {
    if !text.is_empty() {
        out.push(Segment::Spoken(text.to_string()));
    }
}

/// Length (bytes) of the longest suffix of `buf` that is a proper prefix of a
/// surface opener — i.e. a marker possibly split across chunks.
fn partial_open_suffix_len(buf: &str) -> usize {
    let max = OPEN_CARD.len().max(OPEN_FULL.len()) - 1;
    let start = buf.len().saturating_sub(max);
    for i in start..buf.len() {
        if !buf.is_char_boundary(i) {
            continue;
        }
        let suffix = &buf[i..];
        if OPEN_CARD.starts_with(suffix) || OPEN_FULL.starts_with(suffix) {
            return buf.len() - i;
        }
    }
    0
}

/// Coalesce spoken text into sentences for TTS. Pure: no side-effects, so the
/// release ordering is unit-testable without a `Reactor`.
pub(super) fn speak_emits(
    residual: &str,
    splitter: &mut Segmenter<Terminator>,
    now: Instant,
) -> Vec<Emit> {
    splitter.commit(residual, now).into_iter().map(Emit::Speak).collect()
}

/// Release a surface, paced to its sentence: flush whatever spoken text the
/// splitter is still holding FIRST, so the slide lands after the sentence before
/// it and right as the sentence after it begins synthesizing — never jumping
/// ahead of already-produced narration. Pure, for the same reason as
/// [`speak_emits`].
pub(super) fn surface_emits(splitter: &mut Segmenter<Terminator>, env: SurfaceEnvelope) -> Vec<Emit> {
    let mut out = Vec::new();
    if let Some(tail) = splitter.flush() {
        out.push(Emit::Speak(tail));
    }
    out.push(Emit::Show(env));
    out
}

#[cfg(test)]
mod interleave_tests {
    use super::*;

    fn spoken(seg: &Segment) -> Option<&str> {
        match seg {
            Segment::Spoken(s) => Some(s.as_str()),
            _ => None,
        }
    }

    #[test]
    fn passes_plain_text_through() {
        let mut e = Extractor::new();
        let segs = e.push("just talking, nothing to show");
        assert_eq!(segs.len(), 1);
        assert_eq!(spoken(&segs[0]), Some("just talking, nothing to show"));
        assert!(e.flush().is_none());
    }

    #[test]
    fn interleaves_text_and_surface_in_order() {
        let mut e = Extractor::new();
        let segs = e.push("Here. [[surface:card]]<b>hi</b>[[/surface]] Done.");
        assert_eq!(segs.len(), 3);
        assert_eq!(spoken(&segs[0]), Some("Here. "));
        match &segs[1] {
            Segment::Surface(env) => {
                assert_eq!(env.mode, Some(SurfaceMode::Card));
                assert_eq!(env.html.as_deref(), Some("<b>hi</b>"));
            }
            _ => panic!("expected surface"),
        }
        assert_eq!(spoken(&segs[2]), Some(" Done."));
    }

    #[test]
    fn recognizes_marker_split_across_chunks() {
        let mut e = Extractor::new();
        let s1 = e.push("look [[surf");
        assert_eq!(s1.len(), 1);
        assert_eq!(spoken(&s1[0]), Some("look "));
        let s2 = e.push("ace:full]]<p>x</p>[[/surface]]");
        assert_eq!(s2.len(), 1);
        match &s2[0] {
            Segment::Surface(env) => {
                assert_eq!(env.mode, Some(SurfaceMode::Full));
                assert_eq!(env.html.as_deref(), Some("<p>x</p>"));
            }
            _ => panic!("expected surface"),
        }
    }

    #[test]
    fn two_adjacent_surfaces_keep_order() {
        let mut e = Extractor::new();
        let segs =
            e.push("[[surface:card]]A[[/surface]] mid [[surface:card]]B[[/surface]] end");
        // Surface(A), Spoken(" mid "), Surface(B), Spoken(" end")
        assert_eq!(segs.len(), 4);
        assert!(matches!(&segs[0], Segment::Surface(_)));
        assert_eq!(spoken(&segs[1]), Some(" mid "));
        assert!(matches!(&segs[2], Segment::Surface(_)));
        assert_eq!(spoken(&segs[3]), Some(" end"));
    }
}

#[cfg(test)]
mod release_tests {
    use super::*;

    fn card(html: &str) -> SurfaceEnvelope {
        SurfaceEnvelope {
            id: html.to_string(),
            op: SurfaceOp::Show,
            mode: Some(SurfaceMode::Card),
            html: Some(html.to_string()),
            ttl_ms: None,
        }
    }

    /// Render the emit stream into a compact ordered transcript for assertion.
    fn trace(emits: &[Emit]) -> Vec<String> {
        emits
            .iter()
            .map(|e| match e {
                Emit::Speak(s) => format!("speak:{s}"),
                Emit::Show(env) => format!("show:{}", env.html.as_deref().unwrap_or("")),
            })
            .collect()
    }

    #[test]
    fn surface_is_paced_to_its_following_sentence() {
        // The core race fix: card1, narrate one, card2, narrate two — each card
        // emitted before its sentence, never both cards up front. Trailing spaces
        // mirror real LLM output so each sentence cuts cleanly on its terminator.
        let now = Instant::now();
        let mut sp = Segmenter::new(Terminator, now);
        let mut emits = Vec::new();
        emits.extend(surface_emits(&mut sp, card("c1")));
        emits.extend(speak_emits("Narrate one. ", &mut sp, now));
        emits.extend(surface_emits(&mut sp, card("c2")));
        emits.extend(speak_emits("Narrate two. ", &mut sp, now));
        if let Some(tail) = sp.flush() {
            emits.push(Emit::Speak(tail));
        }
        assert_eq!(
            trace(&emits),
            vec![
                "show:c1",
                "speak:Narrate one.",
                "show:c2",
                "speak:Narrate two.",
            ]
        );
    }

    #[test]
    fn surface_flushes_a_preceding_partial_sentence_first() {
        // A sentence with no terminator before a surface: the surface flushes it as
        // a Speak BEFORE the Show, so the slide never jumps ahead of its narration.
        let now = Instant::now();
        let mut sp = Segmenter::new(Terminator, now);
        let mut emits = speak_emits("partial no period", &mut sp, now);
        emits.extend(surface_emits(&mut sp, card("c1")));
        assert_eq!(
            trace(&emits),
            vec!["speak:partial no period", "show:c1"]
        );
    }
}
