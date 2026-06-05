# hi-agent — Memory

## Goal

Give the agent a continuous self that remembers across sessions. The design rests on one idea:

> **Everything is memory at a depth.** One gradient — deep/stable/always-loaded at one end, shallow/volatile/loaded-on-demand at the other — with a small working set that is always present and links out to cold detail pulled in by relevance.

Two consequences shape the whole subsystem:

- **One lossless source of truth, many cheap regenerable projections.** The raw signal stream is the only authority; episodes, facets, and the working set are *derivations* that can be thrown away and rebuilt. (Lambda architecture / hippocampus→neocortex.)
- **Capture is mechanical; meaning is the mind's job.** Recording a signal is a dumb, lossless write. *Segmenting* it into events, *summarizing* it into understanding — those are judgments, and per the project's standing value (human-interface fidelity over code heuristics) they belong to a cognition session at reflection time, never to a heuristic in Rust.

This document is the **durable design contract** for memory, in the spirit of `architecture.md`. It describes the target, not the path there; migration steps are disposable and live in `impl.md`. The raw foundation is now in place; the derived layers remain (see §9).

## Design decisions

| Decision | Reasoning |
|---|---|
| **Everything is memory at a depth** — one gradient, one rule (pull from the core outward by relevance) | A single generative model instead of a lookup table of special cases; scales past the handful of behaviors we can enumerate |
| **`raw/` is the only source of truth; everything else is a regenerable projection** | Lossless log + lossy views. A wrong summary is never load-bearing because the log can re-derive it |
| **Regenerate, don't patch; every derived claim cites its source signals** | Projections stay trustworthy and disposable; no drift between a summary and the facts under it |
| **Capture is mechanical & lossless; episodes/facets are reflection-time judgments** | A topic boundary or a "what I now believe" is a judgment; judgment lives in the mind, not in a Rust filter |
| **One `raw/` slice per scene** | Realizes the "journal slice per scene" the scene already implies; makes per-scene reads bounded instead of a full-file scan |
| **A signal = a text surface (always) + an optional media payload** | Text and multimodal are one record type, not two systems. Every modality has a text surface (words / transcript / caption); bytes are an attachment |
| **Workers are scenes too — a worker run is its own lossless `raw/` stream** | Uniform ("everything is a scene with a signal stream"), and `architecture.md` §7 already requires worker transcripts to be inspectable |
| **The always-loaded core = `self.md` (stable identity) + `hot.md` (volatile activation)** | The two heat sources — permanence and recent-significance — kept as two files of different volatility |
| **`self` is not a facet** | facets model *external* entities; `self` is the core modeling itself. It sits on the selfhood gradient `SOUL → self → hot`, not next to people/locations |
| **No privileged facet dimensions** | people/locations/projects/culture are seeds, not an enum; the subject space grows as structure emerges |

---

## 1. The gradient

Two things make a memory "hot" (in the always-loaded working set):

- **Permanence** — it is always relevant (who I am, my values, a standing commitment). Deep, slow-changing.
- **Activation** — it is recent or significant right now (today's thread, the active project). Shallow, fast-decaying.

Depth also sets **plasticity**: deep memory has high inertia (a bad week cannot rewrite the soul), shallow memory turns over freely. This is why the same content can live at different depths — a one-off remark is shallow; a correction the person insisted on is deep "scar tissue."

The on-disk layout is just this gradient made concrete: `raw/` (the unfiltered firehose) → `episodes/` (events) → `facets/` (durable understanding) → `self.md`/`hot.md` (the always-on core).

## 2. Layout

All paths are under `<data_dir>/memory/`. `SOUL.md` stays at `<data_dir>/SOUL.md` — it is the *birth seed* (authored, shipped in repo as the default), not accumulated memory.

```
memory/
├── self.md                          ← the agent's evolving model of itself (core, always loaded)
├── hot.md                           ← the working set (volatile activation, always loaded)
├── raw/                             ← LOSSLESS source of truth — JSONL + blobs, append-only
│   └── <scene>/
│       ├── scene.json               ← true scene id, display name, created, last_active
│       └── signals/
│           ├── 2026-06-04/
│           │   ├── log.jsonl        ← that day's signals (the text surface lives here)
│           │   ├── audio-<id>.mp3   ← a signal's media payload, co-located
│           │   └── vision-<id>.jpg
│           └── 2026-06-05/
│               └── log.jsonl
│       └── files/<name>             ← exchanged/produced artifacts (user-sent docs, worker outputs)
├── episodes/                        ← DERIVED event bundles (markdown + attachments)
│   └── 2026-06-04-kyoto-trip-7a3f/
│       ├── episode.md               ← gist + frontmatter (scene, signal-id range, citations, claims)
│       └── <attachments>            ← references into raw/ for the salient frames/files
└── facets/                          ← DERIVED current-understanding (markdown, every claim cites episodes)
    ├── people/<person>.md
    ├── locations/<place>.md
    ├── projects/<project>.md        ← the durable "task memory": goal, decisions, files, open threads
    └── culture/<topic>.md           ← the vividness-loop output: what it absorbed from the world
```

**Format split:** `raw/` is JSONL — structured, append-only, machine truth. Everything else is markdown — derived prose a mind reads directly. Lossless truth is structured; memory-as-read is prose.

## 3. `raw` — the lossless source of truth

`raw/` is organized **by scene**, because the scene is the isolation unit (`architecture.md` §6). A scene runs forever, so its signals are **sharded into day-folders**: a day's everything (log + that day's blobs) is one directory, trivial to archive when cold.

Because scene ids are arbitrary strings (`alice@phone`, possibly containing path-unsafe characters), the `<scene>` directory name is a **path-safe encoding** (percent-encode), and the true id lives in `scene.json`.

### The signal record

One JSON object per line in `signals/<date>/log.jsonl`:

| field | type | req | notes |
|---|---|---|---|
| `id` | uuidv7 | yes | unique + time-sortable. The cursor and the citation key; resolves ts ties |
| `kind` | `signal_in` \| `signal_out` | yes | direction |
| `ts` | RFC3339 | yes | when it happened |
| `channel` | text·vision·audio·touch·smell·taste | yes | the modality |
| `stream` | string | no | named stream within a channel (`webcam`, `headset`); absent = default |
| `scene` | string | yes | kept in-line too, so a record is self-describing and movable |
| `body` | string | yes | **the text surface of any modality** — words / STT transcript / caption. The unifier |
| `media` | object | no | `{ file, mime, duration_ms?, width?, height? }`; `file` is the co-located blob name. Absent for pure text |
| `origin` | `human`·`reactor`·`worker` | no | *which mind* produced it (mechanical). Not speaker identity — that stays soft/inferred |
| `turn` | int | no | the turn it was batched into; lets stimulus→response grouping be reconstructed without re-running settle |

`body` is always present → text and multimodal are one record type. `media.file` names the blob beside the log (`audio-<id>.mp3`); the metadata travels in the line so the log is self-describing without opening the blob.

### Three kinds of raw content

- **Signals** — channel I/O events. The perception/expression stream. (`signals/<date>/log.jsonl`)
- **Media** — the sensory blobs a signal references (audio bytes, camera frame). Co-located with the signal that produced them.
- **Files** — named, human-meaningful artifacts *exchanged or produced* (a user-sent PDF, a worker's deliverable). Not sensory, not events; they outlive any one day, so they sit in `files/`. Code being actively developed stays in its real workspace/repo and is referenced by path + commit — never copied into memory.

### Workers are scenes

A working session's run (its tool calls, intermediate output, deliverable) is recorded as its own `raw/<worker-scene>/` stream — same record type, same lossless treatment. Its report flows back to the parent scene as an ordinary signal. This keeps worker transcripts inspectable, which `architecture.md` §7 requires for progress-checking and seeding one worker with another's history.

## 4. `episodes` — derived event bundles

An **episode** is a coherent event within a scene ("the afternoon we planned the Kyoto trip") — the missing middle tier between a single turn and a forever-running scene:

```
Scene  ⊃  Episode  ⊃  Turn  ⊃  Signal
(where)   (an event)  (a beat)  (an utterance)
```

An episode is a **directory**, not a single file: a gist (`episode.md` with frontmatter — scene, the signal-id range it covers, citations, extracted claims) plus the attachments that make it vivid (a key vision frame, the deliverable). Attachments are **references into `raw/`**, not copies — single-source-of-truth holds; only genuinely derived artifacts (a thumbnail, the final deliverable) are materialized in the bundle. Scene lives in frontmatter, not as a directory level, so episodes browse chronologically across scenes; a short id suffix (`-7a3f`) keeps same-day same-slug names unique.

**Episodes are derived, not captured.** A boundary ("is this still the same event?") is a topic judgment, so it is made by a cognition session at reflection time — never by a time-gap heuristic in Rust. A time-gap is a legitimate *mechanical hint* (a long silence is a fact, not an opinion) that pre-segments candidate boundaries for the mind to accept or split.

**The cursor is the frontier of formed episodes.** Reflection consumes "signals in scene S after the last episode's end," then advances. The anchor is therefore not a separate cursor file to keep in sync — it is `max(episode end signal-id)` for the scene, which means deleting `episodes/` resets it to genesis and re-running rebuilds everything (regenerate-don't-patch). The heartbeat briefing (`reactor/heartbeat.rs`) is already a mind-authored, scene-scoped gist that today seeds a replacement session and is discarded; persisting it is the cheap first episode seed.

## 5. `facets` — derived current-understanding

A facet is the agent's best current understanding of one subject, **regenerated from episodes**, with every claim citing the source episodes/signals. `projects/<project>.md` is the durable task memory — the rolling state of a piece of work (goal, decisions, files touched, open threads) — distinct from the episodes that record the *sessions* of work and from the code that lives in the workspace.

Facet dimensions are **open-ended**. people/locations/projects/culture are seeds; new subject types are created as structure emerges, never baked into an enum.

## 6. `self` and `hot` — the always-loaded core

There is a **selfhood gradient by volatility**:

```
SOUL.md       ← birth seed. Authored, set once, ships in repo. Deepest, highest inertia.
memory/self.md ← the EVOLVING core: voice, learned manners, point of view. Slow-changing, sticky.
memory/hot.md  ← the working set: self + standing commitments + recent significant episode gists.
```

`self` is not a facet (facets model *external* entities; `self` is the subject modeling itself, and its plasticity — corrections as scar tissue — puts it next to SOUL). Both `self.md` and `hot.md` load into every session: `self.md` is the stable identity (permanence-hot), `hot.md` is the recent activation (activation-hot). The **per-scene** activation layer is already handled by the existing recency snapshot (`memory/snapshot.rs`, `build_for_scene`) — so `hot.md` is global and slow; the snapshot stays per-scene and transient. Three tiers, no duplication: `hot.md` (global core, always) → snapshot (per-scene recent, per turn) → episodes/facets (cold, on demand via links).

## 7. Reflection — the mind consolidating ("sleep")

Consolidation is a **working session**, not the reactor turn loop — so cost never blocks speech. It is seeded like the heartbeat (`unconsolidated signal tail + the gist of the last episode or two for continue-vs-new judgment`), reads a scene's signals after the cursor, and writes episodes and updated facets. It is triggered on **scene-idle** (a silence gap is the natural "the event ended, file it" moment), which also keeps the unconsolidated tail small. *When* it runs is the only knob, and it is a cost/cadence choice (every wake is a paid cognition turn) — not a judgment problem.

## 8. Invariants

- **`raw/` is the only source of truth.** Only ever *append* to it; never edit a past signal.
- **Regenerate, don't patch.** Episodes and facets are rebuilt from raw, never hand-edited in place.
- **Every derived claim cites source signal ids.** A facet line without a citation is a bug.
- **Lossy projections are fine** precisely because the log under them is lossless.
- **No privileged dimensions.** Materialize hot slices on demand; let facet types emerge.
- **The observatory is not memory.** `sessions.jsonl` (lifecycle/debug events) stays separate; `raw/` is signals only.

## 9. Status

**Implemented:**
- **Raw** (`src/memory/{layout,journal,media}.rs`, `src/types.rs`): per-scene day-folder slices, a uuidv7 `id` per signal, co-located media, `scene.json`. `origin` provenance (human/reactor/worker) is captured; `turn` is still deferred.
- **Core loading** (`src/memory/core.rs`): every reactor session loads `self.md` (sticky identity) + `hot.md` (working set) on top of the soul — at session open and at each heartbeat hot-swap.
- **Episodes** (`src/memory/episodes.rs`): the heartbeat persists its conversation briefing as an episode (`episodes/<date>-<short>/episode.md`) — the cheap seed. This is the only producer today.
- **hot.md** (`refresh_hot`): regenerated from recent episode gists on each heartbeat — a mechanical projection (regenerate, don't patch), not yet an agent-curated working set.

No migration was needed — there was no prior data.

**Still to build:**
- **`facets/`** — per-subject understanding; needs subject extraction (a judgment), hence a real reflection session.
- **Agent-judgment reflection** — today consolidation is mechanical (briefing→episode, episodes→hot.md). The design's "sleep" — a working session that segments episodes semantically, derives facets, and curates `self.md`/`hot.md` — is the remaining engine.
- **Idle trigger** — episodes form only at heartbeat (context-pressure) boundaries; a scene-idle trigger would consolidate sooner and on semantic, not size, boundaries.
- **Workers as raw streams**, **`files/`**, **content index** (§3, §8) — still open.

## References

- [Architecture](architecture.md) — §5 (journal as durable backstop), §6 (scene isolation), §7 (inspectable workers)
- [human-interface spec](../../human-interface/docs/human-interface.md)
