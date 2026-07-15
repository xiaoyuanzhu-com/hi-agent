# Forgetting ambient identity clusters

## Why

Most voices and faces the agent meets are **not people it needs to know** — a
stranger in a café, a character in a video the kid played, a passer-by on the
street. Left alone they accumulate: the people store fills with single-shot
noise, the calibration/claim view drowns in strangers, and every video night
dumps more. So unnamed clusters must be **biased to forget**; only the ones that
**recur across time** earn the right to persist and eventually be named.

This is the cleanup half of people-recognition. It is deliberately gentle
(keep-biased): it removes only the obviously-ambient, and never touches anyone
who has a name or who has been seen more than once.

## Decisions

- **Recurrence, not sample count, earns keeping.** 601 voice samples from one
  bedtime-story video night are **one occasion**, not 601. A voice heard on three
  separate days is genuinely someone. Sample volume is meaningless; temporal
  spread is everything.
- **The timeline is already on disk — no new schema.** Every sample's filename is
  a uuid-v7 whose timestamp says *when* it was seen. A cluster's whole history is
  reconstructed by reading stems. No salience field, no last-seen column, no
  database. The criteria are pure functions over the existing directory tree.
- **Forget = plain delete.** No archive, no revival path. If a forgotten person
  ever matters, they show up again and re-cluster from scratch, earning their keep
  the normal way. (Considered a soft-archive/`.forgotten/` tier; rejected as
  hoarding — the store never consults it, so it would only cost disk.)
- **Runs inside reflection.** Folded into `heartbeat::run_consolidation`, once per
  consolidation (the people store is global, not per-scene), on the same
  adaptive-backoff reflection clock as the media `decay`.
- **Ships as a dry run first.** `SWEEP_DRY_RUN = true` — it logs what it *would*
  forget so the criteria can be watched on real data before it is trusted to
  delete. Flip the constant to arm it.

## Criteria

A cluster is examined only if it is a subject directory under
`memory/facets/people/`. It is **forgotten** iff all hold:

1. **Unnamed** — no `facet.md` exists. A `facet.md` means the mind has modeled
   this subject (or a human named it), so it is kept forever, even at zero
   samples (e.g. a named daughter whose voiceprints haven't landed yet).
2. **Single-occasion** — seen on fewer than `KEEP_OCCASIONS` (= 2) occasions.
   Occasions are sightings across both modalities, split whenever the gap between
   consecutive sightings exceeds `OCCASION_GAP` (= 30 min). One night, one call,
   one burst = one occasion however many samples it left.
3. **Gone cold** — the most recent sighting is at least `FORGET_AFTER` (= 30 days)
   before now. A one-off gets a month to recur before it ages out.

A cluster with only a legacy packed blob (no per-sample uuid-v7 stems) has no
datable timeline, so it reports zero occasions and is **never** forgotten — we
don't age what we can't date.

### Dials

| Constant | Default | Meaning |
|---|---|---|
| `KEEP_OCCASIONS` | 2 | occasions at/above which a cluster is kept forever |
| `OCCASION_GAP` | 30 min | gap that separates one occasion from the next |
| `FORGET_AFTER` | 30 days | grace since last sighting before a one-off ages out |
| `SWEEP_DRY_RUN` | true | report-only vs actually delete |

All live in `people_vectors.rs` (criteria) and `heartbeat.rs` (`SWEEP_DRY_RUN`).

## Implementation

- `people_vectors::cluster_vitals(dir, subject) -> ClusterVitals` — reads the
  stems across `face/` + `voice/`, counts occasions, finds last-seen, checks for
  `facet.md`. Pure over disk state.
- `ClusterVitals::forgettable(now)` — the rule above.
- `people_vectors::sweep_forgettable(data_dir, now, dry_run) -> ForgetReport` —
  walks the store, applies the rule, deletes (unless `dry_run`), returns what was
  forgotten for logging.
- Hooked in `heartbeat::run_consolidation` right after the "reflection fired"
  log, gated by `SWEEP_DRY_RUN`.

## De-mixing a cluster that is actually several people

A cluster can hold **more than one person** — mostly voice (overlapping speech,
similar timbre, imperfect diarization), sometimes faces. Same shape as the
contamination in the 复盘 view, but from the source rather than a bad merge. Since
the append threshold is loose, an over-broad cluster still contains tighter knots
of embeddings, so re-clustering its own samples at a stricter threshold separates
the people.

There is no single right threshold, so the machine doesn't ask for one — it
**sweeps** and proposes:

- `propose_split(subject, modality)` — sweeps cosine thresholds loose→tight
  (`SPLIT_SWEEP`), returns the **loosest** split into **≥ 2** groups (group count
  only grows as the threshold tightens, so the loosest split lands in the preferred
  **2–3** range almost always), hard-capped at **10** (`MAX_SPLIT_GROUPS`, a
  backstop for a party/crowd, not a target). Singleton samples become `strays`
  (probable outlier frames), kept out of the group count. Empty `groups` = "one
  person, didn't separate". **Moves nothing** — it is a proposal to preview.
- `apply_split(subject, modality, groups)` — commits the human's accepted grouping:
  the **largest group stays** under the original subject (a named cluster keeps its
  name for its main occupant); every other group's sample pairs move into a fresh
  `mint_id`'d cluster. Per-modality (voice and face spaces don't compare; a
  mixed-modality cluster is de-mixed one modality at a time — cross-modal rebinding
  is out of scope).

In the view this is a **⟳ re-cluster button** on a cluster: tap → the proposal is
previewed (each group auditionable) → **Apply**. Preview-before-apply is the whole
safety story: a bad threshold costs nothing because nothing moves until accepted.

**This is the shared un-merge primitive.** Pointed at the *named*, contaminated 赵力
cluster, the same `propose_split`/`apply_split` breaks the 601 mis-merged 7/10
samples off into their own cluster to be renamed — so the calibration claim view
and the 复盘 contamination-repair view use one capability.

## The review surface — 认识的人

A single web view (`_builtin/people-review`) is where a human sets identity
straight — the human-in-the-loop moment for a system that otherwise clusters
silently. It's a Contacts-style grid: each person a poster card (a face crop or a
voice glyph) with just a name. Clicking a card expands it **in place** (the row
grows, poster slides left, the review opens beside/below) into a review pane with
the clips split into a **人脸** section and a **声音** section — the two modalities
never mix, matching the store. There is no separate "contamination" view: a
polluted known cluster is just a card whose 声音 section you auto-regroup.

Every correction maps to a `people_vectors` primitive:

- **Name / rename** — inline-editable name. Renaming onto an existing name **is the
  merge** (`rename`); it's the only merge path and how a mistaken split heals.
- **Eject a clip** — per-clip "不是这个人" pulls one clip out into its own fresh
  cluster (`eject_clip`). The finest correction; the everyday tool.
- **Auto-regroup** — "自动重新分组 ⟳" runs `propose_split` (preview, moves nothing)
  → human confirms → `apply_split` (largest group keeps the name). The shortcut for
  "too mixed to fix clip-by-clip."

Backend (`src/foundation/server/people.rs`, global — no scene header):
`GET /api/people`, `GET /api/people/{subject}/{modality}/{stem}` (serve one
crop/clip), `POST /api/people/name`, `/eject`, `/split/preview`, `/split/apply`.
Data layer: `people_vectors::{list_clusters, clip_media_path, eject_clip}`. The
agent offers the view via `show_view _builtin/people-review`, taught in
`identity/core.md` as a light, presence-appropriate exercise (offer, don't insist).

## Not in scope (deliberately)

- **No media-playback detection.** Tagging encounters as "from a screen" vs "in
  the room" would need a context flag threaded into `assign` (doesn't exist
  today). Not needed: the recurrence rule already catches video nights, because
  they are single-occasion bursts. A later refinement, not a prerequisite.
- **No cross-modal binding in split.** `apply_split` moves one modality; it does
  not try to keep a person's face and voice together across the split (that
  binding is unsolved and designed elsewhere).
- **No salience curve.** A binary grace-gated keep/forget is easier to audit than
  a half-life, and enough for the goal.
