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
| **One `raw/` slice per scene, stored by channel** | The scene is the isolation unit; within it each modality is its own day-sharded folder, so a channel is a complete, bounded, separately-fadeable record |
| **A signal = a text surface (always) + an optional media payload** | Text and multimodal are one record type, not two systems. Every modality has a text surface (words / transcript / caption); bytes are an attachment |
| **Text is permanent; media fades to chosen keepsakes** | Three depths — text (forever) / full bytes (recent) / a keepsake frame-or-seconds — and no in-place transcoding (no low-res/low-bitrate rung). Bounds size without losing the memory |
| **Forgetting is delegated judgment, not a rule** | *When* and *what* to fade is a content call made by a forgetting sub-agent (informed by byte-pressure), not an age-sweep; the only hard rail is never fading bytes a reflection hasn't consolidated |
| **The interleaved timeline is derived, never stored** | A scene is one timeline but stored per-channel; the mind reads a merge built on read (ordered by uuidv7 `id`), so there is no second copy to drift |
| **`appearance` is retained state, not an utterance stream** | The screen persists until changed, so it is recorded as timestamped whole-state snapshots; the newest is the current screen (no separate current-state file). View lifetime is the reactor's decision — no server-side auto-expiry |
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

All paths are under `<data_dir>/memory/`. The soul is *not* here: it ships inside the binary and is materialized to `<data_dir>/prompts/core.md` (composed with an optional `core.local.md` operator override) — the *birth seed*, authored and shipped, not accumulated memory.

```
memory/
├── self.md                           ← evolving identity (core, always loaded)
├── hot.md                            ← working set (derived projection, always loaded)
│
├── raw/                              ← LOSSLESS TRUTH, per scene — append-only, never edited
│   └── <scene>/                      ← dir name = path-safe encoding of the scene id
│       ├── scene.json                ← the true (un-encoded) id + created_at
│       ├── text/
│       │   └── 2026-06-11/text.jsonl ← the day's messages, both directions
│       ├── audio/
│       │   └── 2026-06-11/
│       │       ├── audio.jsonl       ← surface log (transcripts), both directions
│       │       ├── 09/16.mp3         ← input stream (mic) — default, bare, minute grid
│       │       └── output/09/11.mp3  ← output stream (TTS)
│       ├── vision/
│       │   └── 2026-06-11/
│       │       ├── vision.jsonl      ← surface log (captions)
│       │       └── 10/15.mp4         ← camera; output/ holds generated frames
│       ├── appearance/               ← the one STATE channel: screen-state history
│       │   └── 2026-06-11/           ← whole-state snapshots; newest = current screen
│       │       └── appearance-101502Z.json
│       └── files/                    ← exchanged/produced artifacts (kept verbatim)
│           └── 2026-06-11-trip-plan.pdf
│
├── episodes/                         ← DERIVED event bundles (markdown + attachments)
│   └── 2026-06-11-kyoto-trip-7a3f/
│       ├── episode.md                ← gist + frontmatter (scene, signal-id range, citations)
│       └── <attachments>             ← refs into raw/ + genuinely-derived artifacts
└── facets/                           ← DERIVED current-understanding (every claim cites)
    ├── people/<person>.md
    ├── locations/<place>.md
    ├── projects/<project>.md         ← durable task memory: goal, decisions, open threads
    └── culture/<topic>.md            ← what it absorbed from the world
```

**Truth vs. projection.** Everything under `raw/` is append-only lossless truth — identity, the channel streams (including the `appearance/` state-snapshot history), imported artifacts. Everything else (`episodes/`, `facets/`, `hot.md`, the current screen, and the interleaved per-scene timeline the mind reads) is a **projection**: regenerable from `raw/`, never a second source of truth, safe to delete and rebuild.

**Format split:** the channel surface logs are JSONL — structured, append-only, machine truth. Everything derived is markdown — prose a mind reads directly.

## 3. `raw` — the lossless source of truth

### Organized by scene, then by channel

`raw/` is sliced **by scene** (the isolation unit, `architecture.md` §6). Scene ids are arbitrary strings (`alice@phone`), so the `<scene>` directory is a **path-safe percent-encoding** and the true id lives in `scene.json`.

Within a scene, each **channel is its own folder** (`text/`, `audio/`, `vision/`, `appearance/`), sharded by UTC day. A channel is that sense's complete record; the day-folder keeps reads bounded and makes per-channel fading/archival a single subtree. Each channel-day carries a **surface log named for the channel** — `text.jsonl`, `audio.jsonl`, `vision.jsonl` — one JSON object per line, both directions interleaved. (The filename is self-describing even detached from its folder — the old generic `log.jsonl` was not.)

A scene is **one timeline**, but it is *stored* per channel. The interleaved timeline the mind reads (and the recent-window snapshot) is a **derived merge** over the channel logs, ordered by the uuidv7 `id` — built on read, never persisted. Splitting storage by channel costs only a cheap merge; persisting the merge would create a second, driftable copy.

### The signal record

One JSON object per line in `<channel>/<date>/<channel>.jsonl`:

| field | type | req | notes |
|---|---|---|---|
| `id` | uuidv7 | yes | unique + time-sortable. The cursor and the citation key; orders the cross-channel merge |
| `kind` | `signal_in` \| `signal_out` | yes | direction. Mirrored in the byte path (`output/`) |
| `ts` | RFC3339 | yes | when it happened |
| `channel` | text·audio·vision·appearance·… | yes | the modality. Redundant with the path, kept so a line is self-describing and movable |
| `stream` | string | no | named stream within a channel (`mic`, `voice`, `webcam`); absent = the default stream |
| `scene` | string | yes | kept in-line too, so a record is self-describing and movable |
| `body` | string | yes | **the text surface of any modality** — words / transcript / caption. The unifier. May be `""` (an un-captioned frame) |
| `media` | object | no | `{ file, mime, duration_ms?, width?, height? }`; `file` is a path **relative to the channel-date folder** (`09/16.mp3`, `output/09/11.mp3`). Absent for pure text |
| `origin` | `human`·`reactor`·`worker` | no | *which mind* produced it (mechanical). Not speaker identity — that stays soft/inferred |
| `turn` | int | no | the turn it was batched into; lets stimulus→response grouping be reconstructed without re-running settle |

`body` is always present → text and multimodal are one record type. The bytes never enter the log — only `media.file` + metadata — so the log stays small and self-describing without opening a blob.

### Bytes: capture on the minute grid

Continuous channels (mic, camera) are **segmented at capture on the wall-clock minute**: while a stream is open it writes one file per minute, `<hh>/<mm>.<ext>`; a closed stream or a silent minute writes nothing (silence costs zero bytes — there is no day-long tape). A one-off capture (a posted clip, a still) is named by second to share the grid without colliding. **Every captured chunk keeps its bytes — the live mic included** (the audio *is* the raw signal; the transcript is a derivation of it).

Direction and streams: **input is the default** and writes bare under `<channel>/<date>/`; **output writes under `output/`**; when a channel carries more than one of either, the extras get an id-suffixed folder (`input-<id>/`, `output-<id>/`). Direction is also the `kind` field on the line.

### Forgetting: full → keepsakes → text

Media is not kept forever, and this is what bounds size. A signal has three depths of vividness, and it sheds them with age — but it **never degrades in place**: a blob is either its full self, a chosen keepsake, or gone. There is no low-res or low-bitrate middle rung.

- **Text — permanent.** The `.jsonl` surface (words / transcript / caption) is never edited or deleted. KBs/day, nearly free; *the log is the memory.* Nothing ever fades below it.
- **Full bytes — the recent vivid window.** The originally-captured audio/video, kept verbatim while a memory is fresh enough to replay in detail.
- **Keepsakes — content-chosen survivors.** Between the two: a frame or a few seconds judged worth keeping vivid. Sparse, and often there are none — most moments rightly survive as words alone. *What* to keep is never a fixed rule (no "first N seconds", no per-minute thumbnail); it is a content judgment.

**Forgetting is delegated judgment, not a timer.** "When is a memory ripe to fade, and which moments survive it" is a judgment, so it lives in a cognition session — an age-sweep in Rust would be the one hardcoded heuristic the rest of the subsystem refuses. So reflection grows a second, backward-looking faculty beyond consolidating the frontier — **tending the old store** — and does it by **delegating to a forgetting sub-agent**. Being a worker, that sub-agent is itself a scene, so the run is inspectable like any other (§3 *Files and workers*). The split mirrors `record_episode` — soft judgment, exact hands:

- **The sub-agent makes the soft calls.** Shown the *pressure* on a scene — per-channel byte weight, age, consolidation status — it judges which cold windows are ripe to fade and, content-aware, which frame or seconds of each to keep. Informed by real pressure, never reduced to a rule.
- **A deterministic tool does the cut.** `keep_and_fade(scene, channel, window, spans_to_keep[])` slices the kept spans into clip files and unlinks the full bytes. The mind never reasons about byte offsets.

Slowness comes from the judgment, not a clock: reflection can dispatch the pass on its own cadence, and most runs it finds nothing ripe — a cheap near-no-op — so a given memory sheds its bytes **once**, when it has genuinely gone cold, not on every consolidation.

**One hard rail, the rest soft.** Exactly one rule is not the sub-agent's to bend: **never fade bytes a reflection has not yet consolidated** — the window must lie entirely behind the scene cursor (`max(episode.to_id)`), so un-summarized detail is never lost. That is a safety invariant, not a forgetting policy; *when*, *what*, and *how much* are all the sub-agent's call.

Exemptions and mechanics:
- **`files/` never fade** — exchanged/produced artifacts are kept verbatim forever (the passport scan stays whole).
- **Output bytes** (TTS, generated frames) are the most disposable — regenerable from the text/prompt that made them — so they fade to text early unless explicitly kept.
- **`appearance`** is ~text JSON; old days thin to the newest snapshot.
- Forgetting only ever rewrites or removes *blobs* — never a `.jsonl` line (§8 holds). The line keeps naming its original byte path; a reader resolves **best-available** — original, else the kept keepsake, else caption-only.
- One consequence is accepted: once full bytes are gone, a kept keepsake is itself small permanent evidence, no longer regenerable — while the episode *gist* stays regenerable from the permanent text beneath it.

### `appearance` — the one state channel

Every other channel is an **event stream** (utterances). `appearance` is **retained state**: the screen persists until changed, so it is recorded not as deltas but as **timestamped whole-state snapshots** — `appearance/<date>/appearance-<hhmmssZ>.json`, each the full screen as of that moment, valid until the next (a same-second collision bumps to the next free second). The **current** screen is simply the newest snapshot — there is no separate current-state file; the live bus holds it in memory and restores from the newest snapshot on boot. A view persists until the agent dismisses or replaces it: **there is no auto-expiry — view lifetime is the reactor's decision**, not a server-side timer. Showing a view is expression the agent can later cite ("I showed them the itinerary"), so the history feeds reflection like any other channel.

### Files and workers

- **Files** — named artifacts *exchanged or produced* (a user-sent PDF, a worker's deliverable): flat under `files/`, not date-sharded (they outlive any day), kept in their original format. Code under active development stays in its real workspace/repo and is referenced by path + commit — never copied in.
- **Workers are scenes** — a worker run is its own `raw/<worker-scene>/` of the same shape; its report flows back to the parent scene as an ordinary signal. This keeps worker transcripts inspectable, which `architecture.md` §7 requires.

## 4. `episodes` — derived event bundles

An **episode** is a coherent event within a scene ("the afternoon we planned the Kyoto trip") — the missing middle tier between a single turn and a forever-running scene:

```
Scene  ⊃  Episode  ⊃  Turn  ⊃  Signal
(where)   (an event)  (a beat)  (an utterance)
```

An episode is a **directory**, not a single file: a gist (`episode.md` with frontmatter — scene, the `from_id`/`to_id` signal range it covers, the subjects it touched) plus, eventually, the attachments that make it vivid (a key vision frame, the deliverable). Attachments are **references into `raw/`**, not copies — single-source-of-truth holds; only genuinely derived artifacts (a thumbnail, the final deliverable) are materialized in the bundle. *(Attachments are not yet produced — today an episode is just its `episode.md`.)* Scene lives in frontmatter, not as a directory level, so episodes browse chronologically across scenes; a short id suffix (`-7a3f`) keeps same-day same-slug names unique.

**Episodes are derived, not captured.** A boundary ("is this still the same event?") is a topic judgment, so it is made by a cognition session at reflection time — never by a time-gap heuristic in Rust.

**Sequential cuts, by count.** Reflection sees the scene's unconsolidated signals as a numbered, oldest-first list and cuts them front to back: each `record_episode(count, …)` files the next `count` signals as one episode, so the mind chooses *boundaries* and never handles a raw signal id. The range (`from_id`/`to_id`) is filled from the covered signals — that range **is** the episode's citation back into `raw/`. The mind stops early to leave an event still in progress unconsolidated; it returns next round.

**The cursor is the frontier of formed episodes.** Reflection consumes "signals in scene S after the last episode's end," then advances. The anchor is therefore not a separate cursor file to keep in sync — it is `max(episode to_id)` for the scene, which means deleting `episodes/` resets it to genesis and re-running rebuilds everything (regenerate-don't-patch). Each `record_episode` advances the cursor by exactly its `count`, so within one round consecutive calls cut a clean, gapless sequence.

## 5. `facets` — derived current-understanding

A facet is the agent's best current understanding of one subject, **regenerated from episodes**, with every claim citing the source **episodes** (by their refs — episodes in turn cite the raw signal range, so the chain to ground truth holds while facet prose stays readable). `projects/<project>.md` is the durable task memory — the rolling state of a piece of work (goal, decisions, files touched, open threads) — distinct from the episodes that record the *sessions* of work and from the code that lives in the workspace.

Facet dimensions are **open-ended**. people/locations/projects/culture are seeds; new subject types are created as structure emerges, never baked into an enum.

A facet is regenerated whole, never patched: reflection reads the current file, folds in the new episodes, and writes the entire understanding back. Facets are **global** (one `people/alice.md`, not one per scene), so two scenes can touch the same file; the write is atomic (temp + rename) so a reader never sees a torn file, but a cross-scene read-modify-write is deliberately **last-writer-wins** — a facet is a regenerable cache whose truth lives in the episodes, so the next reflection re-derives anything a racing write dropped.

## 6. `self` and `hot` — the always-loaded core

There is a **selfhood gradient by volatility**:

```
prompts/core.md ← birth seed. Authored, ships in the binary, materialized on boot. Deepest, highest inertia.
memory/self.md  ← the EVOLVING core: voice, learned manners, point of view. Slow-changing, sticky.
memory/hot.md   ← the working set: self + standing commitments + recent significant episode gists.
```

`self` is not a facet (facets model *external* entities; `self` is the subject modeling itself, and its plasticity — corrections as scar tissue — puts it next to the soul). Both `self.md` and `hot.md` load into every session: `self.md` is the stable identity (permanence-hot), `hot.md` is the recent activation (activation-hot). The **per-scene** activation layer is already handled by the existing recency snapshot (`memory/snapshot.rs`, `build_for_scene`) — so `hot.md` is global and slow; the snapshot stays per-scene and transient. Three tiers, no duplication: `hot.md` (global core, always) → snapshot (per-scene recent, per turn) → episodes/facets (cold, on demand via links).

## 7. Reflection — the mind consolidating ("sleep")

Consolidation is a **dedicated session of its own**, not the reactor turn loop and not the live mind — so cost never blocks speech and the reactor's context is never polluted. It **reads `raw/` directly** (not the conversation, not the swap briefing — those are lossy self-summaries; truth is the log): the scene's signals after the cursor, seeded into the session alongside the gist of the last episode or two (for continue-vs-new judgment) and the index of subjects already modeled (so it reuses a subject rather than coining a near-duplicate). It then segments by `record_episode` and regenerates facets by `read_facet`/`update_facet`.

It is **triggered on its own clock**, decoupled from the compact hot-swap, so consolidation never waits on the reactor session crossing its context-pressure ceiling (the old coupling meant no compact → no reflection, so a quiet scene never consolidated). **One adaptive clock** feeds it, living as a deadline in the per-scene loop's `select!` next to pulse/alarm (`reactor::next_reflection_at`), anchored on the **last reflection** (or loop start, before the first):
- **Fresh input** since that anchor (`last_activity > anchor`) → the next pass is due `HI_AGENT_REFLECT_EVERY` (the **base** cadence, default 1m) after it. Anchoring on the reflection rather than the last turn is what lets a continuously-busy scene still consolidate ~once per base — the old separate "periodic backstop" falls out of this for free.
- **Caught up and quiet** (`last_activity <= anchor`) → the gap **backs off**, doubling from the base each pass (1m → 2m → 4m → …) up to `HI_AGENT_REFLECT_MAX` (default 8h), so a long-idle scene stops re-checking in vain. Any new turn snaps the gap back to the base, so the next quiet spell consolidates promptly again — the human-like "file it once the event ends, then rest deeper the longer nothing happens."

The backoff also subsumes the old fixed cooldown (the minimum spacing between passes *is* the base) and the old fixed idle trigger (a quiet scene's first backed-off gap is the base). It runs **detached**: nothing is held on it, so a slow consolidation never stalls the floor; the cursor makes it idempotent, and a tick that fires while a prior round is still running simply skips (the anchor advances on every fired tick, spawn or not, so a busy guard never hot-spins the wait loop). New input after a long idle re-anchors on the hours-old reflection, so the catch-up pass is due immediately — fine, it's detached and a sub-`MIN_REFLECT_SIGNALS` frontier no-ops cheaply. One consequence is accepted: a hot-swap firing between reflections may seed its replacement session from a `hot.md`/facets one cycle behind — fine, since those are projections. *Cadence* is the only knob, and it is a cost choice (every round is a paid cognition turn, plus a subprocess spawn) — not a judgment problem. A tiny frontier (< `MIN_REFLECT_SIGNALS`) is skipped rather than spending a session to file a handful of lines; `HI_AGENT_REFLECT=off` disables it entirely.

Reflection also **tends the old store**, not just the frontier: alongside consolidating new signals it can fade cold media down to its keepsakes, delegating that to a forgetting sub-agent. This is the same session's backward-looking half; the mechanism — three-layer fade, the `keep_and_fade` cut tool, the byte-pressure it judges on, and the single safety rail (never fade un-consolidated bytes) — lives in §3 *Forgetting*.

*(The clock fires on time/activity, not yet on a true semantic boundary — detecting that the topic/event changed, rather than just that the base gap of silence passed, remains a future refinement. It would live in the same per-scene loop, so the reflection guard stays a loop-local handle.)*

## 8. Invariants

- **`raw/` is the only source of truth.** Only ever *append* to it; never edit a past signal.
- **Regenerate, don't patch.** Episodes and facets are rebuilt from raw, never hand-edited in place.
- **Every derived claim cites source signal ids.** A facet line without a citation is a bug.
- **Lossy projections are fine** precisely because the log under them is lossless.
- **Forgetting fades blobs, never signals.** Media may shed to a chosen keepsake or drop entirely, but a `.jsonl` line is never edited and never falls below its text surface.
- **Never fade un-consolidated bytes.** A window may only fade once it lies entirely behind the scene cursor (`max(episode.to_id)`), so reflection has always seen the detail before it can be lost.
- **No privileged dimensions.** Materialize hot slices on demand; let facet types emerge.
- **The observatory is not memory.** `sessions.jsonl` (lifecycle/debug events) stays separate; `raw/` holds only signals and exchanged artifacts.

## 9. Status

**Implemented:**
- **Raw — channel-first layout** (`src/memory/{layout,journal,media}.rs`, `src/types.rs`): per-scene, per-channel, per-day folders with a `<channel>.jsonl` surface log; a uuidv7 `id` per signal; media bytes on the wall-clock grid with `media.file` relative to the channel-day folder. `append` routes by channel; `recent` merges channels by `(ts, id)`. Posted audio clips journal as `channel: Audio`; vision stills journal as `channel: Vision`. `origin` is captured; `turn` is still deferred.
- **Appearance state channel** (`src/server/view_bus.rs`): each screen mutation appends a whole-state snapshot to `raw/<scene>/appearance/<date>/appearance-<HHMMSSZ>.json`; the newest restores the live screen on boot. No server-side TTL — view lifetime is the reactor's call (the `ttl_ms` envelope field and client/server expiry were removed).
- **Live mic capture** (`src/server/audio.rs`): the streaming mic's PCM is persisted on the wall-clock-minute grid as `audio/<date>/<HH>/<MM>.wav` (raw 16 kHz mono + a WAV header), flushed at each minute rollover and at close. The bytes are an un-journaled tape; utterance lines correlate to a minute by ts.
- **Vision capture + placeholder perception** (`src/server/vision.rs`): camera WebM is persisted per minute (`vision/<date>/<HH>/<MM>.webm`, init segment prefixed so each file decodes standalone); stills persist as one-offs. Each is **perceived** — `capabilities::vision::understand` captions it (Image for a still, Video for a camera minute), or a placeholder caption when no `VISION_PROVIDER` is set — and the caption is journaled as the vision signal's `body`. Perception runs detached so capture never blocks.
- **Core loading** (`src/memory/core.rs`): every reactor session loads `self.md` + `hot.md` on top of the soul — at session open and at each heartbeat hot-swap.
- **Reflection — episodes + facets** (`src/reactor/heartbeat.rs::reflect`, `src/memory/{episodes,facets,journal}.rs`, `src/mcp/mod.rs`): on **one adaptive clock** (`reactor::next_reflection_at`) anchored on the last reflection — fresh input → fire the base cadence (`HI_AGENT_REFLECT_EVERY`, default 1m); caught up and quiet → the gap backs off (doubling) up to `HI_AGENT_REFLECT_MAX` (default 8h); any turn snaps it back to the base. `HI_AGENT_REFLECT=off` disables it — a **detached reflection session** (`SessionRole::Reflection`, its own subprocess, never the live mind) reads `raw/` after the per-scene cursor (`journal::after_cursor` + `episodes::scene_cursor` = `max(to_id)`), and through reflection-only tools segments the frontier into episodes (`record_episode(count, …)` — sequential count-cuts, range auto-filled) and regenerates the facets they touch (`read_facet`/`update_facet`, atomic, last-writer-wins). Facets cite episode refs; episodes cite the raw range. The reflection session's own instructions are the operator-overridable `prompts/reflection.md` (embedded base materialised at boot, like `core.md`/`speaking.md`, but **inlined** as the session's system prompt rather than Read by the agent — see `reactor::reflection_prompt`). The compact hot-swap no longer writes episodes (the briefing is now only the replacement seed); `hot.md` refreshes at the end of each reflection.
- **hot.md** (`refresh_hot`): regenerated from recent episode gists after each reflection — a mechanical projection, not yet an agent-curated working set.

**Still to build:**
- **`self.md` curation** — reflection writes episodes and facets but does not yet evolve the identity core; that (corrections as "scar tissue") is the deepest, most plasticity-resistant layer, deferred.
- **Agent-curated `hot.md`** — still a mechanical projection of recent gists, not a working set the mind shapes.
- **Semantic trigger** — reflection now fires on one adaptive time/activity clock (base cadence when there's fresh input, exponential backoff to a cap while quiet); a true *semantic* trigger (detecting the topic/event actually changed, not just that the base gap of silence passed) remains future. Same per-scene loop, so the guard stays a loop-local handle.
- **Episode attachments + per-claim citations** — an episode is just its `episode.md` today (no materialized thumbnails/deliverables); claims cite at episode granularity, not per-signal.
- **Vision attention policy** — perception currently fires on every still and every camera minute; a real cadence/salience policy (when to actually look) is the deliberate placeholder left open.
- **Forgetting (media fading)** — the three-layer fade (full → keepsakes → text) is not built; raw bytes are kept indefinitely (`src/memory/media.rs` flags the absent GC). Needs the reflection-delegated forgetting sub-agent, the `keep_and_fade` cut tool (judge spans → slice + unlink), and the cursor safety gate. §3 *Forgetting* is the spec.
- **Workers as raw streams**, **`files/`**, **content index** (§3, §8) — still open.

## References

- [Architecture](architecture.md) — §5 (journal as durable backstop), §6 (scene isolation), §7 (inspectable workers)
- [human-interface spec](../../human-interface/docs/human-interface.md)
