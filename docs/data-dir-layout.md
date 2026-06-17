# hi-agent — Data Directory Layout

## Goal

The data dir is **the agent's computer** — the one place every durable thing about the
agent lives. This document is the durable contract for *what each place is for and why*,
organized so the whole tree reads like a person's machine: a mind, a Documents folder, a
view workshop, a manual it was handed at the factory, and the runtime it thinks in.

The guiding test, as everywhere in hi-agent, is **fidelity to the human metaphor** — where
the layout diverges from how a person organizes their own computer, the divergence is named
and justified. Memory internals are owned by [`memory.md`](memory.md); this doc owns the
*whole* data dir and especially the **drive / views** split.

## Design decisions

| Decision | Reasoning |
|---|---|
| **Durability is the only physical boundary** — precious (synced, backed up) vs. disposable (regenerable, gitignored) | It's the one distinction the system *must* act on; everything else is soft convention |
| **Everything is memory; the drive is memory's verbatim annex, not a rival store** | A person has one mind that *reaches for* a notebook when exact bytes matter — the notebook isn't a second memory |
| **Meaning-valued → digested into memory (fuzzy); bytes-valued → kept verbatim in the drive** | Reconstruction is right for understanding, catastrophic for an API key |
| **`drive/` is verbatim and reflection-read-only; `views/` is fully disposable** | Once precious and disposable live in separate trees, the old `.cache` dotdir marker is unneeded — a whole tree is disposable, nothing to mark |
| **Ad-hoc views start in `views/`; their source *graduates* into `drive/` when worth keeping** | Filing is a deliberate act, the same fluid→solid move as `raw → facet`; most views die in `views/`, unmissed |
| **Capabilities are reached as on-demand skills, not always-loaded tools** | MCP tools cost context every turn; a long tail of capabilities belongs in the loaded-on-demand tier |
| **Secrets are resolved at call-time by the effector, never held in the mind's context** | You don't recite your password to use it; the value sits in the drive/env, the mind holds only a pointer |

---

## The map

```
data/
  memory/            # the mind — what the agent experiences & understands   (precious; see memory.md)
    raw/             #   lived signals, lossless, per-scene (verbatim, auto-captured)
    episodes/        #   consolidated moments (reconstructive)
    facets/          #   subject-indexed understanding (reconstructive, regenerated whole)
    hot.md           #   recency digest (default-loaded)

  drive/             # what the agent KEEPS — verbatim, precious, reflection-read-only   (proposed)
    projects/<p>/    #   sedimented work: kept view source + assets (the source of record)
    notes/  papers/  #   agent-curated keeps: the notebook, references, the digested world-doc — open shape
    …                #   the agent makes folders like a person organizes Documents

  views/             # the view workshop — disposable, gitignored, regenerable   (proposed; replaces workspace/)
    <project>/       #   ad-hoc views: source + build, until the source graduates to drive/projects/
    <toolchain>      #   esbuild + the headless-preview harness + node_modules — once, shared (NOT per-project)

  prompts/           # what the FACTORY gives — read-only to the agent, operator-overridable   (seed)
    core.md speaking.md aesthetic.md appearance.md meaning.md reflection.md
    world.md         #   (proposed) seeded world-priors: an "article from a trusted source" the agent digests into memory

  claude-config/     # the cognition RUNTIME — the ACP/claude subprocess's home (managed); transcripts are durable records
  sessions.jsonl     # the session ledger/index (a durable record)
```

Five **kinds**, each a place on a person's computer:

1. **memory/** — the mind. What the agent experienced (`raw/`) and what it understands (`episodes`, `facets`, `hot.md`). Reconstructive: reflection summarizes and regenerates it.
2. **drive/** — Documents + the notebook. What the agent deliberately keeps, **verbatim**.
3. **views/** — the view workshop. Where views are built; safe to wipe.
4. **prompts/** (the seed) — the manual handed over at the factory: how to be, plus priors about the world. Read-only to the agent, updatable by us.
5. **claude-config/ + sessions.jsonl** — the OS/process the mind runs in, and the logbook.

## The two axes that place everything

Every directory sits where it does because of two questions:

- **Precious or disposable?** — must this survive a `git reset --hard` on the disposable
  Mac mini, or can it be rebuilt? `memory/`, `drive/`, records, and the seed are precious
  and sync; `views/` is not. This is the only boundary the system is *required* to honor
  (backup/GC).
- **Reconstructive or verbatim?** — is the value in the *meaning* (digest it, let it blur
  and grow) or the *exact bytes* (keep it, look it up)? `memory/`'s `episodes`/`facets` are
  reconstructive; `raw/` and all of `drive/` are verbatim.

---

## Per-place contracts

### memory/ — the mind (reconstructive)

Owned by [`memory.md`](memory.md). The reconstructive store: `raw/` is the lossless,
auto-captured tape (verbatim but not *kept by choice* — captured by the system); `episodes/`
and `facets/` are the regenerable understanding reflection distills from it; `hot.md` is the
recency digest. Precious and synced. Reflection **owns** this tree — it rewrites facets whole.

### drive/ — what the agent keeps (verbatim, precious)

The agent's Documents and notebook. Everything here is **kept by a deliberate act**, stored
**verbatim**, and **never digested by reflection** (reflection may file and reference, never
paraphrase). Precious — this is what backup/sync exists for.

- `projects/<project>/` — sedimented work: the **source of record** for kept views (the
  `.jsx` + its assets), and any multi-artifact project. A graduated project's source lives
  here and only here; its build is rebuilt in `views/`.
- `notes/`, `papers/`, … — **agent-curated**, open shape. This is where the conversational
  design's *notebook* lands: exact capability recipes ("call the face-detect API like
  `…`"), references, and the **digested world-doc**. The folder names are the agent's call,
  like a person's Documents; the *rules* are what's fixed (verbatim, reflection-read-only,
  synced).

A drive entry is addressed **from memory** — a facet claim carries the path (`see
drive/notes/facedet`). An orphan drive file nothing in memory points at is a note you forgot
you took: dead weight. Memory is the index; the drive holds the bytes memory refuses to blur.

### views/ — the view workshop (disposable)

Where views are built, and the one fully-disposable tree: everything here is regenerable and
gitignored, so there is **no `.cache` dotdir** — the whole tree is the cache.

- `<project>/` — ad-hoc views start here (source + build). Most are shown once and never
  kept; they die here. Along the way it holds the throwaway of building: compiled `.mjs`
  modules, the worker's preview self-check screenshots, and candidate images fetched before
  the chosen ones graduate to kept assets.
- shared toolchain — esbuild and the headless-preview harness (with its `node_modules`) are
  set up **once** and reused, not duplicated per project. Identical view source still
  compiles at most once.

### prompts/ — what the factory gives (the seed)

Read-only to the agent, materialized at boot from the binary (`include_str!`), operator-
overridable via `*.local.md`. Two flavors of seed:

- **Behavior** — `core.md`, `speaking.md`, `aesthetic.md`, `appearance.md`, `meaning.md`,
  `reflection.md`: how to be. Read as guidance.
- **World priors** — `world.md` *(proposed)*: "YOLO is good for X", "lark-cli does Y". The
  agent reads it like **an article from a kind-of-trusted source**, *digests it into memory*,
  and forms its own updatable understanding. We can push a new version (a correction from the
  source); lived experience supersedes it on conflict. This is the `core.md` pattern pointed
  at the world instead of the self.

### claude-config/ — cognition runtime & records

The ACP/claude subprocess's home (settings, plugins, telemetry, per-session transcripts under
`projects/*/<session>.jsonl`). Mostly **managed** by the cognition layer, not part of the
knowledge design — but the **transcripts are durable records** (ground truth for what a
session actually did; see CLAUDE.md "Testing user journeys live"). `sessions.jsonl` is the
session ledger.

---

## The model behind drive vs. memory

The drive exists because **memory is reconstructive and the agent must not trust it for exact
bytes** — the same reason a person keeps a notebook despite having a good memory. The cut
isn't "two stores"; it's one memory that *offloads* the bytes it refuses to blur:

- **Meaning → memory.** "Face-detection is good for X, prefer it over YOLO when Y" — fuzzy,
  mergeable, grows with use. A facet.
- **Bytes → drive.** "endpoint=…, auth=…, call it like `…`" — exact, looked up, verbatim. A
  notebook page the facet points at.
- **Competence is read, not stored.** How much the agent "knows" YOLO = the shape of the
  evidence (how many *doing* episodes cite it, how recent), computed on read — never a stored
  level field. Claims carry **provenance** (authored-seed < read < did < did-repeatedly), and
  higher provenance wins on conflict, so a lived result quietly overrides a factory prior.
- **Capabilities = skills, not tools.** Equipping a capability is two things: making the
  effector *reachable* (config/env/PATH — not memory) and a *seeded skill* telling the agent
  it exists and how to use it (on-demand, in `drive/notes` + memory). MCP stays the small
  always-loaded control set; the long tail loads on demand.
- **Secrets stay out of the reconstructive layer.** The mind knows "invoke this via that
  skill"; the **effector resolves the secret at call-time** from the drive/env. The token
  never enters the mind's reasoning or a transcript.

## Graduation: ad-hoc → sediment

The lifecycle that ties `views/` to `drive/`:

1. The agent builds an ad-hoc view in `views/<project>/` — source + build together.
2. Most are shown once and never kept; they die in `views/`, unmissed.
3. When something **repeats or proves worth keeping**, the agent *sediments* it: its source
   graduates into `drive/projects/<project>/` (the source of record), and a memory claim is
   written that points at it. The `views/` copy is now just a rebuildable working copy.

Filing = a memory claim taking an address. The keep-bit *is* "a durable claim references it."

## Status & migration

- **Today:** the data dir has `memory/`, `prompts/`, `claude-config/`, `sessions.jsonl`, and a
  single `workspace/` that mixes kept projects with a disposable `.cache/` (compiled views,
  preview harness) — segregated only by the dotdir convention.
- **Target:** split `workspace/` into **`drive/`** (precious — the kept projects move under
  `drive/projects/`) and **`views/`** (disposable — the `.cache/` contents and view build).
  This makes the durability rule a directory boundary ("sync `drive/`, ignore `views/`")
  instead of "back up everything except dotdirs", and retires the `.cache` marker. Served
  paths change from `/workspace/.cache/views/<hash>.mjs` to a `/views/…` path; the view-ref
  resolver, `appearance.md`'s `/workspace/…` asset URLs, and the static route move with it.
- `drive/notes`/`papers` (the notebook), `prompts/world.md`, and explicit claim-provenance are
  **design, not yet built**.

## Open questions

- **World-doc placement** — `prompts/world.md` (with the behavior seeds) or a sibling `seed/`?
- **"Filing" mechanics** — a drive ref is just a path inside a facet claim (leaning this; no
  new index to maintain), or a thin explicit "kept index"?
- **Per-project `dist/` vs. a shared content-addressed compiled cache** — legibility vs.
  compile-once dedup.
- **Credentials in `drive/` vs. env with the drive page only pointing at the env var** —
  leaning pointer, to keep the actual secret off the agent's filesystem-of-record.

## References

- [Architecture](architecture.md) — the layered stack, scenes, the reactor/worker split.
- [Memory subsystem](memory.md) — the contract for `memory/` (raw, episodes, facets, reflection).
- CLAUDE.md — "Testing user journeys live" (transcripts as ground truth), Mac-mini-as-disposable.
