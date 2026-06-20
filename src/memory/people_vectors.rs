//! Per-person voice/face embedding samples — the recognition sidecars of the
//! `people` facet dimension.
//!
//! Each person is a directory `memory/facets/people/<subject>/` (see
//! [`super::facets`]): its prose understanding is `facet.md`, and this module
//! stores their **embedding samples** right beside it as compact binary
//! `<modality>.f32` files (`face.f32`, `voice.f32`). It answers the one mechanical
//! question: *which known person is this query vector nearest to?*
//!
//! The `.f32` galleries are the recognition index. The **raw media** each sample
//! came from — face crops, voice turns — is kept separately under
//! `<subject>/<modality>/<id>.<ext>` ([`save_preview`]) purely so a cluster can be
//! eyeballed; it never enters matching.
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
//! Callers: the perception paths that produce embeddings — face recognition on
//! posted stills and camera-stream keyframes, voiceprints of posted clips and
//! live-mic speaker turns ([`crate::server`]) — and reflection clustering
//! ([`crate::reactor::heartbeat`]).

use std::path::{Path, PathBuf};

use uuid::Uuid;

use super::{facets, layout};

/// The facet dimension these sidecars attach to.
const DIM: &str = "people";

/// The directory under [`layout::facets_dir`] holding every person's subdir.
fn people_dir(data_dir: &Path) -> PathBuf {
    layout::facets_dir(data_dir).join(DIM)
}

/// Gallery filename for a modality inside a person's dir, e.g. `face.f32`.
fn gallery_file(modality: Modality) -> String {
    format!("{}.f32", modality.tag())
}

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

    let dir = people_dir(data_dir).join(&subj);
    tokio::fs::create_dir_all(&dir).await?;
    let file = gallery_file(modality);
    let path = dir.join(&file);

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

    let tmp = dir.join(format!(".{file}.tmp-{}", Uuid::now_v7().simple()));
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

/// Save the raw media a sample came from — a face crop, a voice turn — beside the
/// gallery so a cluster can be *eyeballed*, not just matched on its vectors. Lands
/// at `people/<subject>/<modality>/<id>.<ext>` and is capped to [`MAX_SAMPLES`]
/// files (oldest dropped), mirroring the gallery's bound. Pair it with [`assign`]:
/// assign returns the subject the embedding landed in, then save the preview under
/// that same subject. Deliberately decoupled from matching — purely for human
/// preview — so a failure here never affects recognition. The `id` is a fresh
/// time-ordered uuid, unrelated to any gallery offset.
pub async fn save_preview(
    data_dir: &Path,
    subject: &str,
    modality: Modality,
    bytes: &[u8],
    ext: &str,
) -> anyhow::Result<()> {
    let subj = facets::slug(subject);
    anyhow::ensure!(!subj.is_empty(), "subject must contain a usable character");
    anyhow::ensure!(!bytes.is_empty(), "preview must be non-empty");
    // Keep the extension a safe single path segment (it can come from an arbitrary
    // upload mime); fall back to a neutral one.
    let ext: String = ext.chars().filter(|c| c.is_ascii_alphanumeric()).collect();
    let ext = if ext.is_empty() { "bin".to_string() } else { ext };

    let dir = people_dir(data_dir).join(&subj).join(modality.tag());
    tokio::fs::create_dir_all(&dir).await?;
    let path = dir.join(format!("{}.{ext}", Uuid::now_v7().simple()));
    tokio::fs::write(&path, bytes).await?;
    prune_previews(&dir, MAX_SAMPLES).await?;
    Ok(())
}

/// Keep at most `max` preview files in `dir`, dropping the oldest. Names are
/// time-ordered uuids, so a lexical sort is chronological. Best-effort.
async fn prune_previews(dir: &Path, max: usize) -> anyhow::Result<()> {
    let mut names: Vec<String> = Vec::new();
    let mut rd = tokio::fs::read_dir(dir).await?;
    while let Some(ent) = rd.next_entry().await? {
        if let Ok(name) = ent.file_name().into_string()
            && !name.starts_with('.')
        {
            names.push(name);
        }
    }
    if names.len() <= max {
        return Ok(());
    }
    names.sort();
    let drop = names.len() - max;
    for name in &names[..drop] {
        let _ = tokio::fs::remove_file(dir.join(name)).await;
    }
    Ok(())
}

/// Move the `facets/people/<old>/` directory to `<new>/` — the structural side of
/// naming (rename a minted id to a learned name) and of merging (collapse two
/// clusters of one person). When `<new>/` is free the whole dir is renamed in one
/// step; when it already exists the two are merged artifact by artifact: a `.f32`
/// gallery present on both sides is concatenated (a later [`enroll`] re-applies the
/// cap), a preview dir (`face/`, `voice/`) has its uuid-named files moved over and
/// re-capped, any other file (the `facet.md` prose) keeps the target — it
/// regenerates from episodes — and the old is dropped. Renaming a subject onto
/// itself, or one with no directory, is a no-op.
pub async fn rename(data_dir: &Path, old: &str, new: &str) -> anyhow::Result<()> {
    let (old_s, new_s) = (facets::slug(old), facets::slug(new));
    anyhow::ensure!(!old_s.is_empty() && !new_s.is_empty(), "old and new must each slug to something");
    if old_s == new_s {
        return Ok(());
    }

    let dim_dir = people_dir(data_dir);
    let old_dir = dim_dir.join(&old_s);
    let new_dir = dim_dir.join(&new_s);
    if !tokio::fs::try_exists(&old_dir).await? {
        return Ok(()); // nothing to rename
    }
    if !tokio::fs::try_exists(&new_dir).await? {
        // Target free: a single directory rename moves prose + every gallery.
        if let Some(parent) = new_dir.parent() {
            tokio::fs::create_dir_all(parent).await?;
        }
        tokio::fs::rename(&old_dir, &new_dir).await?;
        return Ok(());
    }

    // Merge into the existing target. Collect the old dir's entries first, then
    // mutate, so we don't read and modify the directory at the same time.
    let mut rd = tokio::fs::read_dir(&old_dir).await?;
    let mut arts: Vec<(PathBuf, String)> = Vec::new();
    while let Some(ent) = rd.next_entry().await? {
        let Ok(fname) = ent.file_name().into_string() else {
            continue;
        };
        if fname.starts_with('.') {
            continue; // skip hidden/tmp half-writes
        }
        arts.push((ent.path(), fname));
    }
    drop(rd);

    for (src, fname) in arts {
        let dst = new_dir.join(&fname);
        let target_exists = tokio::fs::try_exists(&dst).await?;
        if src.is_dir() {
            // A preview directory (face/, voice/). Move its files into the target's
            // matching dir — names are unique uuids, so no collision — then re-cap.
            tokio::fs::create_dir_all(&dst).await?;
            let mut prd = tokio::fs::read_dir(&src).await?;
            while let Some(pent) = prd.next_entry().await? {
                if let Ok(pname) = pent.file_name().into_string()
                    && !pname.starts_with('.')
                    && !tokio::fs::try_exists(dst.join(&pname)).await?
                {
                    tokio::fs::rename(pent.path(), dst.join(&pname)).await?;
                }
            }
            drop(prd);
            prune_previews(&dst, MAX_SAMPLES).await?;
        } else if target_exists && fname.ends_with(".f32") {
            let mut merged = tokio::fs::read(&dst).await?;
            merged.extend(tokio::fs::read(&src).await?);
            let tmp = new_dir.join(format!(".{fname}.tmp-{}", Uuid::now_v7().simple()));
            tokio::fs::write(&tmp, &merged).await?;
            tokio::fs::rename(&tmp, &dst).await?;
        } else if !target_exists {
            tokio::fs::rename(&src, &dst).await?;
        }
        // else: keep the target (e.g. facet.md regenerates); the old copy is
        // dropped with the directory below.
    }
    // Drop the now-merged source directory and any leftover (hidden/kept) files.
    tokio::fs::remove_dir_all(&old_dir).await?;
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

    let dir = people_dir(data_dir);
    let file = gallery_file(modality);
    let mut rd = match tokio::fs::read_dir(&dir).await {
        Ok(rd) => rd,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(Vec::new()),
        Err(e) => return Err(e.into()),
    };

    let mut out: Vec<Candidate> = Vec::new();
    while let Some(ent) = rd.next_entry().await? {
        if !ent.file_type().await?.is_dir() {
            continue; // each subject is a directory; skip stray files
        }
        let Ok(subject) = ent.file_name().into_string() else {
            continue;
        };
        if subject.is_empty() || subject.starts_with('.') {
            continue;
        }
        // This subject's gallery for the modality; absent (person known only by
        // the other modality, or only by prose) → skip, not an error.
        let bytes = match tokio::fs::read(ent.path().join(&file)).await {
            Ok(b) => b,
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => continue,
            Err(e) => return Err(e.into()),
        };
        if let Some(best) = best_cosine(&bytes, query) {
            out.push(Candidate { subject, similarity: best });
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
        let path = layout::facets_dir(dir.path()).join("people").join("alice").join("voice.f32");
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
        for f in ["face.f32", "voice.f32", "facet.md"] {
            assert!(people.join("赵力").join(f).exists(), "{f} moved into 赵力/");
        }
        assert!(!people.join("ff32ce3w").exists(), "old id dir gone");
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

        let path = layout::facets_dir(dir.path()).join("people").join("赵力").join("face.f32");
        let len = std::fs::metadata(&path).unwrap().len() as usize;
        assert_eq!(len, 2 * 4 * std::mem::size_of::<f32>(), "both samples now under 赵力");
        assert!(!layout::facets_dir(dir.path()).join("people").join("dupe1234").exists());
        // Either original observation now matches 赵力.
        for q in [[1.0, 0.0, 0.0, 0.0], [0.0, 1.0, 0.0, 0.0]] {
            let got = nearest(dir.path(), Modality::Face, &q, 1).await.unwrap();
            assert_eq!(got[0].subject, "赵力");
        }
    }

    /// Count the non-hidden files in a subject's preview dir.
    async fn preview_count(data_dir: &Path, subject: &str, modality: Modality) -> usize {
        let dir = layout::facets_dir(data_dir).join("people").join(subject).join(modality.tag());
        let mut rd = match tokio::fs::read_dir(&dir).await {
            Ok(rd) => rd,
            Err(_) => return 0,
        };
        let mut n = 0;
        while let Some(e) = rd.next_entry().await.unwrap() {
            if !e.file_name().to_string_lossy().starts_with('.') {
                n += 1;
            }
        }
        n
    }

    #[tokio::test]
    async fn save_preview_keeps_a_capped_bag_separate_from_the_gallery() {
        let dir = td();
        for i in 0..(MAX_SAMPLES + 4) {
            save_preview(dir.path(), "Alice", Modality::Face, &[i as u8; 16], "jpg").await.unwrap();
        }
        assert_eq!(preview_count(dir.path(), "alice", Modality::Face).await, MAX_SAMPLES);
        // Previews never pollute the gallery index — no .f32 was written.
        assert!(nearest(dir.path(), Modality::Face, &[1.0, 0.0], 1).await.unwrap().is_empty());
        // ...nor the named-subject index (no facet.md prose yet).
        assert!(facets::facet_subject_index(dir.path()).await.unwrap().is_empty());
    }

    #[tokio::test]
    async fn rename_into_existing_merges_preview_bags() {
        let dir = td();
        for s in ["dupe1234", "赵力"] {
            enroll(dir.path(), s, Modality::Voice, &[1.0, 0.0, 0.0, 0.0]).await.unwrap();
            save_preview(dir.path(), s, Modality::Voice, &[7u8; 8], "wav").await.unwrap();
        }
        rename(dir.path(), "dupe1234", "赵力").await.unwrap();
        // Both clusters' previews now live under the surviving subject.
        assert_eq!(preview_count(dir.path(), "赵力", Modality::Voice).await, 2);
        assert!(!layout::facets_dir(dir.path()).join("people").join("dupe1234").exists());
    }
}
