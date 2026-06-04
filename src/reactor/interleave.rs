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
use crate::types::{SurfaceEnvelope, SurfaceMode, SurfaceOp, ViewOp};

const OPEN_CARD: &str = "[[surface:card]]";
const OPEN_FULL: &str = "[[surface:full]]";
const CLOSE: &str = "[[/surface]]";
// A view opener is variable-length (`[[view id=… op=…]]`): this is its prefix,
// and the opening tag runs to the next `]]`. `[[view` cannot be mistaken for the
// `[[/view]]` close, which starts `[[/`.
const OPEN_VIEW: &str = "[[view";
const VIEW_TAG_CLOSE: &str = "]]";
const CLOSE_VIEW: &str = "[[/view]]";

/// One ordered piece of a turn's reply, in document order. Plain text the agent
/// speaks/displays is [`Spoken`](Segment::Spoken); a completed `[[surface:*]]`
/// block is [`Surface`](Segment::Surface); a completed `[[view…]]` block is
/// [`View`](Segment::View), carrying its raw JSX source for later compilation.
/// Keeping these interleaved is what lets a slide/view be paced to its sentence.
#[derive(Debug)]
pub(super) enum Segment {
    Spoken(String),
    Surface(SurfaceEnvelope),
    View { id: String, op: ViewOp, source: String },
}

/// One release action the policy decides on. `Speak` goes to TTS only (the raw
/// chunk is mirrored to /thought separately by the caller); `Show` goes to
/// /surface; `ShowView` is compiled then sent to /view.
#[derive(Debug)]
pub(super) enum Emit {
    Speak(String),
    Show(SurfaceEnvelope),
    ShowView { id: String, op: ViewOp, source: String },
}

/// Which kind of block the extractor is currently inside. `None` (outside) scans
/// for the next opener.
#[derive(Debug, Clone)]
enum Inside {
    Surface(SurfaceMode),
    View { id: String, op: ViewOp },
}

/// Streaming extractor that turns the agent's text stream into an ordered run of
/// [`Segment`]s, pulling `[[surface:…]] … [[/surface]]` and `[[view…]] …
/// [[/view]]` blocks out while surrounding text passes through as
/// [`Segment::Spoken`]. A short tail that could be a partial opener is held back
/// so a marker split across chunks is still recognized. Mirrors the convention
/// taught in the soul.
pub(super) struct Extractor {
    buf: String,
    inside: Option<Inside>,
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
            // Clone the small state so no borrow of `self.inside` is held while we
            // mutate `self.buf` / `self.inside` in the arms.
            match self.inside.clone() {
                None => {
                    // The earliest opener wins — the two fixed surface markers and
                    // the variable-length view marker, in document order.
                    let card = self.buf.find(OPEN_CARD);
                    let full = self.buf.find(OPEN_FULL);
                    let view = self.buf.find(OPEN_VIEW);
                    let Some(idx) = [card, full, view].into_iter().flatten().min() else {
                        // No opener: emit everything except a tail that could be the
                        // start of one continuing in the next chunk.
                        let keep = partial_open_suffix_len(&self.buf);
                        let emit_to = self.buf.len() - keep;
                        push_spoken(&mut out, &self.buf[..emit_to]);
                        self.buf = self.buf[emit_to..].to_string();
                        break;
                    };

                    if self.buf[idx..].starts_with(OPEN_CARD) {
                        push_spoken(&mut out, &self.buf[..idx]);
                        self.buf = self.buf[idx + OPEN_CARD.len()..].to_string();
                        self.inside = Some(Inside::Surface(SurfaceMode::Card));
                        continue;
                    }
                    if self.buf[idx..].starts_with(OPEN_FULL) {
                        push_spoken(&mut out, &self.buf[..idx]);
                        self.buf = self.buf[idx + OPEN_FULL.len()..].to_string();
                        self.inside = Some(Inside::Surface(SurfaceMode::Full));
                        continue;
                    }

                    // A view opener: its opening tag runs from `[[view` to the next
                    // `]]`. If that close hasn't arrived, hold the partial opener and
                    // wait for the next chunk.
                    let after = idx + OPEN_VIEW.len();
                    let Some(rel) = self.buf[after..].find(VIEW_TAG_CLOSE) else {
                        push_spoken(&mut out, &self.buf[..idx]);
                        self.buf = self.buf[idx..].to_string();
                        break;
                    };
                    let (id, op) = parse_view_attrs(&self.buf[after..after + rel]);
                    push_spoken(&mut out, &self.buf[..idx]);
                    self.buf = self.buf[after + rel + VIEW_TAG_CLOSE.len()..].to_string();
                    if op == ViewOp::Dismiss {
                        // A dismiss carries no body and needs no `[[/view]]`.
                        out.push(Segment::View { id, op, source: String::new() });
                        continue;
                    }
                    self.inside = Some(Inside::View { id, op });
                    continue;
                }
                Some(Inside::Surface(mode)) => {
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
                Some(Inside::View { id, op }) => {
                    if let Some(idx) = self.buf.find(CLOSE_VIEW) {
                        let source = self.buf[..idx].trim().to_string();
                        self.buf = self.buf[idx + CLOSE_VIEW.len()..].to_string();
                        self.inside = None;
                        out.push(Segment::View { id, op, source });
                        continue;
                    }
                    break; // close not present yet; keep buffering the source
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
/// surface or view opener — i.e. a marker possibly split across chunks.
fn partial_open_suffix_len(buf: &str) -> usize {
    let max = OPEN_CARD.len().max(OPEN_FULL.len()).max(OPEN_VIEW.len()) - 1;
    let start = buf.len().saturating_sub(max);
    for i in start..buf.len() {
        if !buf.is_char_boundary(i) {
            continue;
        }
        let suffix = &buf[i..];
        if OPEN_CARD.starts_with(suffix)
            || OPEN_FULL.starts_with(suffix)
            || OPEN_VIEW.starts_with(suffix)
        {
            return buf.len() - i;
        }
    }
    0
}

/// Parse a view opener's attributes (`id=… op=…`) from the text between `[[view`
/// and `]]`. A missing `id` gets a fresh uuid (no animation continuity); an
/// unknown or missing `op` defaults to `show`. Values may be optionally quoted.
fn parse_view_attrs(attrs: &str) -> (String, ViewOp) {
    let mut id: Option<String> = None;
    let mut op = ViewOp::Show;
    for tok in attrs.split_whitespace() {
        if let Some(v) = tok.strip_prefix("id=") {
            id = Some(v.trim_matches('"').to_string());
        } else if let Some(v) = tok.strip_prefix("op=") {
            op = match v.trim_matches('"') {
                "replace" => ViewOp::Replace,
                "dismiss" => ViewOp::Dismiss,
                _ => ViewOp::Show,
            };
        }
    }
    (id.unwrap_or_else(|| Uuid::now_v7().to_string()), op)
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

/// Release a view, paced to its sentence exactly like [`surface_emits`]: flush
/// whatever spoken text the splitter is still holding FIRST, so the view lands
/// after the sentence before it and right as the next begins. Pure.
pub(super) fn view_emits(
    splitter: &mut Segmenter<Terminator>,
    id: String,
    op: ViewOp,
    source: String,
) -> Vec<Emit> {
    let mut out = Vec::new();
    if let Some(tail) = splitter.flush() {
        out.push(Emit::Speak(tail));
    }
    out.push(Emit::ShowView { id, op, source });
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

    fn view(seg: &Segment) -> Option<(&str, ViewOp, &str)> {
        match seg {
            Segment::View { id, op, source } => Some((id.as_str(), *op, source.as_str())),
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

    #[test]
    fn interleaves_text_and_view_in_order() {
        let mut e = Extractor::new();
        let segs = e.push("Here. [[view id=q op=show]]<X/>[[/view]] Done.");
        assert_eq!(segs.len(), 3);
        assert_eq!(spoken(&segs[0]), Some("Here. "));
        assert_eq!(view(&segs[1]), Some(("q", ViewOp::Show, "<X/>")));
        assert_eq!(spoken(&segs[2]), Some(" Done."));
    }

    #[test]
    fn recognizes_view_marker_split_across_chunks() {
        let mut e = Extractor::new();
        // The opener itself is split mid-attribute across the chunk boundary.
        let s1 = e.push("look [[view id=q op=re");
        assert_eq!(s1.len(), 1);
        assert_eq!(spoken(&s1[0]), Some("look "));
        let s2 = e.push("place]]<Y/>[[/view]]");
        assert_eq!(s2.len(), 1);
        assert_eq!(view(&s2[0]), Some(("q", ViewOp::Replace, "<Y/>")));
    }

    #[test]
    fn view_dismiss_needs_no_close() {
        let mut e = Extractor::new();
        let segs = e.push("[[view id=old op=dismiss]] bye");
        assert_eq!(segs.len(), 2);
        assert_eq!(view(&segs[0]), Some(("old", ViewOp::Dismiss, "")));
        assert_eq!(spoken(&segs[1]), Some(" bye"));
    }

    #[test]
    fn view_attrs_default_op_and_synthesize_id() {
        let (id, op) = parse_view_attrs("");
        assert!(!id.is_empty(), "a missing id is synthesized");
        assert_eq!(op, ViewOp::Show);
        let (id2, op2) = parse_view_attrs(r#"id="quiz" op="replace""#);
        assert_eq!(id2, "quiz");
        assert_eq!(op2, ViewOp::Replace);
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
