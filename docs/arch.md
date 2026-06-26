# hi-agent — Architecture (faculties)

> **Second architecture doc, to be merged into `architecture.md` later.** That doc
> is the *runtime data-flow* contract (how a signal flows: wire → adapter → reactor
> → session → ACP; continuous vs. batch; carriers). This one is the *static
> organization* contract: **what is built vs. grown, and which layer every feature
> and line of code lives in.** They are complementary lenses on one system. The one
> term they currently fight over — "mind" — is reconciled under [Merge notes](#merge-notes).

## Goal

Give every feature a single, mechanical home. When we add anything — a sense, a
skill, a fact, a habit — one question decides where it goes:

> **Is this built by the developer (sealed in the binary) or grown by the agent
> (data, accumulated in use)?**

The guiding test is the same as `architecture.md`'s: **fidelity to the human
metaphor.** A person is a *built body running a grown mind* — manufactured hardware
that never changes, and a mind that learns for a lifetime. The code should be shaped
the same way.

## The model: a built body, a grown mind

Four faculties, plus the engine beneath them. Two are factory-sealed **code**; two
are agent-grown **data** with a thin code port.

```
        data/ — GROWN by the agent, accumulates in use         (soft · the agent's pen)
   ┌───────────────────────────┬───────────────────────────────┐
   │ identity                  │ mind                           │
   │ who it is                 │ what it knows & remembers      │
   └─────────────┬─────────────┴──────────────┬────────────────┘
                 ▲   the loader composes them into each moment
   ┌─────────────┴────────────────────────────┴────────────────┐
   │ body — built so the agent can perceive & act,              │  (hard · factory)
   │ plus the loops that keep it running and the loader          │
   └───────────────────────────┬────────────────────────────────┘
                                ▲  built on
   ┌────────────────────────────┴───────────────────────────────┐
   │ foundation — the engine; no agent-meaning                   │  (hard · factory)
   └──────────────────────────────────────────────────────────────┘
```

| Faculty | Substance | Holds |
|---|---|---|
| **foundation** | factory code | the engine: server, runtime, store I/O, the LLM/ACP gateway, process mgmt, the layered-config cascade, the build pipeline, vendor adapters. No agent-meaning. |
| **body** | factory code | everything built so the agent can perceive and act — senses, actions, the always-on loops (pulse, reflection clock, perception), and the loader that assembles each moment of awareness. |
| **identity** | grown data + thin port | who the agent is: character, the per-install authored self, its standing commitments. Always-loaded. The slowest-changing thing it has. |
| **mind** | grown data + thin port | what the agent knows and remembers: episodic record, learned facts, learned skills and views. On-demand. Grows constantly. |

## The placement rule

**Default every feature to the mind (soft, grown). A feature earns its way into
code (body or foundation) only by passing one of three gates.** Importance is *not*
a gate — `identity` is the most important faculty and it is maximally soft.

Ask three yes/no questions, first *yes* wins; all *no* → it is mind:

1. **Privilege** — does it need something the agent can't obtain at runtime (open a
   route/socket, touch a device, spawn a process, read the host filesystem)? →
   **body** (a sense or action) or **foundation**. It cannot be expressed as a prompt.
2. **Autonomy of firing** — must it run *without the agent choosing to*, on a clock
   or an event? → **body** (a loop) — but thin: the trigger is hard wiring, the
   judgment it wakes is soft (mind).
3. **Catastrophe** — must an invariant hold even if the agent is confused or
   adversarially pushed, where a soft failure is irreversible? → a **hard floor**
   (kept deliberately near-empty; see `soft guidance, no homegrown security`).

**Almost no feature is one layer.** Each is a *thin hard spine + soft flesh*, and the
craft is drawing the line as low as possible. Examples already in the tree:

| Feature | Hard spine (body/foundation) | Soft flesh (mind) |
|---|---|---|
| people recognition | capture + embedding compute | "same person? what's their name?" — clusters, names |
| screen control | grab frame + dispatch click | see → decide what/where to click |
| file exchange | the upload route + drive read/write | save-where, find, organize |
| reflection | the clock that fires it | what to consolidate, what to write |

## The two axes

- **built vs. grown (code vs. data) — who holds the pen.** The agent's pen *only ever
  touches `data/`*. `foundation` and `body` are developer-authored Rust; `identity`
  and `mind` are agent-authored files (seeded by the developer at install). Wire the
  agent's write tools so they cannot reach `src/`.
- **always vs. on-demand — the loader's job, not a folder.** Whether a piece is always
  in context (identity) or retrieved when relevant (most of mind) is decided at
  session-assembly time by `body`'s loader. Do not make `always/` and `on-demand/`
  directories.

**Learning-rate gradient:** `foundation`+`body` are never written at runtime (factory
only); `identity` is written *slowly* (reflection/consolidation, never a per-turn
edit); `mind` is written constantly. **Only the left column grows.** You never learn
new hardware — only the mind that drives it.

> Agent-run code (a saved snippet, a skill that stands up a server) is **mind**, not
> body. The axis is *sealed in the binary* vs. *agent-owned*, not *code* vs. *prose*.

## The harmonization cascade (three authors)

`identity` and `mind` are each written by **three authors — factory, user, and self**
— and they coexist by *layering*, never by sharing a file:

```
data/identity/  base (factory, replaced on upgrade) ‹ user ‹ self
data/mind/      seed (factory, replaced on upgrade) + learned   (learned shadows seed)
```

The loader composes the layers at load time. Conflicts resolve by precedence (user is
sovereign; self-growth yields to an explicit user setting; the factory base is the
irreducible floor the others extend). **A binary upgrade replaces only the factory
layer; `user`/`self`/learned are untouched deltas — so there is never a merge
conflict, only a precedence decision.** Provenance is durable metadata that does *not*
decay (so "user-told vs. self-inferred" stays answerable — a deliberate divergence
from human source-amnesia).

*Already partially built:* `reactor::install_prompts` / `compose_prompt` layer a
bundled base under an operator `*.local.md` override — the same cascade, today
spanning only base + operator. The model generalizes it to base ‹ user ‹ self.

## Dependency rule

One direction only:

```
foundation  ←  body  ←  (body's loader consumes)  identity, mind
```

`foundation` imports no faculty. `body`/`identity`/`mind` are built on `foundation`;
the loader (a body organ) consumes the `identity` and `mind` ports. Nothing imports
upward. Swap an engine piece without touching a faculty; add a sense without touching
the engine.

## The faculties, and where today's code lands

The current tree is organized by *technical layer* (`acp`, `server`, `reactor`,
`memory`, …), not by faculty. The mapping:

| Faculty | Today's modules | Notes |
|---|---|---|
| **foundation** | `acp`, `agent`, `runtime`, `mcp`, `vendors`, `config`, `models`, `appearance`, `observatory`, `channel_log`, `pcm`, `segment`, `types` | the engine. `vendors` is already the clean impl-layer under `capabilities`. |
| **body** | `capabilities` (senses + actions), `reactor` (loops, loader, sequencer, workers), `presence`, `gesture` | `capabilities`↔`vendors` already maps cleanly to body↔foundation. |
| **identity** | **scattered, no home today**: `load_soul` + `install_prompts` + `compose_prompt` + the `core/speaking/meaning.md` bases (in `reactor/mod.rs`); `self`/`commitments`/`hot` paths (`memory/layout.rs`); `refresh_hot` (`memory/core.rs`) | consolidating these is the model's biggest readability win. |
| **mind** | `memory` (journal, snapshot, episodes, facets, decay, media, people_vectors, layout, core), `views` | the agent-grown store; `views` are learned procedural/presentational memory. |

Frictions to resolve during migration: `server/` tangles transport (foundation) with
channel semantics (body); identity is smeared through the hot reactor loop; and the
"mind" term collides with `architecture.md` (below).

## Migration — status

The grouping shipped to `main` in four build-green, tested commits:

1. **`identity`** ✅ — `load_soul`, the prompt cascade, and the `self`/`commitments`
   path helpers moved out of `reactor`/`memory` into `src/identity/`.
2. **`mind`** ✅ — `memory` + `views` moved under `src/mind/` (the faculty's home and
   namespace; the provenance-tagged, seed-shadowing write *port* is still future work).
3. **`body`** ✅ — `capabilities`, `reactor`, `reflex`, `presence`, `gesture` under
   `src/body/` (the always-on apparatus + loops).
4. **`foundation`** ✅ — the pure-Rust engine modules (`acp`, `agent`,
   `mcp`, `vendors`, `config`, `models`, `observatory`, `channel_log`, `pcm`, `segment`,
   `server`) under `src/foundation/`.

`src/` is now `body/ foundation/ identity/ mind/` plus three deliberate **root
exceptions**:
- **`appearance`** and **`runtime`** carry build assets behind hardcoded paths (the SPA's
  RustEmbed `dist/` folder; the runtime's `CARGO_MANIFEST_DIR` npm includes) — relocating
  them would break the Makefile / dev server / embed, so they stay at root.
- **`types`** — shared cross-faculty vocabulary, not engine machinery.

**Already satisfied — no work needed:**
- The **`server/` transport-vs-channel-semantics split** the model implies is already
  realized by the existing boundary. `server/` (now `foundation/server`) *is* the HTTP
  transport adapter — handlers bind wires ⇄ transport-free `Signal`s and the binder does
  the framing/Content-Type — while the channel *semantics* (turn-taking, fan-in to one
  prompt, when-to-speak) live in `reactor` (now `body/reactor`). No mislocated chunk to move.

**Intentionally not built — infrastructure ahead of need:**
- The **`mind` write-port** (provenance tags + seed-shadowing over the memory write
  path). Deferred on purpose, not blocked: nothing consumes provenance yet; there is no
  mind-seed store to shadow (`world.md` is a prompt the agent *reads*, not a shadowed
  store); and the write-discipline already holds by convention (memory is written via
  scoped MCP tools, not free filesystem writes). Build it when a concrete consumer exists
  — answering "did you tell me this, or did I infer it?", or a shadowed `world.md` — not
  before. Until then the "agent's pen only touches `data/`" rule stays convention.

**Genuine future work:**
- **Reconcile vocabulary** with `architecture.md` and merge the two docs (the "mind"
  rename — see below).

## Merge notes

This doc and `architecture.md` are complementary, not competing:

- `architecture.md` — **runtime data-flow**: how a signal becomes a turn and a reply.
- `arch.md` (this) — **static organization**: what is built vs. grown, where code lives.

The one collision to settle on merge is the word **"mind"**:

- `architecture.md` calls the **reactor session** "the mind / the brain" (the cognition
  locus).
- This doc calls the **accumulated memory** the "mind."

Proposed resolution: **reserve "mind" for the grown memory.** The reactor is **body**
(the loops + loader). The cognition the reactor hosts keeps its precise existing names
— *reactor session*, *working session*, *cognition* — which already avoid "mind" in
the load-bearing parts of `architecture.md` §8.

## References

- [`architecture.md`](architecture.md) — the runtime data-flow contract
- [`human.md`](human.md) — the human behaviors we model
- [`data-dir-layout.md`](data-dir-layout.md) — the `data/` tree this doc's `identity`/`mind` sit over
