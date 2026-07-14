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

## Not in scope (deliberately)

- **No media-playback detection.** Tagging encounters as "from a screen" vs "in
  the room" would need a context flag threaded into `assign` (doesn't exist
  today). Not needed: the recurrence rule already catches video nights, because
  they are single-occasion bursts. A later refinement, not a prerequisite.
- **No `split_cluster`.** Un-merging the contaminated 赵力 cluster (601 misattributed
  samples on 7/10) is a separate capability, tracked with the calibration view.
- **No salience curve.** A binary grace-gated keep/forget is easier to audit than
  a half-life, and enough for the goal.
