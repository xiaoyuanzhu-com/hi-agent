//! Output pacing — release one segment of a turn into ordered wire actions.
//!
//! The mind's output arrives as `say`/`show_view` tool calls the sequencer
//! ([`super::sequencer`]) feeds through here one at a time. These helpers keep a
//! view paced to its narration: [`view_emits`] flushes the pending spoken
//! sentence to TTS *before* showing the view, so it lands after the sentence
//! before it and right as the sentence after it begins synthesizing — never
//! racing ahead of already-produced speech. Both are pure (no `Reactor`), so the
//! release ordering is unit-testable; the sequencer performs the [`Emit`]s.

use std::time::Instant;

use crate::foundation::segment::{Segmenter, Terminator};
use crate::types::{Geometry, ViewOp};

/// One release action the policy decides on. `Speak` goes to TTS only (the raw
/// chunk is mirrored to /thought separately by the sequencer); `ShowView` is
/// compiled then sent to /view.
#[derive(Debug)]
pub(super) enum Emit {
    Speak(String),
    ShowView { id: String, op: ViewOp, source: String, geometry: Option<Geometry> },
}

/// Coalesce spoken text into sentences for TTS. Pure: no side-effects, so the
/// release ordering is unit-testable without a `Reactor`.
pub(super) fn speak_emits(
    text: &str,
    splitter: &mut Segmenter<Terminator>,
    now: Instant,
) -> Vec<Emit> {
    splitter.commit(text, now).into_iter().map(Emit::Speak).collect()
}

/// Release a view, paced to its sentence: flush whatever spoken text the splitter
/// is still holding FIRST, so the view lands after the sentence before it and
/// right as the sentence after it begins synthesizing — never jumping ahead of
/// already-produced narration. Pure, for the same reason as [`speak_emits`].
pub(super) fn view_emits(
    splitter: &mut Segmenter<Terminator>,
    id: String,
    op: ViewOp,
    source: String,
    geometry: Option<Geometry>,
) -> Vec<Emit> {
    let mut out = Vec::new();
    if let Some(tail) = splitter.flush() {
        out.push(Emit::Speak(tail));
    }
    out.push(Emit::ShowView { id, op, source, geometry });
    out
}

#[cfg(test)]
mod release_tests {
    use super::*;

    /// Render the emit stream into a compact ordered transcript for assertion.
    fn trace(emits: &[Emit]) -> Vec<String> {
        emits
            .iter()
            .map(|e| match e {
                Emit::Speak(s) => format!("speak:{s}"),
                Emit::ShowView { source, .. } => format!("show:{source}"),
            })
            .collect()
    }

    #[test]
    fn view_is_paced_to_its_following_sentence() {
        use crate::types::{Region, SizeClass};
        // The core race fix: view1, narrate one, view2, narrate two — each view
        // emitted before its sentence, never both up front. Trailing spaces mirror
        // real LLM output so each sentence cuts cleanly on its terminator.
        let now = Instant::now();
        let mut sp = Segmenter::new(Terminator, now);
        let mut emits = Vec::new();
        let geo = Some(Geometry { region: Region::Right, size: SizeClass::Wide, owns_captions: false });
        emits.extend(view_emits(&mut sp, "a".into(), ViewOp::Show, "c1".into(), geo));
        emits.extend(speak_emits("Narrate one. ", &mut sp, now));
        emits.extend(view_emits(&mut sp, "b".into(), ViewOp::Show, "c2".into(), None));
        emits.extend(speak_emits("Narrate two. ", &mut sp, now));
        if let Some(tail) = sp.flush() {
            emits.push(Emit::Speak(tail));
        }
        assert_eq!(
            trace(&emits),
            vec!["show:c1", "speak:Narrate one.", "show:c2", "speak:Narrate two."]
        );
        // The declared geometry rides the emit untouched (and an undeclared view
        // stays None — the floor).
        let geom_of = |want: &str| {
            emits.iter().find_map(|e| match e {
                Emit::ShowView { id, geometry, .. } if id == want => Some(*geometry),
                _ => None,
            })
        };
        assert_eq!(geom_of("a"), Some(geo));
        assert_eq!(geom_of("b"), Some(None));
    }

    #[test]
    fn view_flushes_a_preceding_partial_sentence_first() {
        // A sentence with no terminator before a view: the view flushes it as a
        // Speak BEFORE the Show, so it never jumps ahead of its narration.
        let now = Instant::now();
        let mut sp = Segmenter::new(Terminator, now);
        let mut emits = speak_emits("partial no period", &mut sp, now);
        emits.extend(view_emits(&mut sp, "a".into(), ViewOp::Show, "c1".into(), None));
        assert_eq!(trace(&emits), vec!["speak:partial no period", "show:c1"]);
    }
}
