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

use std::path::{Path, PathBuf};

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

/// Cosine at/above which an observation is taken to be an existing person rather
/// than someone new (see [`assign`]). Conservative — minting a duplicate cluster
/// (mergeable later) is cheaper than wrongly fusing two people.
const APPEND_THRESHOLD: f32 = 0.5;

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

/// A short, opaque, stable identity key for a freshly-discovered person — what a
/// cluster lives under until a real name is learned and it is [`rename`]d onto
/// the name. Eight base-36 chars from a v7 uuid's random low bits (e.g. `ff32ce3w`).
pub fn mint_id() -> String {
    const ALPHABET: &[u8; 36] = b"0123456789abcdefghijklmnopqrstuvwxyz";
    let mut x = Uuid::now_v7().as_u128() as u64;
    let mut s = String::with_capacity(8);
    for _ in 0..8 {
        s.push(ALPHABET[(x % 36) as usize] as char);
        x /= 36;
    }
    s
}

/// Place one observed `embedding` into the people store: if it is within
/// [`APPEND_THRESHOLD`] of an existing subject, append it there and return that
/// subject; otherwise [`mint_id`] a fresh id, enroll under it, and return the id.
/// This is the **mechanical half of clustering** — identity forms from biometrics
/// alone, no name or LLM. The returned subject is an id (new person) or whatever
/// key the matched cluster currently has (an id, or a name if already named).
pub async fn assign(data_dir: &Path, modality: Modality, embedding: &[f32]) -> anyhow::Result<String> {
    if let Some(top) = nearest(data_dir, modality, embedding, 1).await?.into_iter().next()
        && top.similarity >= APPEND_THRESHOLD
    {
        enroll(data_dir, &top.subject, modality, embedding).await?;
        return Ok(top.subject);
    }
    let id = mint_id();
    enroll(data_dir, &id, modality, embedding).await?;
    Ok(id)
}

/// Move every `facets/people/<old>.*` artifact to `<new>.*` — the structural side
/// of naming (rename a minted id to a learned name) and of merging (collapse two
/// clusters of one person). For a `.f32` sidecar whose target already exists the
/// two galleries are concatenated (a later [`enroll`] re-applies the cap); for any
/// other artifact (the `.md` facet) the target is kept — it regenerates from
/// episodes — and the old dropped. Renaming a subject onto itself is a no-op.
pub async fn rename(data_dir: &Path, old: &str, new: &str) -> anyhow::Result<()> {
    let (old_s, new_s) = (facets::slug(old), facets::slug(new));
    anyhow::ensure!(!old_s.is_empty() && !new_s.is_empty(), "old and new must each slug to something");
    if old_s == new_s {
        return Ok(());
    }

    let dir = layout::facets_dir(data_dir).join(DIM);
    let mut rd = match tokio::fs::read_dir(&dir).await {
        Ok(rd) => rd,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(()),
        Err(e) => return Err(e.into()),
    };
    // Collect first, then mutate — don't modify the dir mid-iteration.
    let prefix = format!("{old_s}.");
    let mut arts: Vec<(PathBuf, String)> = Vec::new();
    while let Some(ent) = rd.next_entry().await? {
        let Ok(fname) = ent.file_name().into_string() else {
            continue;
        };
        if fname.starts_with('.') {
            continue; // skip hidden/tmp
        }
        if let Some(ext) = fname.strip_prefix(&prefix) {
            arts.push((ent.path(), ext.to_string()));
        }
    }
    drop(rd);

    for (src, ext) in arts {
        let dst = dir.join(format!("{new_s}.{ext}"));
        let target_exists = tokio::fs::try_exists(&dst).await?;
        if target_exists && ext.ends_with("f32") {
            let mut merged = tokio::fs::read(&dst).await?;
            merged.extend(tokio::fs::read(&src).await?);
            let tmp = dir.join(format!(".{new_s}.{ext}.tmp-{}", Uuid::now_v7().simple()));
            tokio::fs::write(&tmp, &merged).await?;
            tokio::fs::rename(&tmp, &dst).await?;
            tokio::fs::remove_file(&src).await?;
        } else if target_exists {
            tokio::fs::remove_file(&src).await?;
        } else {
            tokio::fs::rename(&src, &dst).await?;
        }
    }
    Ok(())
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

    #[test]
    fn mint_id_is_eight_base36_chars() {
        let id = mint_id();
        assert_eq!(id.len(), 8);
        assert!(id.chars().all(|c| c.is_ascii_lowercase() || c.is_ascii_digit()));
        assert_ne!(mint_id(), mint_id(), "ids are not constant");
    }

    #[tokio::test]
    async fn assign_mints_on_empty_then_appends_close_and_mints_far() {
        let dir = td();
        // Empty store → mints a fresh id and stores it.
        let id = assign(dir.path(), Modality::Face, &[1.0, 0.0, 0.0, 0.0]).await.unwrap();
        assert_eq!(id.len(), 8);
        // A near-identical observation → appends to the same id (not a new one).
        let again = assign(dir.path(), Modality::Face, &[0.98, 0.0, 0.0, 0.0]).await.unwrap();
        assert_eq!(again, id);
        // An orthogonal observation (cosine 0 < threshold) → a new id.
        let other = assign(dir.path(), Modality::Face, &[0.0, 1.0, 0.0, 0.0]).await.unwrap();
        assert_ne!(other, id);
    }

    #[tokio::test]
    async fn rename_moves_all_artifacts_when_target_is_free() {
        let dir = td();
        enroll(dir.path(), "ff32ce3w", Modality::Face, &[1.0, 0.0, 0.0, 0.0]).await.unwrap();
        enroll(dir.path(), "ff32ce3w", Modality::Voice, &[1.0, 0.0, 0.0, 0.0]).await.unwrap();
        facets::update_facet(dir.path(), "people", "ff32ce3w", "an unnamed face").await.unwrap();

        rename(dir.path(), "ff32ce3w", "赵力").await.unwrap();

        let people = layout::facets_dir(dir.path()).join("people");
        for f in ["赵力.face.f32", "赵力.voice.f32", "赵力.md"] {
            assert!(people.join(f).exists(), "{f} moved");
        }
        for f in ["ff32ce3w.face.f32", "ff32ce3w.voice.f32", "ff32ce3w.md"] {
            assert!(!people.join(f).exists(), "{f} gone");
        }
        // Recognition now answers with the name.
        let got = nearest(dir.path(), Modality::Face, &[1.0, 0.0, 0.0, 0.0], 1).await.unwrap();
        assert_eq!(got[0].subject, "赵力");
    }

    #[tokio::test]
    async fn rename_into_existing_merges_galleries() {
        let dir = td();
        enroll(dir.path(), "赵力", Modality::Face, &[1.0, 0.0, 0.0, 0.0]).await.unwrap();
        enroll(dir.path(), "dupe1234", Modality::Face, &[0.0, 1.0, 0.0, 0.0]).await.unwrap();
        rename(dir.path(), "dupe1234", "赵力").await.unwrap();

        let path = layout::facets_dir(dir.path()).join("people").join("赵力.face.f32");
        let len = std::fs::metadata(&path).unwrap().len() as usize;
        assert_eq!(len, 2 * 4 * std::mem::size_of::<f32>(), "both samples now under 赵力");
        assert!(!layout::facets_dir(dir.path()).join("people").join("dupe1234.face.f32").exists());
        // Either original observation now matches 赵力.
        for q in [[1.0, 0.0, 0.0, 0.0], [0.0, 1.0, 0.0, 0.0]] {
            let got = nearest(dir.path(), Modality::Face, &q, 1).await.unwrap();
            assert_eq!(got[0].subject, "赵力");
        }
    }
}
