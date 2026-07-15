//! Per-person voice/face recognition samples — the biometric sidecars of the
//! `people` facet dimension.
//!
//! Each person is a directory `memory/facets/people/<subject>/` (see
//! [`super::facets`]); their prose understanding is `facet.md`. This module owns
//! the **recognition samples** beside it and answers one mechanical question:
//! *which known person is this query vector nearest to?*
//!
//! A sample is a single observation, stored as a **pair sharing one uuid** in
//! `<subject>/<modality>/` (`face/`, `voice/`): the **media** it came from
//! (`<uuid>.jpg` face crop, `<uuid>.wav` voice turn) and its **embedding**
//! (`<uuid>.f32`, raw little-endian f32). The media is the canonical artifact — it
//! shows *whose* face/voice a cluster is, and an embedding can always be recomputed
//! from it (the capabilities do); the `.f32` is just a cached vector so matching
//! never re-runs a model. The two live and die together — dropping a sample deletes
//! both files — so the gallery is exactly 1:1: N embeddings ⇔ N crops.
//!
//! A gallery is a **bounded, diverse** set, not a log of every observation.
//! [`enroll`] keeps it that way: at most [`MAX_SAMPLES`] samples total, and at most
//! [`MAX_VARIANTS`] that are near-identical (cosine ≥ [`DEDUP_SIMILARITY`]) to one
//! another — so one long call full of near-duplicate frames can't crowd out genuine
//! variety. Past either bound the oldest is dropped (uuid v7 sorts chronologically).
//!
//! This is the **mechanical half of identity**: [`nearest`] returns ranked
//! *candidates* as evidence; the decision — same person? a new person? attach a
//! name? — is the agent's, deliberately ([[project-people-recognition-design]]).
//! Writes are atomic (temp sibling + rename) and last-writer-wins across scenes.
//!
//! Legacy: older galleries stored one packed `<modality>.f32` blob at the person
//! root, with no per-sample media link. [`nearest`] still reads it so recognition
//! keeps working; new samples are written in the per-sample form above, and the
//! blob is left to age out.
//!
//! Callers: the perception paths that produce embeddings — face recognition on
//! posted stills and camera-stream keyframes, voiceprints of posted clips and
//! live-mic speaker turns ([`crate::foundation::server`]) — and reflection clustering
//! ([`crate::body::reactor::heartbeat`]).

use std::path::{Path, PathBuf};

use uuid::Uuid;

use super::{facets, layout};

/// The facet dimension these sidecars attach to.
const DIM: &str = "people";

/// Cap on samples kept per subject per modality. A gallery is a *bounded, diverse*
/// set, not a log of every observation; this is the ceiling on its size.
const MAX_SAMPLES: usize = 1000;

/// How many near-identical samples (cosine ≥ [`DEDUP_SIMILARITY`]) to keep of any
/// one *look*. A few variants are useful (lighting, angle); beyond that they are
/// just one session crowding out diversity, so the oldest is rolled out.
const MAX_VARIANTS: usize = 3;

/// Cosine at/above which two samples count as the *same look* — essentially a
/// duplicate frame, not a new angle. Well above [`APPEND_THRESHOLD`] (same person);
/// a guess until validated on real embeddings. Shared across modalities, like
/// [`APPEND_THRESHOLD`].
const DEDUP_SIMILARITY: f32 = 0.85;

/// Cosine at/above which an observation is taken to be an existing person rather
/// than someone new (see [`assign`]). Conservative — minting a duplicate cluster
/// (mergeable later) is cheaper than wrongly fusing two people.
const APPEND_THRESHOLD: f32 = 0.5;

/// The directory under [`layout::facets_dir`] holding every person's subdir.
fn people_dir(data_dir: &Path) -> PathBuf {
    layout::facets_dir(data_dir).join(DIM)
}

/// The per-modality sample directory inside a person's dir (`face/`, `voice/`),
/// holding the `<uuid>.f32` embeddings and their `<uuid>.<ext>` media siblings.
fn modality_dir(data_dir: &Path, subject: &str, modality: Modality) -> PathBuf {
    people_dir(data_dir).join(subject).join(modality.tag())
}

/// Legacy packed-gallery filename at a person's root, e.g. `face.f32`. Read-only
/// now (back-compat); new samples are per-sample pairs under [`modality_dir`].
fn gallery_file(modality: Modality) -> String {
    format!("{}.f32", modality.tag())
}

/// Which embedding space a sample lives in. Voice and face occupy different spaces
/// and are never compared to each other, so each is its own subdirectory.
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

/// One ranked match: the facet subject (whose `facet.md` neighbour holds the
/// agent's prose understanding) and the best cosine similarity of the query against
/// any of that subject's samples, in `[-1, 1]`.
#[derive(Debug, Clone, PartialEq)]
pub struct Candidate {
    pub subject: String,
    pub similarity: f32,
}

/// One stored sample: its uuid stem (shared with the media sibling) and embedding.
struct Sample {
    stem: String,
    embedding: Vec<f32>,
}

/// Store one observation — its `embedding` and the `media` it came from (`ext` is
/// the media's extension, e.g. `"jpg"`/`"wav"`) — as a uuid-keyed pair under
/// `subject`'s `modality` dir, then re-apply the gallery's bounds. Returns the
/// canonical `people/<subject>` ref. The pair keeps the gallery 1:1 (one crop per
/// embedding); diversity/cap pruning ([`MAX_VARIANTS`]/[`MAX_SAMPLES`]) drops whole
/// pairs, oldest first. Media is written before the embedding so a crash leaves at
/// worst an unmatched media orphan, never an embedding pointing at missing media.
/// Errors if `subject` slugs to nothing, or the embedding or media is empty.
pub async fn enroll(
    data_dir: &Path,
    subject: &str,
    modality: Modality,
    embedding: &[f32],
    media: &[u8],
    ext: &str,
) -> anyhow::Result<String> {
    let subj = facets::slug(subject);
    anyhow::ensure!(!subj.is_empty(), "subject must contain a usable character");
    anyhow::ensure!(!embedding.is_empty(), "embedding must be non-empty");
    anyhow::ensure!(!media.is_empty(), "media must be non-empty");

    let dir = modality_dir(data_dir, &subj, modality);
    tokio::fs::create_dir_all(&dir).await?;

    // Existing samples, oldest first, then decide what the newcomer displaces.
    let existing = read_samples(&dir).await?;
    let embs: Vec<&[f32]> = existing.iter().map(|s| s.embedding.as_slice()).collect();
    for idx in plan_drops(&embs, embedding, DEDUP_SIMILARITY, MAX_VARIANTS, MAX_SAMPLES) {
        remove_sample(&dir, &existing[idx].stem).await;
    }

    let stem = Uuid::now_v7().simple().to_string();
    let ext = sanitize_ext(ext);
    write_atomic(&dir, &format!("{stem}.{ext}"), media).await?;
    let emb_bytes: Vec<u8> = embedding.iter().flat_map(|f| f.to_le_bytes()).collect();
    write_atomic(&dir, &format!("{stem}.f32"), &emb_bytes).await?;

    Ok(format!("{DIM}/{subj}"))
}

/// Decide which of `existing` (oldest first) the newcomer `new` displaces, by index.
/// Diversity first: if `new`'s look already has `max_variants` near-identical
/// samples (cosine ≥ `dedup`), drop the oldest of them down to that bound. Then the
/// global ceiling: with the newcomer added, drop oldest overall until `max_samples`.
/// Pure — the IO-free core of [`enroll`].
fn plan_drops(
    existing: &[&[f32]],
    new: &[f32],
    dedup: f32,
    max_variants: usize,
    max_samples: usize,
) -> Vec<usize> {
    let mut drops: Vec<usize> = Vec::new();
    let near: Vec<usize> = existing
        .iter()
        .enumerate()
        .filter(|(_, e)| cosine(e, new) >= dedup)
        .map(|(i, _)| i)
        .collect();
    if near.len() >= max_variants {
        let to_drop = near.len() - max_variants + 1; // leaves max_variants after adding the newcomer
        drops.extend(near.iter().take(to_drop).copied());
    }
    let mut remaining = existing.len() - drops.len() + 1;
    let mut i = 0;
    while remaining > max_samples && i < existing.len() {
        if !drops.contains(&i) {
            drops.push(i);
            remaining -= 1;
        }
        i += 1;
    }
    drops
}

/// A short, opaque, stable identity key for a freshly-discovered person — what a
/// cluster lives under until a real name is learned and it is [`rename`]d onto the
/// name. Eight base-36 chars from a v7 uuid's random low bits (e.g. `ff32ce3w`).
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

/// Place one observation — `embedding` plus the `media`/`ext` it came from — into
/// the people store: if it is within [`APPEND_THRESHOLD`] of an existing subject,
/// [`enroll`] it there and return that subject; otherwise [`mint_id`] a fresh id,
/// enroll under it, and return the id. This is the **mechanical half of
/// clustering** — identity forms from biometrics alone, no name or LLM. The
/// returned subject is an id (new person) or whatever key the matched cluster
/// currently has (an id, or a name if already named).
pub async fn assign(
    data_dir: &Path,
    modality: Modality,
    embedding: &[f32],
    media: &[u8],
    ext: &str,
) -> anyhow::Result<String> {
    if let Some(top) = nearest(data_dir, modality, embedding, 1).await?.into_iter().next()
        && top.similarity >= APPEND_THRESHOLD
    {
        enroll(data_dir, &top.subject, modality, embedding, media, ext).await?;
        return Ok(top.subject);
    }
    let id = mint_id();
    enroll(data_dir, &id, modality, embedding, media, ext).await?;
    Ok(id)
}

/// Move the `facets/people/<old>/` directory to `<new>/` — the structural side of
/// naming (rename a minted id to a learned name) and of merging (collapse two
/// clusters of one person). When `<new>/` is free the whole dir is renamed in one
/// step; when it already exists the two are merged artifact by artifact: a sample
/// dir (`face/`, `voice/`) has its uuid-named pairs moved over and re-capped, a
/// legacy `.f32` gallery present on both sides is concatenated, any other file (the
/// `facet.md` prose) keeps the target — it regenerates from episodes — and the old
/// is dropped. Renaming a subject onto itself, or one with no directory, is a no-op.
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
        // Target free: a single directory rename moves prose + every sample dir.
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
            // A sample dir (face/, voice/). Move its uuid-named pairs into the
            // target's matching dir — names are unique uuids, so no collision — then
            // re-cap by pairs so the merged gallery stays within MAX_SAMPLES.
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
            cap_samples(&dst, MAX_SAMPLES).await?;
        } else if target_exists && fname.ends_with(".f32") {
            // Legacy packed blobs on both sides: concatenate (a later enroll on the
            // surviving subject is unaffected — it writes per-sample pairs).
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

/// Delete every person's **voice** samples — the `voice/` sample dir and any legacy
/// `voice.f32` blob — across all `people/<subject>/`, leaving `face/`, `facet.md`,
/// and the (possibly named) subject dirs intact. One-shot maintenance to clear
/// voiceprint clusters contaminated before per-speaker span-slicing landed;
/// afterwards voice re-clusters cleanly from fresh observations. Returns how many
/// subjects had voice samples removed. A missing people dir is not an error.
pub async fn purge_voice(data_dir: &Path) -> anyhow::Result<usize> {
    let dir = people_dir(data_dir);
    let mut rd = match tokio::fs::read_dir(&dir).await {
        Ok(rd) => rd,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(0),
        Err(e) => return Err(e.into()),
    };
    let voice_file = gallery_file(Modality::Voice);
    let voice_tag = Modality::Voice.tag();
    let mut removed = 0;
    while let Some(ent) = rd.next_entry().await? {
        if !ent.file_type().await?.is_dir() {
            continue;
        }
        let subj = ent.path();
        let mut hit = false;
        match tokio::fs::remove_file(subj.join(&voice_file)).await {
            Ok(()) => hit = true,
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => {}
            Err(e) => return Err(e.into()),
        }
        match tokio::fs::remove_dir_all(subj.join(voice_tag)).await {
            Ok(()) => hit = true,
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => {}
            Err(e) => return Err(e.into()),
        }
        if hit {
            removed += 1;
        }
    }
    Ok(removed)
}

// ── Forgetting: let ambient, one-off clusters age out ────────────────────────
//
// Most voices and faces the agent meets are not people it needs to know — a
// stranger in a café, a character in a video the kid played, a passer-by on the
// street. Left alone they pile up: the store fills with single-shot noise, the
// calibration view drowns, and every video night dumps more of it. So unnamed
// clusters must be *biased to forget*; only the ones that **recur across time**
// earn the right to persist.
//
// Recurrence — not sample count — is what earns keeping. 601 voice samples from
// one bedtime-story video night are **one occasion**, not 601; a voice heard on
// three separate days is genuinely someone. Both signals are already on disk for
// free: every sample's filename is a uuid-v7 whose timestamp says *when* it was
// seen, so [`cluster_vitals`] reconstructs a cluster's whole timeline by reading
// stems — no schema, no salience field, no new store.
//
// The rule is deliberately gentle (see [[feedback-forgetting-keep-biased]]):
//   - **Named/modeled clusters (a `facet.md` exists) are never touched** — a name
//     is a human saying "this one matters", even at zero samples.
//   - A cluster seen on **≥ [`KEEP_OCCASIONS`] occasions** is a keeper, forever.
//   - Only a **single-occasion, unnamed** cluster is a candidate, and only after a
//     **[`FORGET_AFTER`] grace** since it was last seen. That is the video-night
//     stranger: one burst, never again, a month gone.
// Forgetting is a plain delete — if that person ever matters, they show up again
// and re-cluster from scratch, earning their keep the normal way. No archive.

/// Two samples of one cluster belong to the **same occasion** if their sightings
/// fall within this window; a larger gap starts a new occasion. So a long unbroken
/// session (one bedtime video, one call) counts once however many samples it left,
/// while re-encounters on separate days each add an occasion.
const OCCASION_GAP: chrono::Duration = chrono::Duration::minutes(30);

/// A cluster seen on at least this many distinct occasions is kept indefinitely —
/// it has recurred across time, so it is plausibly a real person, named or not. Two
/// is the gentle bar: seen on genuinely separate occasions even once is enough.
const KEEP_OCCASIONS: usize = 2;

/// Grace before a single-occasion, unnamed cluster becomes forgettable, measured
/// from its most recent sample. Gives a one-off encounter a month to recur before
/// it ages out.
const FORGET_AFTER: chrono::Duration = chrono::Duration::days(30);

/// A cluster's timeline, reconstructed from its sample stems — the inputs the
/// forgetting rule reasons over. Merges both modalities: a person seen once by face
/// and once by voice on the same evening is still one occasion.
#[derive(Debug, Clone, PartialEq)]
pub struct ClusterVitals {
    /// The cluster's directory name (a minted id, or a name once renamed).
    pub subject: String,
    /// Whether a prose `facet.md` exists — i.e. the mind has modeled this subject.
    /// Named/modeled clusters are exempt from forgetting.
    pub named: bool,
    /// Total samples across face + voice.
    pub samples: usize,
    /// Distinct occasions the cluster was seen, sightings split by [`OCCASION_GAP`].
    pub occasions: usize,
    /// Most recent sighting (newest stem), or `None` if the cluster has no
    /// timestamped samples (legacy blob-only, or empty).
    pub last_seen: Option<chrono::DateTime<chrono::Utc>>,
}

impl ClusterVitals {
    /// Whether this cluster may be forgotten as of `now`: unnamed, seen on fewer
    /// than [`KEEP_OCCASIONS`] occasions, and quiet for at least [`FORGET_AFTER`].
    /// A cluster with no datable samples is never forgettable here (nothing to age).
    pub fn forgettable(&self, now: chrono::DateTime<chrono::Utc>) -> bool {
        if self.named || self.occasions >= KEEP_OCCASIONS {
            return false;
        }
        match self.last_seen {
            Some(seen) => now - seen >= FORGET_AFTER,
            None => false,
        }
    }
}

/// The sighting time of a sample, decoded from its uuid-v7 stem (the stem *is* the
/// creation time). Delegates to the shared [`super::journal::uuidv7_ts`] so there is
/// one decoder; `None` if the stem isn't a v7 uuid.
fn stem_time(stem: &str) -> Option<chrono::DateTime<chrono::Utc>> {
    super::journal::uuidv7_ts(stem)
}

/// Count distinct occasions in a set of sighting times: sort, then start a new
/// occasion whenever the gap to the previous sighting exceeds [`OCCASION_GAP`].
/// Empty input is zero occasions.
fn count_occasions(mut times: Vec<chrono::DateTime<chrono::Utc>>) -> usize {
    times.sort_unstable();
    let mut occasions = 0;
    let mut prev: Option<chrono::DateTime<chrono::Utc>> = None;
    for t in times {
        if prev.is_none_or(|p| t - p > OCCASION_GAP) {
            occasions += 1;
        }
        prev = Some(t);
    }
    occasions
}

/// Read one cluster directory and reconstruct its [`ClusterVitals`] from the sample
/// stems across both modalities. `subject` is the directory name. Legacy packed
/// blobs contribute to the sample count but carry no per-sample time, so a
/// blob-only cluster reports `occasions == 0` / `last_seen == None` and is thus
/// never forgotten by [`sweep_forgettable`] — we don't age what we can't date.
async fn cluster_vitals(data_dir: &Path, subject: &str) -> anyhow::Result<ClusterVitals> {
    let mut times: Vec<chrono::DateTime<chrono::Utc>> = Vec::new();
    let mut samples = 0usize;
    for modality in [Modality::Face, Modality::Voice] {
        for s in read_samples(&modality_dir(data_dir, subject, modality)).await? {
            samples += 1;
            if let Some(t) = stem_time(&s.stem) {
                times.push(t);
            }
        }
    }
    let named = facets::read_facet(data_dir, DIM, subject).await?.is_some();
    let last_seen = times.iter().copied().max();
    let occasions = count_occasions(times);
    Ok(ClusterVitals { subject: subject.to_string(), named, samples, occasions, last_seen })
}

/// The outcome of one forgetting sweep: what was (or, in a dry run, would be)
/// forgotten, and how many clusters were examined.
#[derive(Debug, Default, Clone)]
pub struct ForgetReport {
    /// Clusters examined (every subject directory in the people store).
    pub examined: usize,
    /// The vitals of each cluster judged forgettable, for logging/inspection.
    pub forgotten: Vec<ClusterVitals>,
    /// Whether deletion actually happened. In a dry run `forgotten` is populated but
    /// nothing was removed.
    pub deleted: bool,
}

/// Walk the people store and forget every ambient, one-off cluster — unnamed, seen
/// on fewer than [`KEEP_OCCASIONS`] occasions, quiet for [`FORGET_AFTER`] as of
/// `now` (see [`ClusterVitals::forgettable`]). When `dry_run`, judges and reports
/// but deletes nothing — so a sweep can be watched before it is trusted. Named and
/// recurring clusters are left untouched; a missing people dir is not an error.
///
/// Folds into the reflection pass ([`crate::body::reactor::heartbeat`]) beside the
/// media [`super::decay`], on the same adaptive-backoff clock. Global, so it runs
/// once per consolidation, not per scene.
pub async fn sweep_forgettable(
    data_dir: &Path,
    now: chrono::DateTime<chrono::Utc>,
    dry_run: bool,
) -> anyhow::Result<ForgetReport> {
    let dir = people_dir(data_dir);
    let mut rd = match tokio::fs::read_dir(&dir).await {
        Ok(rd) => rd,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(ForgetReport::default()),
        Err(e) => return Err(e.into()),
    };

    let mut report = ForgetReport { deleted: !dry_run, ..Default::default() };
    // Collect subjects first, then act — don't mutate the dir while reading it.
    let mut subjects: Vec<String> = Vec::new();
    while let Some(ent) = rd.next_entry().await? {
        if !ent.file_type().await?.is_dir() {
            continue;
        }
        if let Ok(name) = ent.file_name().into_string()
            && !name.is_empty()
            && !name.starts_with('.')
        {
            subjects.push(name);
        }
    }
    drop(rd);

    for subject in subjects {
        report.examined += 1;
        let vitals = cluster_vitals(data_dir, &subject).await?;
        if !vitals.forgettable(now) {
            continue;
        }
        if !dry_run {
            tokio::fs::remove_dir_all(dir.join(&subject)).await?;
        }
        report.forgotten.push(vitals);
    }
    Ok(report)
}

// ── Re-clustering: de-mix a cluster the biometrics over-merged ────────────────
//
// A single cluster can hold *more than one person* — mostly voice (overlapping
// speech, similar timbre, imperfect diarization), sometimes faces (siblings, dim
// light, a crop spanning two faces). It is the same shape as the contamination in
// the 复盘 view — others fused into one identity — just from the source rather than
// a bad merge. The append threshold ([`APPEND_THRESHOLD`]) is deliberately loose,
// so an over-broad cluster usually still contains *tighter knots* of embeddings:
// re-cluster its own samples at a stricter threshold and the people fall apart.
//
// The threshold has no single right value (too loose → still one blob; too tight →
// one person shatters), so [`propose_split`] doesn't ask for one: it **sweeps**
// loose→tight and returns the loosest split that separates the cluster into ≥ 2
// groups, preferring 2–3 (a person mixed with one or two others is the common
// case) and hard-capped at [`MAX_SPLIT_GROUPS`] for the rare messy cluster. It
// **moves nothing** — it returns a proposal to preview; [`apply_split`] commits
// the human's accepted grouping by moving each group's sample pairs into a fresh
// minted cluster. This is the un-merge primitive that also repairs contamination:
// point it at a *named* cluster and the mis-merged samples split off to be renamed.

/// Loosest-to-tightest cosine thresholds [`propose_split`] tries. Starts at
/// [`APPEND_THRESHOLD`] (the clustering that produced the blob → one group) and
/// tightens; the first that yields ≥ 2 groups is the loosest real split.
const SPLIT_SWEEP: [f32; 9] = [0.5, 0.55, 0.6, 0.65, 0.7, 0.75, 0.8, 0.85, 0.9];

/// Hard ceiling on the auto-proposed group count — a backstop for genuinely messy
/// clusters (a party, a crowd) so they can still separate, not a target. The
/// preferred 2–3 finds an option almost always; the human can push higher in
/// preview if the samples really warrant it.
const MAX_SPLIT_GROUPS: usize = 10;

/// One proposed sub-group of a split: the sample stems (uuid pairs) that cluster
/// together at the chosen threshold. `stems` index into the modality's gallery.
#[derive(Debug, Clone, PartialEq)]
pub struct SplitGroup {
    pub stems: Vec<String>,
}

/// A proposed de-mixing of one cluster's `modality` gallery — several groups plus
/// any leftover singleton strays (lone samples that joined no group; likely outlier
/// frames, surfaced separately rather than inflating the group count). Empty
/// `groups` means the samples did not separate — treat as "one person".
#[derive(Debug, Clone, PartialEq)]
pub struct SplitProposal {
    pub subject: String,
    pub modality: Modality,
    pub groups: Vec<SplitGroup>,
    /// Stems that clustered alone at the chosen threshold — probable outliers, kept
    /// out of the group count so "2 people + 3 stray frames" reads as a 2-way split.
    pub strays: Vec<String>,
}

/// Single-linkage grouping of `embeddings` by index: two samples share a group if a
/// chain of pairwise cosines ≥ `threshold` connects them. Pure; O(n²) over the
/// cluster's own (small) gallery. Returns groups of indices, each sorted, the whole
/// stable by smallest member so results are deterministic.
fn cluster_indices(embeddings: &[&[f32]], threshold: f32) -> Vec<Vec<usize>> {
    let n = embeddings.len();
    // Union-find over sample indices.
    let mut parent: Vec<usize> = (0..n).collect();
    fn find(parent: &mut [usize], mut x: usize) -> usize {
        while parent[x] != x {
            parent[x] = parent[parent[x]];
            x = parent[x];
        }
        x
    }
    for i in 0..n {
        for j in (i + 1)..n {
            if cosine(embeddings[i], embeddings[j]) >= threshold {
                let (ri, rj) = (find(&mut parent, i), find(&mut parent, j));
                if ri != rj {
                    parent[ri] = rj;
                }
            }
        }
    }
    let mut by_root: std::collections::BTreeMap<usize, Vec<usize>> = std::collections::BTreeMap::new();
    for i in 0..n {
        let r = find(&mut parent, i);
        by_root.entry(r).or_default().push(i);
    }
    let mut groups: Vec<Vec<usize>> = by_root.into_values().collect();
    for g in &mut groups {
        g.sort_unstable();
    }
    groups.sort_by_key(|g| g[0]);
    groups
}

/// Propose how to de-mix `subject`'s `modality` gallery, without moving anything.
/// Sweeps [`SPLIT_SWEEP`] loose→tight and picks the loosest threshold whose
/// non-singleton group count is ≥ 2, capped at [`MAX_SPLIT_GROUPS`]; singletons at
/// that threshold become `strays`. Because group count only grows as the threshold
/// tightens, this loosest split lands in the common 2–3 range almost always.
/// Returns a proposal with empty `groups` when nothing separates (≤ 1 real group
/// at every tried threshold) — i.e. "this looks like one person". Errors only on IO.
pub async fn propose_split(
    data_dir: &Path,
    subject: &str,
    modality: Modality,
) -> anyhow::Result<SplitProposal> {
    let subj = facets::slug(subject);
    let samples = read_samples(&modality_dir(data_dir, &subj, modality)).await?;
    let embs: Vec<&[f32]> = samples.iter().map(|s| s.embedding.as_slice()).collect();

    let none = SplitProposal {
        subject: subj.clone(),
        modality,
        groups: Vec::new(),
        strays: Vec::new(),
    };
    if samples.len() < 2 {
        return Ok(none); // nothing to split
    }

    // Group count is monotonically non-decreasing as the threshold tightens, so the
    // *first* threshold that yields ≥ 2 real (size ≥ 2) groups is always the loosest
    // split — the one that keeps each person most whole. Take it, as long as it
    // hasn't already fragmented past the cap (a party/crowd edge case); looser than
    // it didn't separate at all, and tighter only fragments further, so there is no
    // better option to keep sweeping for. The preferred 2–3 range is where this
    // first split lands almost always; the cap is only a backstop.
    let mut chosen: Option<Vec<Vec<usize>>> = None;
    for &t in &SPLIT_SWEEP {
        let groups = cluster_indices(&embs, t);
        let real = groups.iter().filter(|g| g.len() >= 2).count();
        if real < 2 {
            continue; // didn't separate into ≥2 people at this tightness
        }
        if real <= MAX_SPLIT_GROUPS {
            chosen = Some(groups);
        }
        // Whether accepted or over the cap, this is the loosest split and no looser
        // one separated — stop either way (over-cap ⇒ leave `chosen` None ⇒ "one
        // person", better than a 20-way shatter).
        break;
    }

    let Some(groups) = chosen else {
        return Ok(none);
    };
    let mut out = SplitProposal { subject: subj, modality, groups: Vec::new(), strays: Vec::new() };
    for g in groups {
        let stems: Vec<String> = g.iter().map(|&i| samples[i].stem.clone()).collect();
        if stems.len() >= 2 {
            out.groups.push(SplitGroup { stems });
        } else {
            out.strays.extend(stems);
        }
    }
    Ok(out)
}

/// Commit a de-mixing: move every group in `groups` **except the largest** out of
/// `subject`'s `modality` gallery into a freshly [`mint_id`]'d cluster, one new
/// cluster per group, moving each stem's whole pair (embedding + media sibling).
/// The largest group stays in place under `subject` (so a named cluster keeps its
/// name and its dominant occupant). Stems not named in any group — strays and the
/// retained largest group — are left untouched. Returns the new cluster ids, in the
/// order of `groups` minus the retained one. A group naming an unknown stem is
/// skipped for that stem (best-effort, like the rest of the store).
///
/// Only the passed `modality` moves; a cluster mixing face + voice is de-mixed one
/// modality at a time (the two spaces don't compare, so a caller splits whichever
/// looks mixed). Cross-modal re-binding is out of scope here.
pub async fn apply_split(
    data_dir: &Path,
    subject: &str,
    modality: Modality,
    groups: &[SplitGroup],
) -> anyhow::Result<Vec<String>> {
    let subj = facets::slug(subject);
    anyhow::ensure!(!subj.is_empty(), "subject must slug to something");
    let src = modality_dir(data_dir, &subj, modality);
    if groups.len() < 2 {
        return Ok(Vec::new()); // nothing to separate
    }

    // Keep the largest group in place; move the rest out. Largest-stays means a
    // named cluster retains its name for its main occupant.
    let keep = groups
        .iter()
        .enumerate()
        .max_by_key(|(_, g)| g.stems.len())
        .map(|(i, _)| i)
        .unwrap_or(0);

    let mut new_ids = Vec::new();
    for (i, group) in groups.iter().enumerate() {
        if i == keep {
            continue;
        }
        let id = mint_id();
        let dst = modality_dir(data_dir, &id, modality);
        tokio::fs::create_dir_all(&dst).await?;
        for stem in &group.stems {
            move_stem(&src, &dst, stem).await;
        }
        new_ids.push(id);
    }
    Ok(new_ids)
}

/// Move a sample's whole pair (the `<stem>.f32` embedding and its media sibling)
/// from `src` to `dst`. Best-effort per file, mirroring [`remove_sample`]: names are
/// unique uuids so there is never a collision in `dst`.
async fn move_stem(src: &Path, dst: &Path, stem: &str) {
    let prefix = format!("{stem}.");
    if let Ok(mut rd) = tokio::fs::read_dir(src).await {
        while let Ok(Some(ent)) = rd.next_entry().await {
            if let Ok(name) = ent.file_name().into_string()
                && name.starts_with(&prefix)
            {
                let _ = tokio::fs::rename(ent.path(), dst.join(&name)).await;
            }
        }
    }
}

/// Rank known subjects by how close `query` is to their nearest `modality` sample
/// (the max cosine over that subject's samples), best first, capped at `k`. Reads
/// both the per-sample `<uuid>.f32` sidecars and any legacy packed `<modality>.f32`
/// blob. Subjects with no samples for this modality are skipped; a sample whose
/// dimension disagrees with the query contributes nothing (not fatal). Empty before
/// anyone is enrolled.
pub async fn nearest(
    data_dir: &Path,
    modality: Modality,
    query: &[f32],
    k: usize,
) -> anyhow::Result<Vec<Candidate>> {
    anyhow::ensure!(!query.is_empty(), "query must be non-empty");

    let dir = people_dir(data_dir);
    let legacy_file = gallery_file(modality);
    let tag = modality.tag();
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
        let person = ent.path();

        let mut best = f32::NEG_INFINITY;
        // Per-sample sidecars (the current form).
        for s in read_samples(&person.join(tag)).await? {
            let c = cosine(&s.embedding, query);
            if c > best {
                best = c;
            }
        }
        // Legacy packed blob (read-only back-compat); absent for new clusters.
        if let Ok(bytes) = tokio::fs::read(person.join(&legacy_file)).await
            && let Some(c) = best_cosine(&bytes, query)
            && c > best
        {
            best = c;
        }

        if best.is_finite() {
            out.push(Candidate { subject, similarity: best });
        }
    }

    out.sort_by(|a, b| {
        b.similarity.partial_cmp(&a.similarity).unwrap_or(std::cmp::Ordering::Equal)
    });
    out.truncate(k);
    Ok(out)
}

/// Read every `<uuid>.f32` sidecar in a modality dir as a [`Sample`], oldest first
/// (uuid v7 stems sort chronologically). Skips media files, hidden/tmp files, and
/// any sidecar whose bytes don't divide into f32s. A missing dir is empty.
async fn read_samples(dir: &Path) -> anyhow::Result<Vec<Sample>> {
    let mut out: Vec<Sample> = Vec::new();
    let mut rd = match tokio::fs::read_dir(dir).await {
        Ok(rd) => rd,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(out),
        Err(e) => return Err(e.into()),
    };
    while let Some(ent) = rd.next_entry().await? {
        let Ok(name) = ent.file_name().into_string() else {
            continue;
        };
        if name.starts_with('.') {
            continue;
        }
        let Some(stem) = name.strip_suffix(".f32") else {
            continue; // a media sibling, not an embedding
        };
        let bytes = tokio::fs::read(ent.path()).await?;
        if bytes.is_empty() || bytes.len() % 4 != 0 {
            continue;
        }
        let embedding: Vec<f32> = bytes
            .chunks_exact(4)
            .map(|b| f32::from_le_bytes([b[0], b[1], b[2], b[3]]))
            .collect();
        out.push(Sample { stem: stem.to_string(), embedding });
    }
    out.sort_by(|a, b| a.stem.cmp(&b.stem));
    Ok(out)
}

/// Keep at most `max` samples in a modality dir, dropping whole oldest pairs.
async fn cap_samples(dir: &Path, max: usize) -> anyhow::Result<()> {
    let samples = read_samples(dir).await?;
    if samples.len() <= max {
        return Ok(());
    }
    for s in &samples[..samples.len() - max] {
        remove_sample(dir, &s.stem).await;
    }
    Ok(())
}

/// Delete a sample's pair from `dir`: the embedding sidecar first (so it stops
/// matching at once), then any media sibling sharing the `stem`. Best-effort.
async fn remove_sample(dir: &Path, stem: &str) {
    let _ = tokio::fs::remove_file(dir.join(format!("{stem}.f32"))).await;
    let prefix = format!("{stem}.");
    if let Ok(mut rd) = tokio::fs::read_dir(dir).await {
        while let Ok(Some(ent)) = rd.next_entry().await {
            if let Ok(name) = ent.file_name().into_string()
                && name.starts_with(&prefix)
            {
                let _ = tokio::fs::remove_file(ent.path()).await;
            }
        }
    }
}

/// Write `bytes` to `dir/fname` atomically: a temp sibling is renamed into place, so
/// a reader never sees a torn file. `dir` must already exist.
async fn write_atomic(dir: &Path, fname: &str, bytes: &[u8]) -> anyhow::Result<()> {
    let tmp = dir.join(format!(".{fname}.tmp-{}", Uuid::now_v7().simple()));
    tokio::fs::write(&tmp, bytes).await?;
    tokio::fs::rename(&tmp, dir.join(fname)).await?;
    Ok(())
}

/// Keep a media extension a safe single path segment (it can come from an arbitrary
/// upload mime): ascii-alphanumeric only, with a neutral fallback.
fn sanitize_ext(ext: &str) -> String {
    let e: String = ext.chars().filter(|c| c.is_ascii_alphanumeric()).collect();
    if e.is_empty() { "bin".to_string() } else { e }
}

/// Cosine similarity of two equal-length vectors, in `[-1, 1]`. `0.0` if the lengths
/// differ (a model/dim mismatch — they simply don't match) or either is a zero
/// vector.
fn cosine(a: &[f32], b: &[f32]) -> f32 {
    if a.len() != b.len() {
        return 0.0;
    }
    let (na, nb) = (norm(a), norm(b));
    if na == 0.0 || nb == 0.0 {
        return 0.0;
    }
    a.iter().zip(b).map(|(x, y)| x * y).sum::<f32>() / (na * nb)
}

/// Max cosine of `query` against each fixed-length sample packed in `bytes` (the
/// legacy blob form). `None` if `bytes` is empty, doesn't divide into query-sized
/// samples (corrupt or wrong-dim — skipped, not fatal), or the query is a zero
/// vector.
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

    /// Enroll an embedding with a tiny dummy media sibling — the common test shape.
    async fn enroll_v(dir: &Path, subject: &str, modality: Modality, emb: &[f32]) -> String {
        let ext = if modality == Modality::Face { "jpg" } else { "wav" };
        enroll(dir, subject, modality, emb, b"media", ext).await.unwrap()
    }

    /// Count a subject's `.f32` sidecars and its media siblings in a modality dir.
    async fn sample_media_counts(data_dir: &Path, subject: &str, modality: Modality) -> (usize, usize) {
        let dir = modality_dir(data_dir, subject, modality);
        let (mut sidecars, mut media) = (0usize, 0usize);
        let mut rd = match tokio::fs::read_dir(&dir).await {
            Ok(rd) => rd,
            Err(_) => return (0, 0),
        };
        while let Some(e) = rd.next_entry().await.unwrap() {
            let name = e.file_name().to_string_lossy().into_owned();
            if name.starts_with('.') {
                continue;
            }
            if name.ends_with(".f32") {
                sidecars += 1;
            } else {
                media += 1;
            }
        }
        (sidecars, media)
    }

    #[tokio::test]
    async fn enroll_then_nearest_finds_the_subject() {
        let dir = td();
        let r = enroll_v(dir.path(), "Alice", Modality::Voice, &[1.0, 0.0, 0.0, 0.0]).await;
        assert_eq!(r, "people/alice");
        let got = nearest(dir.path(), Modality::Voice, &[1.0, 0.0, 0.0, 0.0], 5).await.unwrap();
        assert_eq!(got.len(), 1);
        assert_eq!(got[0].subject, "alice");
        assert!((got[0].similarity - 1.0).abs() < 1e-6, "sim {}", got[0].similarity);
    }

    #[tokio::test]
    async fn enroll_writes_a_one_to_one_media_pair() {
        let dir = td();
        enroll(dir.path(), "Alice", Modality::Face, &[1.0, 0.0], b"jpgbytes", "jpg").await.unwrap();
        assert_eq!(sample_media_counts(dir.path(), "alice", Modality::Face).await, (1, 1));
    }

    #[tokio::test]
    async fn same_subject_ranks_above_different() {
        let dir = td();
        enroll_v(dir.path(), "Alice", Modality::Voice, &[1.0, 0.0, 0.0, 0.0]).await;
        enroll_v(dir.path(), "Bob", Modality::Voice, &[0.0, 1.0, 0.0, 0.0]).await;
        let got = nearest(dir.path(), Modality::Voice, &[0.9, 0.1, 0.0, 0.0], 5).await.unwrap();
        assert_eq!(got[0].subject, "alice");
        assert!(got[0].similarity > got[1].similarity);
    }

    #[tokio::test]
    async fn nearest_takes_the_max_over_a_subjects_samples() {
        let dir = td();
        // Two orthogonal looks (not near-duplicates) — both kept.
        enroll_v(dir.path(), "Alice", Modality::Voice, &[1.0, 0.0, 0.0, 0.0]).await;
        enroll_v(dir.path(), "Alice", Modality::Voice, &[0.0, 0.0, 1.0, 0.0]).await;
        let got = nearest(dir.path(), Modality::Voice, &[0.0, 0.0, 1.0, 0.0], 5).await.unwrap();
        assert_eq!(got.len(), 1, "one subject, two samples");
        assert!((got[0].similarity - 1.0).abs() < 1e-6);
    }

    #[tokio::test]
    async fn k_caps_the_result_count() {
        let dir = td();
        for name in ["Alice", "Bob", "Carol"] {
            enroll_v(dir.path(), name, Modality::Voice, &[1.0, 0.0, 0.0, 0.0]).await;
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
        enroll_v(dir.path(), "Alice", Modality::Voice, &[1.0, 0.0, 0.0, 0.0]).await;
        enroll_v(dir.path(), "Bob", Modality::Face, &[1.0, 0.0, 0.0, 0.0]).await;
        let voice = nearest(dir.path(), Modality::Voice, &[1.0, 0.0, 0.0, 0.0], 5).await.unwrap();
        let face = nearest(dir.path(), Modality::Face, &[1.0, 0.0, 0.0, 0.0], 5).await.unwrap();
        assert_eq!(voice.iter().map(|c| c.subject.as_str()).collect::<Vec<_>>(), vec!["alice"]);
        assert_eq!(face.iter().map(|c| c.subject.as_str()).collect::<Vec<_>>(), vec!["bob"]);
    }

    #[tokio::test]
    async fn samples_do_not_pollute_the_facet_subject_index() {
        let dir = td();
        enroll_v(dir.path(), "Alice", Modality::Voice, &[1.0, 0.0, 0.0, 0.0]).await;
        // A bare sample (no prose facet yet) must not look like a subject.
        assert!(facets::facet_subject_index(dir.path()).await.unwrap().is_empty());
        // Once a prose facet exists, the subject shows up.
        facets::update_facet(dir.path(), "people", "Alice", "x").await.unwrap();
        assert_eq!(facets::facet_subject_index(dir.path()).await.unwrap(), vec!["people/alice"]);
    }

    #[tokio::test]
    async fn empty_subject_embedding_or_media_is_rejected() {
        let dir = td();
        assert!(enroll(dir.path(), "??", Modality::Voice, &[1.0], b"m", "wav").await.is_err());
        assert!(enroll(dir.path(), "Alice", Modality::Voice, &[], b"m", "wav").await.is_err());
        assert!(enroll(dir.path(), "Alice", Modality::Voice, &[1.0], b"", "wav").await.is_err());
    }

    #[tokio::test]
    async fn near_identical_samples_cap_at_max_variants_keeping_pairs() {
        let dir = td();
        // Several identical observations (cosine 1.0 ≥ DEDUP) of one look.
        for _ in 0..(MAX_VARIANTS + 3) {
            enroll_v(dir.path(), "Alice", Modality::Face, &[1.0, 0.0, 0.0, 0.0]).await;
        }
        // Only a few variants are kept — and crops track embeddings 1:1.
        assert_eq!(
            sample_media_counts(dir.path(), "alice", Modality::Face).await,
            (MAX_VARIANTS, MAX_VARIANTS)
        );
    }

    #[tokio::test]
    async fn distinct_looks_are_all_kept() {
        let dir = td();
        // Four orthogonal looks — none near another, all under MAX_SAMPLES.
        for one_hot in [
            [1.0, 0.0, 0.0, 0.0],
            [0.0, 1.0, 0.0, 0.0],
            [0.0, 0.0, 1.0, 0.0],
            [0.0, 0.0, 0.0, 1.0],
        ] {
            enroll_v(dir.path(), "Alice", Modality::Face, &one_hot).await;
        }
        assert_eq!(sample_media_counts(dir.path(), "alice", Modality::Face).await, (4, 4));
    }

    #[test]
    fn plan_drops_rolls_the_oldest_variant_when_a_look_is_full() {
        // Three near-identical (cosine ~1) + a newcomer near them, max_variants 3.
        let a = [1.0_f32, 0.0];
        let existing: Vec<&[f32]> = vec![&a, &a, &a];
        let drops = plan_drops(&existing, &a, 0.85, 3, 1000);
        assert_eq!(drops, vec![0], "drop the single oldest variant");
    }

    #[test]
    fn plan_drops_keeps_a_look_below_the_variant_bound() {
        let a = [1.0_f32, 0.0];
        let existing: Vec<&[f32]> = vec![&a, &a];
        assert!(plan_drops(&existing, &a, 0.85, 3, 1000).is_empty(), "2 < 3 variants, keep all");
    }

    #[test]
    fn plan_drops_trims_oldest_overall_at_the_global_cap() {
        // Distinct (orthogonal) looks so none are near; cap forces oldest out.
        let vs: Vec<Vec<f32>> = (0..6)
            .map(|i| (0..6).map(|j| if i == j { 1.0 } else { 0.0 }).collect())
            .collect();
        let existing: Vec<&[f32]> = vs.iter().map(|v| v.as_slice()).collect();
        let newcomer = vec![0.5_f32; 6];
        // max_samples 4: with the newcomer there'd be 7 → drop the 3 oldest.
        assert_eq!(plan_drops(&existing, &newcomer, 0.85, 3, 4), vec![0, 1, 2]);
    }

    #[tokio::test]
    async fn assign_mints_on_empty_then_appends_close_and_mints_far() {
        let dir = td();
        // Empty store → mints a fresh id and stores it.
        let id = assign(dir.path(), Modality::Face, &[1.0, 0.0, 0.0, 0.0], b"m", "jpg").await.unwrap();
        assert_eq!(id.len(), 8);
        // A near-identical observation → appends to the same id (not a new one).
        let again = assign(dir.path(), Modality::Face, &[0.98, 0.0, 0.0, 0.0], b"m", "jpg").await.unwrap();
        assert_eq!(again, id);
        // An orthogonal observation (cosine 0 < threshold) → a new id.
        let other = assign(dir.path(), Modality::Face, &[0.0, 1.0, 0.0, 0.0], b"m", "jpg").await.unwrap();
        assert_ne!(other, id);
    }

    #[test]
    fn mint_id_is_eight_base36_chars() {
        let id = mint_id();
        assert_eq!(id.len(), 8);
        assert!(id.chars().all(|c| c.is_ascii_lowercase() || c.is_ascii_digit()));
        assert_ne!(mint_id(), mint_id(), "ids are not constant");
    }

    #[tokio::test]
    async fn rename_moves_all_artifacts_when_target_is_free() {
        let dir = td();
        enroll_v(dir.path(), "ff32ce3w", Modality::Face, &[1.0, 0.0, 0.0, 0.0]).await;
        enroll_v(dir.path(), "ff32ce3w", Modality::Voice, &[1.0, 0.0, 0.0, 0.0]).await;
        facets::update_facet(dir.path(), "people", "ff32ce3w", "an unnamed face").await.unwrap();

        rename(dir.path(), "ff32ce3w", "赵力").await.unwrap();

        let people = layout::facets_dir(dir.path()).join("people");
        assert!(people.join("赵力").join("facet.md").exists(), "prose moved");
        assert_eq!(sample_media_counts(dir.path(), "赵力", Modality::Face).await, (1, 1), "face pair moved");
        assert_eq!(sample_media_counts(dir.path(), "赵力", Modality::Voice).await, (1, 1), "voice pair moved");
        assert!(!people.join("ff32ce3w").exists(), "old id dir gone");
        // Recognition now answers with the name.
        let got = nearest(dir.path(), Modality::Face, &[1.0, 0.0, 0.0, 0.0], 1).await.unwrap();
        assert_eq!(got[0].subject, "赵力");
    }

    #[tokio::test]
    async fn rename_into_existing_merges_samples() {
        let dir = td();
        enroll_v(dir.path(), "赵力", Modality::Face, &[1.0, 0.0, 0.0, 0.0]).await;
        enroll_v(dir.path(), "dupe1234", Modality::Face, &[0.0, 1.0, 0.0, 0.0]).await;
        rename(dir.path(), "dupe1234", "赵力").await.unwrap();

        assert_eq!(sample_media_counts(dir.path(), "赵力", Modality::Face).await, (2, 2), "both pairs under 赵力");
        assert!(!layout::facets_dir(dir.path()).join("people").join("dupe1234").exists());
        // Either original observation now matches 赵力.
        for q in [[1.0, 0.0, 0.0, 0.0], [0.0, 1.0, 0.0, 0.0]] {
            let got = nearest(dir.path(), Modality::Face, &q, 1).await.unwrap();
            assert_eq!(got[0].subject, "赵力");
        }
    }

    #[tokio::test]
    async fn purge_voice_removes_voice_keeps_face_and_prose() {
        let dir = td();
        enroll_v(dir.path(), "赵力", Modality::Face, &[1.0, 0.0, 0.0, 0.0]).await;
        enroll_v(dir.path(), "赵力", Modality::Voice, &[1.0, 0.0, 0.0, 0.0]).await;
        facets::update_facet(dir.path(), "people", "赵力", "prose").await.unwrap();

        assert_eq!(purge_voice(dir.path()).await.unwrap(), 1);

        let subj = layout::facets_dir(dir.path()).join("people").join("赵力");
        assert!(!subj.join("voice").exists(), "voice samples removed");
        assert!(subj.join("face").exists(), "face samples kept");
        assert!(subj.join("facet.md").exists(), "prose kept");
        // Voice no longer matches; the named person still recognizes by face.
        assert!(nearest(dir.path(), Modality::Voice, &[1.0, 0.0, 0.0, 0.0], 1).await.unwrap().is_empty());
        assert_eq!(
            nearest(dir.path(), Modality::Face, &[1.0, 0.0, 0.0, 0.0], 1).await.unwrap()[0].subject,
            "赵力"
        );
    }

    #[tokio::test]
    async fn purge_voice_on_empty_store_is_noop() {
        let dir = td();
        assert_eq!(purge_voice(dir.path()).await.unwrap(), 0);
    }

    #[tokio::test]
    async fn nearest_still_reads_a_legacy_packed_blob() {
        let dir = td();
        // Simulate an old-format gallery: a packed root blob, no per-sample dir.
        let person = layout::facets_dir(dir.path()).join("people").join("legacy");
        tokio::fs::create_dir_all(&person).await.unwrap();
        let emb: [f32; 4] = [1.0, 0.0, 0.0, 0.0];
        let bytes: Vec<u8> = emb.iter().flat_map(|f| f.to_le_bytes()).collect();
        tokio::fs::write(person.join("face.f32"), &bytes).await.unwrap();
        let got = nearest(dir.path(), Modality::Face, &emb, 1).await.unwrap();
        assert_eq!(got[0].subject, "legacy");
        assert!((got[0].similarity - 1.0).abs() < 1e-6);
    }

    // ── Forgetting ───────────────────────────────────────────────────────────

    /// A fixed clock so tests don't depend on wall-time.
    fn t0() -> chrono::DateTime<chrono::Utc> {
        chrono::DateTime::from_timestamp(1_700_000_000, 0).unwrap()
    }

    /// A uuid-v7 whose embedded timestamp is exactly `at` — lets a test place a
    /// sample at a chosen sighting time (the real `enroll` always stamps "now").
    fn uuid_at(at: chrono::DateTime<chrono::Utc>) -> String {
        let ts = uuid::Timestamp::from_unix(
            uuid::NoContext,
            at.timestamp() as u64,
            at.timestamp_subsec_nanos(),
        );
        Uuid::new_v7(ts).simple().to_string()
    }

    /// Write one backdated sample pair (embedding + media) directly, bypassing
    /// `enroll` so the stem carries `at` rather than "now". Distinct embeddings via
    /// `seed` keep dedup/variant capping from collapsing them.
    async fn place_sample(
        data_dir: &Path,
        subject: &str,
        modality: Modality,
        at: chrono::DateTime<chrono::Utc>,
        seed: f32,
    ) {
        let dir = modality_dir(data_dir, &facets::slug(subject), modality);
        tokio::fs::create_dir_all(&dir).await.unwrap();
        let stem = uuid_at(at);
        let emb = [seed, 1.0 - seed];
        let emb_bytes: Vec<u8> = emb.iter().flat_map(|f| f.to_le_bytes()).collect();
        tokio::fs::write(dir.join(format!("{stem}.f32")), &emb_bytes).await.unwrap();
        tokio::fs::write(dir.join(format!("{stem}.wav")), b"m").await.unwrap();
    }

    #[test]
    fn count_occasions_groups_a_burst_and_splits_days() {
        let base = t0();
        // A single burst: 5 sightings seconds apart → one occasion.
        let burst: Vec<_> = (0..5).map(|i| base + chrono::Duration::seconds(i * 3)).collect();
        assert_eq!(count_occasions(burst), 1);
        // Three sightings on separate days → three occasions.
        let days: Vec<_> =
            (0..3).map(|i| base + chrono::Duration::days(i)).collect();
        assert_eq!(count_occasions(days), 3);
        // A gap just over the window opens a new occasion; just under does not.
        assert_eq!(count_occasions(vec![base, base + OCCASION_GAP + chrono::Duration::seconds(1)]), 2);
        assert_eq!(count_occasions(vec![base, base + OCCASION_GAP - chrono::Duration::seconds(1)]), 1);
        assert_eq!(count_occasions(vec![]), 0);
    }

    #[test]
    fn stem_time_round_trips_a_backdated_uuid() {
        let at = t0() + chrono::Duration::days(3);
        let stem = uuid_at(at);
        let got = stem_time(&stem).expect("v7 stem has a timestamp");
        // uuid-v7 carries millisecond precision — allow a millisecond of slack.
        assert!((got - at).num_milliseconds().abs() <= 1, "got {got}, want {at}");
        assert!(stem_time("not-a-uuid").is_none());
    }

    #[tokio::test]
    async fn one_off_stranger_ages_out_after_the_grace() {
        let dir = td();
        let now = t0();
        // Seen once, 40 days ago — a single-occasion unnamed cluster past its grace.
        place_sample(dir.path(), "2xk04cyd", Modality::Voice, now - chrono::Duration::days(40), 0.1).await;

        let v = cluster_vitals(dir.path(), "2xk04cyd").await.unwrap();
        assert_eq!(v.occasions, 1);
        assert!(!v.named);
        assert!(v.forgettable(now), "one-off past grace should be forgettable");

        let report = sweep_forgettable(dir.path(), now, false).await.unwrap();
        assert!(report.deleted);
        assert_eq!(report.forgotten.len(), 1);
        assert_eq!(report.forgotten[0].subject, "2xk04cyd");
        // The directory is gone.
        assert!(!tokio::fs::try_exists(people_dir(dir.path()).join("2xk04cyd")).await.unwrap());
    }

    #[tokio::test]
    async fn a_recurring_cluster_is_kept() {
        let dir = td();
        let now = t0();
        // Same unnamed id, but seen on three separate days → keeper.
        for d in [40, 25, 10] {
            place_sample(dir.path(), "ydeeeu6v", Modality::Voice, now - chrono::Duration::days(d), 0.2).await;
        }
        let v = cluster_vitals(dir.path(), "ydeeeu6v").await.unwrap();
        assert_eq!(v.occasions, 3);
        assert!(!v.forgettable(now));

        let report = sweep_forgettable(dir.path(), now, false).await.unwrap();
        assert!(report.forgotten.is_empty());
        assert!(tokio::fs::try_exists(people_dir(dir.path()).join("ydeeeu6v")).await.unwrap());
    }

    #[tokio::test]
    async fn a_recent_one_off_is_kept_until_the_grace_passes() {
        let dir = td();
        let now = t0();
        // Single occasion, but only 5 days ago — still within grace.
        place_sample(dir.path(), "sgstq9sb", Modality::Face, now - chrono::Duration::days(5), 0.3).await;
        let v = cluster_vitals(dir.path(), "sgstq9sb").await.unwrap();
        assert_eq!(v.occasions, 1);
        assert!(!v.forgettable(now), "within grace, keep it");
        let report = sweep_forgettable(dir.path(), now, false).await.unwrap();
        assert!(report.forgotten.is_empty());
    }

    #[tokio::test]
    async fn a_named_cluster_is_never_forgotten_even_stale_and_one_off() {
        let dir = td();
        let now = t0();
        // Named (has a facet.md), seen once long ago — exactly 糯米's shape.
        place_sample(dir.path(), "糯米", Modality::Voice, now - chrono::Duration::days(200), 0.4).await;
        facets::update_facet(dir.path(), DIM, "糯米", "女儿").await.unwrap();
        let v = cluster_vitals(dir.path(), "糯米").await.unwrap();
        assert!(v.named);
        assert!(!v.forgettable(now), "a name means keep");
        let report = sweep_forgettable(dir.path(), now, false).await.unwrap();
        assert!(report.forgotten.is_empty());
        assert!(tokio::fs::try_exists(people_dir(dir.path()).join("糯米")).await.unwrap());
    }

    #[tokio::test]
    async fn a_burst_of_many_samples_in_one_night_is_still_one_occasion() {
        let dir = td();
        let now = t0();
        // The 7/10 case in miniature: many samples, all one evening, 40 days ago.
        let night = now - chrono::Duration::days(40);
        for i in 0..30 {
            place_sample(dir.path(), "b4gdp0hu", Modality::Voice, night + chrono::Duration::seconds(i * 20), i as f32 / 100.0).await;
        }
        let v = cluster_vitals(dir.path(), "b4gdp0hu").await.unwrap();
        assert!(v.samples >= 3, "kept several samples: {}", v.samples);
        assert_eq!(v.occasions, 1, "one night = one occasion regardless of sample count");
        assert!(v.forgettable(now), "a one-night burst, gone cold, ages out");
    }

    #[tokio::test]
    async fn dry_run_reports_but_deletes_nothing() {
        let dir = td();
        let now = t0();
        place_sample(dir.path(), "urwmpurn", Modality::Voice, now - chrono::Duration::days(40), 0.5).await;
        let report = sweep_forgettable(dir.path(), now, true).await.unwrap();
        assert!(!report.deleted);
        assert_eq!(report.forgotten.len(), 1, "still reported as forgettable");
        // ...but the cluster is untouched.
        assert!(tokio::fs::try_exists(people_dir(dir.path()).join("urwmpurn")).await.unwrap());
    }

    #[tokio::test]
    async fn a_face_and_voice_on_one_evening_count_as_one_occasion() {
        let dir = td();
        let now = t0();
        let evening = now - chrono::Duration::days(40);
        place_sample(dir.path(), "e1y8mx6b", Modality::Face, evening, 0.1).await;
        place_sample(dir.path(), "e1y8mx6b", Modality::Voice, evening + chrono::Duration::minutes(2), 0.6).await;
        let v = cluster_vitals(dir.path(), "e1y8mx6b").await.unwrap();
        assert_eq!(v.samples, 2);
        assert_eq!(v.occasions, 1, "cross-modality sightings close in time are one occasion");
    }

    #[tokio::test]
    async fn a_legacy_blob_only_cluster_is_never_forgotten() {
        let dir = td();
        let now = t0();
        // No datable per-sample stems — only a packed blob. We can't age it, so keep.
        let person = people_dir(dir.path()).join("legacyonly");
        tokio::fs::create_dir_all(&person).await.unwrap();
        let bytes: Vec<u8> = [1.0f32, 0.0].iter().flat_map(|f| f.to_le_bytes()).collect();
        tokio::fs::write(person.join("voice.f32"), &bytes).await.unwrap();
        let v = cluster_vitals(dir.path(), "legacyonly").await.unwrap();
        assert_eq!(v.occasions, 0);
        assert_eq!(v.last_seen, None);
        assert!(!v.forgettable(now));
        let report = sweep_forgettable(dir.path(), now, false).await.unwrap();
        assert!(report.forgotten.is_empty());
    }

    #[tokio::test]
    async fn sweep_on_empty_store_is_noop() {
        let dir = td();
        let report = sweep_forgettable(dir.path(), t0(), false).await.unwrap();
        assert_eq!(report.examined, 0);
        assert!(report.forgotten.is_empty());
    }

    // ── Re-clustering / split ─────────────────────────────────────────────────

    /// A near-`axis` embedding (unit-ish, small jitter via `k`) so a group of them
    /// is mutually cosine ≈ 1 but distinct from another axis.
    fn near(axis: usize, k: f32) -> Vec<f32> {
        let mut v = vec![0.0f32; 4];
        v[axis] = 1.0;
        v[(axis + 1) % 4] = k * 0.05; // tiny off-axis wobble, well within any group
        v
    }

    #[test]
    fn cluster_indices_splits_two_tight_knots() {
        // Three near +x, two near +y — one loose blob, two tight knots.
        let a = [near(0, 0.0), near(0, 1.0), near(0, 2.0), near(1, 0.0), near(1, 1.0)];
        let refs: Vec<&[f32]> = a.iter().map(|v| v.as_slice()).collect();
        // Loose: everything is one group (all cosine ≥ 0.5? +x·+y = 0 < 0.5, so two).
        let g = cluster_indices(&refs, 0.6);
        assert_eq!(g.len(), 2, "two knots at a tight-enough threshold");
        assert_eq!(g[0], vec![0, 1, 2]);
        assert_eq!(g[1], vec![3, 4]);
    }

    #[tokio::test]
    async fn propose_split_finds_two_people_in_a_mixed_voice_cluster() {
        let dir = td();
        // A single unnamed cluster whose voice gallery is really two speakers:
        // three samples near +x, two near +y.
        for v in [near(0, 0.0), near(0, 1.0), near(0, 2.0), near(1, 0.0), near(1, 1.0)] {
            enroll_v(dir.path(), "mixedvox", Modality::Voice, &v).await;
        }
        let p = propose_split(dir.path(), "mixedvox", Modality::Voice).await.unwrap();
        assert_eq!(p.groups.len(), 2, "proposes two speakers");
        let sizes: Vec<usize> = p.groups.iter().map(|g| g.stems.len()).collect();
        assert!(sizes.contains(&3) && sizes.contains(&2), "3 + 2 split, got {sizes:?}");
        assert!(p.strays.is_empty());
    }

    #[tokio::test]
    async fn propose_split_returns_empty_for_one_person() {
        let dir = td();
        // All near one axis — one person, however many samples.
        for k in 0..4 {
            enroll_v(dir.path(), "solo", Modality::Voice, &near(0, k as f32)).await;
        }
        let p = propose_split(dir.path(), "solo", Modality::Voice).await.unwrap();
        assert!(p.groups.is_empty(), "one person does not split");
    }

    #[tokio::test]
    async fn propose_split_prefers_two_to_three_over_more() {
        let dir = td();
        // Three distinct axes → a clean 3-way split is available and preferred.
        for axis in [0, 1, 2] {
            for k in 0..2 {
                enroll_v(dir.path(), "trio", Modality::Voice, &near(axis, k as f32)).await;
            }
        }
        let p = propose_split(dir.path(), "trio", Modality::Voice).await.unwrap();
        assert_eq!(p.groups.len(), 3, "three speakers, three groups (within preferred range)");
    }

    #[tokio::test]
    async fn apply_split_moves_all_but_the_largest_group_into_new_clusters() {
        let dir = td();
        for v in [near(0, 0.0), near(0, 1.0), near(0, 2.0), near(1, 0.0), near(1, 1.0)] {
            enroll_v(dir.path(), "mixed2", Modality::Voice, &v).await;
        }
        let p = propose_split(dir.path(), "mixed2", Modality::Voice).await.unwrap();
        assert_eq!(p.groups.len(), 2);
        let new_ids = apply_split(dir.path(), "mixed2", Modality::Voice, &p.groups).await.unwrap();
        assert_eq!(new_ids.len(), 1, "largest group stays, one new cluster minted");

        // The retained cluster keeps the larger (3) group; the new one has the 2.
        let (kept, _) = sample_media_counts(dir.path(), "mixed2", Modality::Voice).await;
        let (moved, moved_media) = sample_media_counts(dir.path(), &new_ids[0], Modality::Voice).await;
        assert_eq!(kept, 3, "largest group retained under original subject");
        assert_eq!((moved, moved_media), (2, 2), "smaller group moved with its media pairs");
    }

    #[tokio::test]
    async fn apply_split_is_a_noop_below_two_groups() {
        let dir = td();
        enroll_v(dir.path(), "one", Modality::Voice, &near(0, 0.0)).await;
        let ids = apply_split(dir.path(), "one", Modality::Voice, &[]).await.unwrap();
        assert!(ids.is_empty());
        let (kept, _) = sample_media_counts(dir.path(), "one", Modality::Voice).await;
        assert_eq!(kept, 1, "nothing moved");
    }
}
