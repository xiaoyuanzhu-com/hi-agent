//! Per-person voice/face embedding samples — the recognition sidecars of the
//! `people` facet dimension.
//!
//! Each person's prose understanding lives at `memory/facets/people/<subject>.md`
//! (see [`super::facets`]). This module stores their **embedding samples** right
//! next to it as compact binary `<subject>.<modality>.f32` files, and answers the
//! one mechanical question: *which known person is this query vector nearest to?*
//!
//! A modality file is a flat concatenation of fixed-length samples written as raw
//! little-endian f32. The sample dimension is inferred from the query at match
//! time — all samples of one modality come from one model, so nothing about a
//! specific model (CAM++'s 192, ArcFace's 512) is hardcoded here. Samples are
//! expected L2-normalized (the capabilities normalize), but we compute true
//! cosine so a stray un-normalized vector can't skew a score.
//!
//! This is the **mechanical half of identity**: it returns ranked *candidates*
//! as evidence. The decision — same person? a new person? attach a name? — is the
//! agent's, deliberately ([[project-people-recognition-design]]). Like facets,
//! writes are atomic (temp sibling + rename) and last-writer-wins across scenes.
//!
//! **No caller wires this in yet.** The future caller is the perception path that
//! produces embeddings (e.g. embedding each STT speaker-turn); wiring it in later
//! is purely additive.

use std::path::Path;

use uuid::Uuid;

use super::{facets, layout};

/// The facet dimension these sidecars attach to.
const DIM: &str = "people";

/// Cap on samples kept per subject per modality. A gallery is a *bounded,
/// diverse* set, not a log of every observation — without this, one long call
/// would dump hundreds of near-identical samples and let a single session
/// dominate. First cut keeps the most-recent N (drop oldest); diversity-aware
/// pruning is a later refinement.
const MAX_SAMPLES: usize = 32;

/// Which embedding space a sample lives in. Voice and face occupy different
/// spaces and are never compared to each other, so each is its own sidecar file.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Modality {
    Voice,
    Face,
}

impl Modality {
    fn tag(self) -> &'static str {
        match self {
            Modality::Voice => "voice",
            Modality::Face => "face",
        }
    }
}

/// One ranked match: the facet subject (whose `<subject>.md` neighbour holds the
/// agent's prose understanding) and the best cosine similarity of the query
/// against any of that subject's samples, in `[-1, 1]`.
#[derive(Debug, Clone, PartialEq)]
pub struct Candidate {
    pub subject: String,
    pub similarity: f32,
}

/// Append one embedding `sample` to `subject`'s `modality` sidecar, creating it
/// if new. Returns the canonical `people/<subject>` ref. Atomic: the updated
/// bytes are written to a temp sibling and renamed in, so a reader never sees a
/// torn file. Errors if `subject` slugs to nothing, the sample is empty, or its
/// dimension disagrees with samples already on file (a model/dim mismatch).
pub async fn enroll(
    data_dir: &Path,
    subject: &str,
    modality: Modality,
    sample: &[f32],
) -> anyhow::Result<String> {
    let subj = facets::slug(subject);
    anyhow::ensure!(!subj.is_empty(), "subject must contain a usable character");
    anyhow::ensure!(!sample.is_empty(), "sample must be non-empty");

    let dir = layout::facets_dir(data_dir).join(DIM);
    tokio::fs::create_dir_all(&dir).await?;
    let path = dir.join(format!("{subj}.{}.f32", modality.tag()));

    let mut bytes = match tokio::fs::read(&path).await {
        Ok(b) => b,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => Vec::new(),
        Err(e) => return Err(e.into()),
    };
    let stride = sample.len() * 4;
    if !bytes.is_empty() && bytes.len() % stride != 0 {
        anyhow::bail!(
            "{}: {} existing bytes don't divide into {}-dim samples (model/dim mismatch)",
            path.display(),
            bytes.len(),
            sample.len()
        );
    }
    bytes.extend(sample.iter().flat_map(|f| f.to_le_bytes()));

    // Bound the gallery: keep the most recent MAX_SAMPLES, dropping oldest first.
    let max_bytes = MAX_SAMPLES * stride;
    if bytes.len() > max_bytes {
        bytes.drain(..bytes.len() - max_bytes);
    }

    let tmp = dir.join(format!(".{subj}.{}.f32.tmp-{}", modality.tag(), Uuid::now_v7().simple()));
    tokio::fs::write(&tmp, &bytes).await?;
    tokio::fs::rename(&tmp, &path).await?;
    Ok(format!("{DIM}/{subj}"))
}

/// Rank known subjects by how close `query` is to their nearest `modality`
/// sample (the max cosine over that subject's samples), best first, capped at
/// `k`. Subjects with no samples for this modality are skipped; a sidecar whose
/// byte length doesn't divide into query-sized samples is skipped (not fatal).
/// Empty before anyone is enrolled.
pub async fn nearest(
    data_dir: &Path,
    modality: Modality,
    query: &[f32],
    k: usize,
) -> anyhow::Result<Vec<Candidate>> {
    anyhow::ensure!(!query.is_empty(), "query must be non-empty");

    let dir = layout::facets_dir(data_dir).join(DIM);
    let suffix = format!(".{}.f32", modality.tag());
    let mut rd = match tokio::fs::read_dir(&dir).await {
        Ok(rd) => rd,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(Vec::new()),
        Err(e) => return Err(e.into()),
    };

    let mut out: Vec<Candidate> = Vec::new();
    while let Some(ent) = rd.next_entry().await? {
        let Ok(fname) = ent.file_name().into_string() else {
            continue;
        };
        // Match only this modality's sidecars; the `.md` facet and the other
        // modality (and any `.tmp-…` half-write) don't end in this suffix.
        let Some(subject) = fname.strip_suffix(&suffix) else {
            continue;
        };
        if subject.is_empty() || subject.starts_with('.') {
            continue;
        }
        let bytes = tokio::fs::read(ent.path()).await?;
        if let Some(best) = best_cosine(&bytes, query) {
            out.push(Candidate { subject: subject.to_string(), similarity: best });
        }
    }

    out.sort_by(|a, b| {
        b.similarity.partial_cmp(&a.similarity).unwrap_or(std::cmp::Ordering::Equal)
    });
    out.truncate(k);
    Ok(out)
}

/// Max cosine of `query` against each fixed-length sample packed in `bytes`.
/// `None` if `bytes` is empty, doesn't divide into query-sized samples (corrupt
/// or wrong-dim — skipped, not fatal), or the query is a zero vector.
fn best_cosine(bytes: &[u8], query: &[f32]) -> Option<f32> {
    let stride = query.len() * 4;
    if bytes.is_empty() || bytes.len() % stride != 0 {
        return None;
    }
    let q_norm = norm(query);
    if q_norm == 0.0 {
        return None;
    }

    let mut best = f32::NEG_INFINITY;
    for sample in bytes.chunks_exact(stride) {
        let mut dot = 0.0_f32;
        let mut sq = 0.0_f32;
        for (i, f) in sample.chunks_exact(4).enumerate() {
            let v = f32::from_le_bytes([f[0], f[1], f[2], f[3]]);
            dot += v * query[i];
            sq += v * v;
        }
        let s_norm = sq.sqrt();
        if s_norm > 0.0 {
            best = best.max(dot / (q_norm * s_norm));
        }
    }
    best.is_finite().then_some(best)
}

fn norm(v: &[f32]) -> f32 {
    v.iter().map(|x| x * x).sum::<f32>().sqrt()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn td() -> tempfile::TempDir {
        tempfile::tempdir().unwrap()
    }

    #[tokio::test]
    async fn enroll_then_nearest_finds_the_subject() {
        let dir = td();
        let r = enroll(dir.path(), "Alice", Modality::Voice, &[1.0, 0.0, 0.0, 0.0]).await.unwrap();
        assert_eq!(r, "people/alice");
        let got = nearest(dir.path(), Modality::Voice, &[1.0, 0.0, 0.0, 0.0], 5).await.unwrap();
        assert_eq!(got.len(), 1);
        assert_eq!(got[0].subject, "alice");
        assert!((got[0].similarity - 1.0).abs() < 1e-6, "sim {}", got[0].similarity);
    }

    #[tokio::test]
    async fn same_subject_ranks_above_different() {
        let dir = td();
        enroll(dir.path(), "Alice", Modality::Voice, &[1.0, 0.0, 0.0, 0.0]).await.unwrap();
        enroll(dir.path(), "Bob", Modality::Voice, &[0.0, 1.0, 0.0, 0.0]).await.unwrap();
        let got = nearest(dir.path(), Modality::Voice, &[0.9, 0.1, 0.0, 0.0], 5).await.unwrap();
        assert_eq!(got[0].subject, "alice");
        assert!(got[0].similarity > got[1].similarity);
    }

    #[tokio::test]
    async fn nearest_takes_the_max_over_a_subjects_samples() {
        let dir = td();
        enroll(dir.path(), "Alice", Modality::Voice, &[1.0, 0.0, 0.0, 0.0]).await.unwrap();
        enroll(dir.path(), "Alice", Modality::Voice, &[0.0, 0.0, 1.0, 0.0]).await.unwrap();
        let got = nearest(dir.path(), Modality::Voice, &[0.0, 0.0, 1.0, 0.0], 5).await.unwrap();
        assert_eq!(got.len(), 1, "one subject, two samples");
        assert!((got[0].similarity - 1.0).abs() < 1e-6);
    }

    #[tokio::test]
    async fn k_caps_the_result_count() {
        let dir = td();
        for name in ["Alice", "Bob", "Carol"] {
            enroll(dir.path(), name, Modality::Voice, &[1.0, 0.0, 0.0, 0.0]).await.unwrap();
        }
        let got = nearest(dir.path(), Modality::Voice, &[1.0, 0.0, 0.0, 0.0], 2).await.unwrap();
        assert_eq!(got.len(), 2);
    }

    #[tokio::test]
    async fn nearest_is_empty_before_anyone_is_enrolled() {
        let dir = td();
        assert!(nearest(dir.path(), Modality::Voice, &[1.0, 0.0], 5).await.unwrap().is_empty());
    }

    #[tokio::test]
    async fn modalities_are_independent() {
        let dir = td();
        enroll(dir.path(), "Alice", Modality::Voice, &[1.0, 0.0, 0.0, 0.0]).await.unwrap();
        enroll(dir.path(), "Bob", Modality::Face, &[1.0, 0.0, 0.0, 0.0]).await.unwrap();
        let voice = nearest(dir.path(), Modality::Voice, &[1.0, 0.0, 0.0, 0.0], 5).await.unwrap();
        let face = nearest(dir.path(), Modality::Face, &[1.0, 0.0, 0.0, 0.0], 5).await.unwrap();
        assert_eq!(voice.iter().map(|c| c.subject.as_str()).collect::<Vec<_>>(), vec!["alice"]);
        assert_eq!(face.iter().map(|c| c.subject.as_str()).collect::<Vec<_>>(), vec!["bob"]);
    }

    #[tokio::test]
    async fn sidecars_do_not_pollute_the_facet_subject_index() {
        let dir = td();
        enroll(dir.path(), "Alice", Modality::Voice, &[1.0, 0.0, 0.0, 0.0]).await.unwrap();
        // A bare embedding sidecar (no prose facet yet) must not look like a subject.
        assert!(facets::facet_subject_index(dir.path()).await.unwrap().is_empty());
        // Once a prose facet exists, the subject shows up — and still no `.f32` noise.
        facets::update_facet(dir.path(), "people", "Alice", "x").await.unwrap();
        assert_eq!(facets::facet_subject_index(dir.path()).await.unwrap(), vec!["people/alice"]);
    }

    #[tokio::test]
    async fn dim_mismatch_on_enroll_errors() {
        let dir = td();
        enroll(dir.path(), "Alice", Modality::Voice, &[1.0, 0.0, 0.0, 0.0]).await.unwrap();
        assert!(enroll(dir.path(), "Alice", Modality::Voice, &[1.0, 0.0, 0.0]).await.is_err());
    }

    #[tokio::test]
    async fn empty_subject_or_sample_is_rejected() {
        let dir = td();
        assert!(enroll(dir.path(), "??", Modality::Voice, &[1.0]).await.is_err());
        assert!(enroll(dir.path(), "Alice", Modality::Voice, &[]).await.is_err());
    }

    #[tokio::test]
    async fn enroll_caps_the_gallery_dropping_oldest() {
        let dir = td();
        for i in 0..(MAX_SAMPLES + 5) {
            enroll(dir.path(), "Alice", Modality::Voice, &[i as f32, 1.0, 0.0, 0.0]).await.unwrap();
        }
        let path = layout::facets_dir(dir.path()).join("people").join("alice.voice.f32");
        let len = std::fs::metadata(&path).unwrap().len() as usize;
        assert_eq!(len, MAX_SAMPLES * 4 * std::mem::size_of::<f32>(), "file holds exactly MAX_SAMPLES");
    }
}
