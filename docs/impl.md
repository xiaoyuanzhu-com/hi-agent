# hi-agent — Migration Plan

**Status:** migration plan · 2026-06-01 · supersedes the v0.1 build plan

This document is **disposable**. It is the path from *today's code* to the design in
[`architecture.md`](architecture.md) — once the migration lands, delete it. The durable
design contract lives in `architecture.md`; this file only sequences the work and names the
concrete code seams. Where the two disagree, `architecture.md` wins.

---

## Goal

Move the codebase from the current shape — a single shared ACP subprocess, a per-turn
**ephemeral** session, and a reactor that is wired straight to HTTP types — to the target
topology: an **agent session layer** (per-scene process pool, independent handles), a
**persistent reactor session** per scene (hot-swapped, never per-turn), **working sessions**
reached over an async collaboration bus, and a **transport-agnostic reactor**.

Nothing about the design is re-argued here; read `architecture.md` first. This is ordering,
gap analysis, and file-level seams.

---

## Where we are vs. where we're going

| Concern | Today (in code) | Target (`architecture.md`) |
|---|---|---|
| Process model | one shared `AcpProcess` (`lib.rs:73`), passed to `reactor::start` | per-scene pool behind an **agent session layer** (§6) |
| Session lifetime | **ephemeral per turn** — `run_routing_turn` calls `acp.new_session()`, drops it at turn end (`reactor.rs:255,402`) | **one persistent reactor session per scene**, used forever (§5) |
| Context hygiene | journal rebuilt fresh each turn (`build_for_scene`) — session is stateless | warm session + **heartbeat hot-swap** (compact → pre-warm → atomic swap) (§5) |
| Heavy work | done inline in the one session | **working sessions**, capability peers, channel-mute, async bus (§7) |
| Cancel | `session/cancel` on the per-turn session (`reactor.rs:154`) | **fix-forward**; barge-in lands on the always-free reactor session (§5) |
| Transport coupling | reactor imports `server::{AudioEvent, SurfaceEvent, ThoughtBus}`, owns `mime`, `turn`, per-turn frame binding (`reactor.rs:41,271–389`) | reactor speaks **continuous channel signals only**; HTTP artifacts live in the adapter (§2,§3) |
| Carriers | natural-language + `[[surface:…]]` markers parsed in-reactor (`SurfaceExtractor`); tool calls only logged (`reactor.rs:333`) | three carriers, **emission vs. action/perception** split; tool-call carrier real (§4) |
| Naming | "router", `ROUTER_SYSTEM_PROMPT`, "routing turn" | reactor session / working session / cognition; text vs. thought (§8) |

What is **already true** and should not be rebuilt:
- `AcpSession` is already an independent handle — own `session_id` + `rx` + cloned
  connection/routing (`session.rs`); `session_id` demux is already hidden in
  `dispatch_session_update` (`process.rs`). The agent session layer is mostly a *façade +
  pool* over what exists, not a rewrite.
- Commit-after-quiet settle and barge-in are implemented (`reactor.rs:197–237`). Persistence
  changes *what* gets cancelled/reused, not the turn-taking rules.
- The journal is already the durable backstop (`memory/`, `build_for_scene`).

---

## Phases

Each phase is independently shippable and leaves the binary runnable. Order matters: A puts a
clean seam under the session model so B/C/D have something stable to build on; B must exist
before C can swap it; D delegates *from* the persistent session; E and F are seam-move and
cleanup.

### Phase 0 — TTS idle-timeout fix (independent quick win)

- **Goal.** Stop the synthesis session dying mid-turn during long silences.
- **Problem.** `volcengine_tts.rs` closes the upstream socket after a 30 s `IDLE_TIMEOUT`;
  a turn that goes quiet while the agent searches/thinks loses its TTS session and the rest
  of the reply is spoken into a dead socket.
- **Fix.** Keep the session warm for the life of a turn (heartbeat/ping the upstream, or
  re-open lazily on the next `text` push), so silence inside a turn doesn't tear it down.
- **Seam.** `src/voice/volcengine_tts.rs` only. No reactor change. Can land before or after
  any other phase.

### Phase A — Agent session layer (per-scene process pool)

- **Goal.** The reactor stops holding an `AcpProcess`; it talks to a layer that hands out
  **independent session handles** and hides subprocesses, the routing table, and `session_id`
  demux entirely.
- **Decision.** One subprocess **per scene** (Chrome-style isolation). All of a scene's
  sessions — its reactor session and its workers — multiplex inside that scene's process.
- **Why.** Contain blast radius to one scene; keep intra-scene `session/new` cheap (shared
  runtime/MCP). See §6.
- **Facts/limits.** Within-scene shared fate is accepted: a worker OOM can take that scene's
  brain down — recovered by killing the scene's process and rebuilding the reactor session
  from the journal. Cross-scene isolation is hard. Process count = scene count (bounded;
  multi-tenant LRU/idle eviction is later, not now).
- **Seam.**
  - New `src/agent/mod.rs` (working name) — a façade owning `HashMap<Scene, AcpProcess>`,
    lazy-spawn on a scene's first session, plus `session(scene, opts) -> AcpSession`,
    `restart(scene)`. Spawn logic moves out of `lib.rs:62–76`.
  - `reactor::start` takes the agent layer instead of `Arc<AcpProcess>` (`reactor.rs:111`,
    `ReactorInner.acp` at `:88`).
  - `run_routing_turn`'s `acp.new_session(...)` (`reactor.rs:258`) becomes
    `agent.session(scene, …)`.
  - No change to `process.rs`/`session.rs` internals — the handle already exists.

### Phase B — Persistent reactor session per scene

- **Goal.** Replace the per-turn ephemeral session with **one session per scene, reused
  forever** as the brain.
- **Decision.** The scene's reactor session is opened once (lazily) and held in `SceneHandle`;
  each turn `prompt()`s it again instead of `new_session` + drop.
- **Why.** A warm, continuous mind, not a cold per-turn rebuild. Continuity moves *into* the
  session; the journal stays the durable backstop. See §5.
- **Facts/limits.** ACP's one-in-flight-prompt-per-session still holds — turns are serial per
  scene, which the per-scene loop already guarantees. Barge-in now `cancel()`s the *current
  prompt* on the persistent session (`reactor.rs:154` already calls `session.cancel()`); the
  session is **not** dropped. The session will grow — that's what Phase C addresses, so B
  alone is safe only for short-lived runs.
- **Seam.**
  - `SceneHandle` (`reactor.rs:104`) gains `reactor_session: Arc<AcpSession>`, opened in
    `get_or_create_scene` (`:166`).
  - `run_routing_turn` (`reactor.rs:241`) stops calling `new_session` / `drop(session)`
    (`:255,402`); it prompts the held session. The `in_flight` slot becomes "is the reactor
    session mid-prompt," used for barge-in cancel — most of `:149–159,266–269,398–401` stays,
    pointed at the persistent handle.
  - `ROUTER_SYSTEM_PROMPT` is set once at session open, not per prompt.

### Phase C — Heartbeat hot-swap (async auto-compaction)

- **Goal.** Keep the persistent reactor session from rotting or overflowing, invisibly.
- **Decision.** A heartbeat asynchronously (1) summarizes the live session, (2) pre-warms a
  replacement seeded with that summary + a verbatim recent tail, (3) **atomically swaps** it
  in between turns. A hard context-limit hit forces the same swap (hard-stop).
- **Why.** A warm mind without unbounded growth; the conversation never sees a cold restart. See §5.
- **Facts/limits.** The swap happens only between turns (never mid-prompt). The journal stays
  authoritative for durability/recovery/cold-start — if a swap fails, rebuild from snapshot.
- **Seam.**
  - New `src/reactor/heartbeat.rs` (or a `swap` submodule): owns the summarize +
    pre-warm + swap. Reuses the agent layer (Phase A) to open the replacement session.
  - `SceneHandle.reactor_session` becomes swappable (`Arc<Mutex<Arc<AcpSession>>>` or an
    `arc-swap`); `run_routing_turn` reads the current handle at turn start.
  - Summarization is itself an ACP prompt (a working-style session, or a dedicated
    summarizer prompt against a throwaway session in the scene's process).

### Phase D — Working sessions + collaboration bus

- **Goal.** Move heavy / tool-using work off the reactor session so it stays responsive.
- **Decision.** "If something takes more than a few trivial thoughts, use a working session."
  Workers are **capability peers** (share user memory, skills, tools, the right to spawn
  further workers), **channel-mute** (cannot emit/perceive — single-voice coherence). The
  reactor is the lifecycle parent but does not gate a live worker's capabilities.
- **Why.** Responsiveness comes from delegation, not from a model-free reactor. See §7.
- **Facts/limits.** The worker↔reactor link is a **bidirectional async bus**, not
  call-returns-summary: workers post progress/questions/needs; the reactor injects
  guidance/placeholders. Asks are **non-blocking intents** — the worker proceeds with a
  placeholder and reconciles later (fix-forward on missing input). The reactor decides
  when/whether to voice an ask on its own social timing; a **social timeout** (~5 min) fires
  → "proceed with placeholder." Progress-checking is **emergent** (the reactor inspects a
  worker's transcript on demand), so **worker transcripts must be inspectable**.
- **Seam.**
  - A `delegate` path the reactor session can invoke — a reactor-side tool (carrier #3,
    Phase F) that does `agent.session(scene, …)` (worker, in the same per-scene process) +
    prompt + register.
  - New `src/reactor/workers.rs`: worker registry per scene, the async bus (worker→reactor
    intents, reactor→worker injects), the social-timeout policy. The reactor session reads a
    worker's transcript to answer "how's it going."
  - Workers get the shared substrate (memory/skills/tools) but **no channel emit/perceive** —
    enforced by which seams the worker session is granted.

### Phase E — Transport-agnostic reactor seam

- **Goal.** The reactor knows nothing about HTTP. Its interface is **N continuous input
  signal streams in + N continuous output signal streams out**, in human-model vocabulary.
- **Decision.** Push the HTTP-shaped artifacts — utterance = body-close, `mime` →
  `Content-Type`, per-turn frame binding so one turn's audio never bleeds into another
  response — **out of the reactor and into the transport adapter** (the "reactor owner").
- **Why.** Keep the mind aligned to the continuous human model; HTTP is just one batch
  transport, swappable. See §2–§3 and the continuous-vs-batch rule (§1).
- **Facts/limits.** Swap HTTP→WebSocket and the adapter shrinks toward passthrough; the
  reactor is unchanged. The reactor still renders carriers (surface markers, sentence
  coalescing for TTS) — those are *channel* concerns, not *transport*; only wire/framing
  moves.
- **Seam.**
  - Reactor stops importing `server::{AudioEvent, SurfaceEvent, ThoughtBus}`
    (`reactor.rs:41`). Define neutral output-signal types (text stream, audio-frame stream,
    surface stream) with no `mime`/HTTP `turn`-framing in them.
  - `AudioEvent::Start{mime}`, the `turn`-binding, and body-close (`reactor.rs:28–62`,
    `server/audio.rs`, `server/thought_bus.rs`) move into the adapter, which binds neutral
    streams ⇄ the wire.
  - `forward_frames` / `emit_end_of_utterance` (`reactor.rs:422,445`) become adapter-side
    framing of a neutral frame stream.

### Phase F — Carrier convention + naming cleanup

- **Goal.** Make the ACP carrier contract (§4) explicit and fix the names.
- **Decision.** Three carriers: inline markers / typed blocks (emission), tool calls
  (action/perception, request/response). Add a real tool-call carrier (even if one tool to
  start — e.g. a perception or timer tool) so the seam exists, instead of only logging
  `SessionUpdate::ToolCall` (`reactor.rs:333`).
- **Why.** "Emission via natural language; action/perception via tools" is what preserves
  think-then-organize-words. See §4.
- **Facts/limits.** Renaming the `/thought` **wire path** to `text` is a **spec change** in
  `human-interface.md` — raise upstream, don't do it as a local rename. Internal renames
  (`ROUTER_SYSTEM_PROMPT` → reactor, "routing turn" → "turn", router → reactor session) are
  local and safe.
- **Seam.** `reactor.rs` identifiers; a small `tools` module for the reactor session's
  tool-call carrier; `SurfaceExtractor`/`Segmenter` stay (they are carrier rendering).

---

## Dependency order

```
Phase 0  ─ independent, land anytime
A ─▶ B ─▶ C
        └▶ D
A,B ─────▶ E   (E is cleaner once the session model is settled)
all ─────▶ F   (cleanup last; the /thought→text wire rename is a separate spec PR)
```

---

## Stack (unchanged)

| Layer | Choice | Notes |
|---|---|---|
| Language | Rust (2024 edition) | Single binary |
| Async runtime | `tokio` | Multi-threaded scheduler |
| HTTP server | `axum` | Streaming bodies, long-poll |
| ACP client | `agent-client-protocol` | Official crate |
| Memory | JSONL journal + caches | append-only |
| Frontend | Vite + React + TS | embedded via `rust-embed` |
| Logging | `tracing` | structured |
| CLI | `clap` | derive |
| Errors | `thiserror` + `anyhow` | lib / bin split |

No direct LLM SDK — all cognition goes through ACP. Cognition runtime (Node + ACP adapter +
`claude` CLI) is installed into an OS cache dir on first run (`runtime/mod.rs`), not embedded.

---

## Project layout (current)

The actual tree as of this plan — the migration adds the `agent/` layer and `reactor/`
submodules noted above.

```
src/
├── main.rs                  # CLI parse → Config → run
├── lib.rs                   # run(): open memory, spawn ACP, start reactor, serve
├── types.rs                 # Signal, Scene, Channel, Journal/Surface types
├── config/mod.rs            # AgentConfig: upstream, settings.json, child env
├── runtime/mod.rs           # first-run install of node + ACP adapter + claude CLI
├── llm_proxy/mod.rs         # local proxy the adapter talks to instead of upstream
├── acp/
│   ├── mod.rs               # re-exports
│   ├── process.rs           # AcpProcess: child + connection + RoutingTable (demux)
│   └── session.rs           # AcpSession: independent handle, prompt/cancel/close
├── reactor.rs               # per-scene loop, ephemeral turn, carriers   ← biggest change
├── memory/
│   ├── mod.rs · journal.rs · media.rs · snapshot.rs   # journal + build_for_scene
├── server/                  # HTTP front (the transport adapter, today fused to reactor)
│   ├── mod.rs               # axum router, AppState, AudioEvent/SurfaceEvent
│   ├── thought.rs · thought_bus.rs   # text channel in/out (the /thought wire path)
│   ├── audio.rs · surface.rs · vision.rs · stubs.rs · headers.rs
├── voice/
│   ├── mod.rs · stt.rs · tts.rs       # capability traits
│   ├── volcengine_stt.rs · volcengine_tts.rs   # ← Phase 0 fix here
├── channel_log.rs           # structured channel in/out logging
└── appearance/              # embedded SPA + OG tags
    ├── mod.rs · embed.rs · og.rs
```

Target additions: `src/agent/` (Phase A), `src/reactor/{heartbeat,workers}.rs` (C, D),
neutral signal types at the reactor↔adapter seam (E).

---

## Build, dev, deploy (unchanged)

- **Dev:** `cargo run -- --port 8080` for the Rust side; the SPA dev server proxies channel
  routes. Cognition runtime resolves on startup (system PATH, else first-run install into
  the OS cache dir).
- **Release:** SPA built into `appearance/web/dist/`, embedded via `rust-embed`; single
  static binary.
- **Dependency:** the binary uses Node + the ACP adapter + the `claude` CLI at runtime
  (`runtime/mod.rs`); it prefers them from `PATH` (`node` / `claude-agent-acp` / `claude`)
  and only downloads a pinned set when the system doesn't offer all three.

---

## References

- [hi-agent architecture](architecture.md) — the durable design contract this plan migrates toward
- [human-interface spec](../../human-interface/docs/human-interface.md)
- [Agent Client Protocol](https://agentclientprotocol.com)
