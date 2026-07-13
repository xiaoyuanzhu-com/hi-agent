# reactor / cognition / worker — the tempo split

> **Status: the split is the default and always on** (`split_enabled()` hardcoded true;
> the `HI_AGENT_REACTOR_SPLIT` env flag is retired). The `feat/reactor-communicator`
> change makes the reactor a proper fast **communicator** and fixes the silence the split
> shipped with — see *Current state* below. The legacy agentic reactor path is dead code
> pending deletion.
>
> **The Contract section immediately below is authoritative (agreed 2026-07-12).** It
> supersedes two things in the historical notes: the reactor runs a **smart** model, not a
> small one (§Current state pt. 21 / Open fork 2 are wrong on this), and communication is
> **bidirectional** (cognition initiates too), not one-way report-back.

## The contract (agreed 2026-07-12)

### Governing principle — fast is *no fetch*, not *no knowledge*

The reactor's speed was never about the model tier; it's about **zero round-trips**. So
the layers are cut on two independent lines:

- **Think vs. act.** Reactor and cognition both only *think* — read and reason, never
  touch the world. **Only workers act** (fetch, read files, build a view, open a PR, run a
  task). Workers are the sole layer with side effects.
- **Bounded/instant vs. open/deep.** The reactor thinks *only over what it already holds*,
  in one generation. Cognition thinks *open-endedly* — pulls more memory, reasons in steps,
  takes seconds.

The reactor is a **smart** model precisely because its core skill is judging the edge of
what it knows ("answer from what I hold, or hand to cognition?") — a small model can't do
that. It's fast anyway: one generation, short output, no tools to wait on. Not blind
because its memory is **prepared** (see Stage 1). Fast because it *cannot* fetch.

### The three layers

| | **Reactor** — the mouth | **Cognition** — the mind | **Workers** — the hands |
|---|---|---|---|
| Does | Speaks, holds the floor, manages the interaction | Interprets, reads *more* memory, reasons, plans, decides what work is needed | The actual work |
| Model | Smart | Smart | Task-fit |
| Tools | **None** — empty cwd; no read/fetch/work | Memory reads + reasoning — **never acts** | Everything; only layer with side effects |
| Context | Prepared working set (reasonable size, not tiny) | Full memory, own history, worker reports | Just its task |
| Time | One generation | Seconds+ | However long |
| Emits | `say` (plain text), `show_view`, `delegate`→cognition | `surface`→reactor, `dispatch`→worker, memory read/write | `report`→cognition |
| Never | Touch the world; command workers | `say`; act on the world | Speak; own memory/policy |

Cognition doesn't build the view or open the PR *either* — the moment there's an artifact
or a side effect, that's a worker. Cognition is pure planning + memory; workers are the
effectors. Single mouth: **cognition never calls `say`** — everything it wants said is a
`surface` *proposal* the reactor arbitrates.

### Bidirectional communication — hi-agent is the switchboard

There is **no agent↔agent link**. Both reactor and cognition are reactive ACP subprocesses
that do nothing until prompted and have no clock. All bidirectionality, timers, and routing
live in the Rust host. Each agent has exactly two wires, both to the host:

- **ACP (stdio JSON-RPC)** — host→agent `session.prompt(text)`; agent→host a stream of
  `session/update` notifications (`SessionUpdate::{Text,Thought,ToolCall,…}`), ending in a
  stop. This is how a session is fed and read.
- **MCP over local HTTP (`/mcp`, scene-routed by `X-HI-Scene`)** — the agent's tool calls
  (`show_view`, `surface`, `dispatch`, `report`). The agent's *push* channel.

The bridge between the two sessions is **two host-side tokio tasks + two `mpsc` channels**:

```
   human ─▶┌ reactor driver ─ select! over reactor_rx ─▶ session.prompt ─▶ say/show_view
 (/mcp) ─▶ │                         ▲ reactor_tx        │ delegate → cognition_tx
 surface   └────────────────────────────────────────────┘
   (/mcp) ▶┌ cognition driver ─ select! over cognition_rx + timers ─▶ session.prompt
 report    └──────── surface → reactor_tx · dispatch → worker registry
```

- **reactor→cognition** (`delegate`): after a turn, `cognition_tx.send(Delegate{…})`.
  Non-blocking — the reactor never awaits a reply.
- **cognition→reactor** (`surface`): cognition's `surface` tool call arrives at `/mcp`; the
  handler does `reactor_tx.send(FromCognition{…})`, landing as an ordinary input on the
  reactor's *next* turn (this generalizes today's `LoopInput::Worker` path). The reactor
  arbitrates whether/when/how to voice it.

**The driver owns the clock.** "Cognition brings something up on its own" = the *cognition
driver task* has a `select!` arm on a timer or a worker report; when it fires, the driver
prompts the parked session, which reasons and `surface`s. Two gates keep this human-shaped:
cognition's "is this worth raising?" (memory + `proactivity.md`) then the reactor's "is now
the moment?" (`speaking.md` + presence). The thinking part decides it matters; the social
part decides the timing.

### Warm-up — two sessions, both prepared, neither blocking

- **Reactor warm-up** assembles the prepared working set (identity + `commitments.md` +
  `hot.md` + scene recall) so the *first* turn already holds memory. Cheap file reads →
  ready before the first word.
- **Cognition warm-up** is heavier and **strictly async**: read `commitments.md`/`hot.md`,
  decide which interrupted tasks to resume, and spin up workers for them.

**Non-negotiable:** the reactor's first turn never waits on cognition. History — warm-up
once ran synchronously and blocked the first turn, which *was* the silent-turn bug. So the
reactor's cheap memory prep is the only synchronous part; cognition's warm is background.

### Staging

- **Stage 1 — prepared memory (this branch, `feat/reactor-prepared-memory`).** The reactor
  carries its durable working set (`self.md` + `commitments.md` + `hot.md`) every session,
  inlined because it's tools-off. `mind::memory::working_set()`; wired into the fresh
  reactor turn (retained across warm turns via session memory). Directly fixes the "no
  memory" symptom. **UNBUILT** — build on the Mac.
- **Stage 2 — must-relay + smart model (DONE, unbuilt).** Cognition's report is tagged
  `is_cognition` end-to-end (`WorkerReport`) and `render_report` frames a cognition result
  as a *must-relay instruction to the voice* ("this is the answer you owe them — say what
  you found now"), where a plain worker's report stays an observation on the reactor's own
  timing. `reactor_model()` now returns the **main smart model** (was pinned to the small
  slot — a retired workaround for the old adapter-hang era).
- **Stage 3a — `surface`, cognition-initiated (DONE, unbuilt).** Cognition (and any worker)
  gets a `surface` MCP verb: hand something to the voice *mid-work*, without waiting to
  finish — an interim finding, or something raised on its own initiative. It rides the same
  path as `ask` (`SceneControl::WorkerSurface` → `apply_control` → a `WorkerReportKind::
  Surfaced` report), but crucially it **drives a turn** (returns `Some(LoopInput)`), so the
  voice gets to say it *even with no human input*. That is the mechanism for
  cognition-initiated speech — "bring something up like a person," gated by the reactor's
  own read of the room. A cognition `surface` is must-relay; a task worker's is an
  observation. Single mouth preserved: cognition still never `say`s — it proposes, the
  reactor voices.
- **Stage 3b — full switchboard (DEFERRED).** The remaining, larger pieces: promoting
  cognition from a mute `Worker` into a **first-class session with its own driver task and
  independent timer wake-source** (so it can surface from *idle*, not only while a turn or a
  worker report pokes it); a cognition→worker **`dispatch`** verb (restores the 3rd tier —
  today cognition does multi-step work inline, no fan-out); and the **two-session
  non-blocking warm-up** (reactor prep sync + cheap; cognition warm strictly async, deciding
  task resumptions off the first-turn path). Deferred because it reshapes the carefully-built
  per-scene loop (down-state/backoff/pulse/barge-in) and is best done against a box where it
  can be run; Stages 1–3a already deliver the felt fix (memory + guaranteed relay +
  cognition-initiated word) without touching that loop's core.

## Current state (reactor-communicator change)

What the split actually is today, correcting the historical design notes further down:

- **The reactor is a tools-light ACP session, not a direct Messages call.** The direct
  Anthropic Messages path (below) was tried and **reverted** — the hand-rolled request hung
  on the songguo gateway — so the reactor rides an ACP session, reusing the CLI's proven
  gateway path. It carries `speaking.md` as its system prompt, speaks via plain message
  text (`agent_message_chunk`), and gets a **`show_view`-only** `/mcp` surface.
- **Naming: `SessionRole::ReactorVoice` was collapsed into `Reactor`** (the old agentic
  `Reactor` role is deleted). Cognition is a persistent `SessionRole::Worker`. This
  supersedes the deferred `Reactor → Cognition` rename listed under *Remaining*.
- **Speed came from two real fixes, not from "no tool loop":** (1) the reactor now runs the
  **small model** (a per-role `ANTHROPIC_MODEL` override), where it had silently inherited
  the heavy `ANTHROPIC_MODEL`; (2) `resolve_system()` now **rejects a PATH `claude-agent-acp`
  whose version ≠ the pin**, so a stray global 0.55.x (which hangs every ACP prompt for
  minutes) can't shadow the pinned adapter. A tools-off single generation on Opus/0.55.x was
  itself taking minutes — the original "the agentic loop is the latency" claim was wrong.
- **Views work again.** `show_view` had become unreachable in split mode (tools-off reactor,
  worker surface without it); the reactor now has it, so it can put a worker-built view on
  screen. Expression is enforced reactor-only at dispatch (`dispatch_tool` role guard).

Deferred: deleting the dead legacy agentic path (`voice.rs` gate, the legacy `run_turn`
body, `warm_up`/`open_session`/`discard_reactor_session`/`drive_racing_inbound`/`DriveOutcome`,
the heartbeat hot-swap); and Stage 2 (progressive, presence-paced interim views).

## The problem

The always-present conversational turn was **slow** and **didn't conform to
`speaking.md`**. Both trace to one root cause: **articulation was fused into a
heavyweight agentic ACP loop** — the persistent per-scene "reactor session".

- **Slow.** Even a one-breath reply ("on it") ran the full agentic envelope: a
  `claude`-CLI subprocess doing a multi-step think→tool loop, over a `node`→CLI→HTTPS
  double indirection, on a large frontier model, with a **mandatory `say` tool
  round-trip** (streamed text is dropped, `reactor/mod.rs:1768`) and the CLI's own
  system prompt + full tool schema **re-sent every turn**. The session is already
  *task-free* (it delegates real work to workers) — so the slowness is the **agentic
  thinking itself**, plus **under-delegation** (it starts solving in-line, because
  delegating *well* needs an agentic loop of its own).
- **Poor speaking-rule conformance.** `speaking.md` reaches the model as a **buried
  path reference**: `load_soul` (`identity/mod.rs`) hands the session file paths and
  says "read them all now". `core.md` + `speaking.md` ≈ 34K chars ≈ **~70 % of the
  48K-char working budget** (`heartbeat.rs` `DEFAULT_SWAP_AFTER_CHARS`), Read once then
  receding behind tool results, memory, and task content — least salient exactly when a
  turn happens, and outnumbered by `core.md`'s operational bulk + tool schemas.

## The split (naming)

Three sessions, three tempos:

| Session | Tempo | What it is |
|---|---|---|
| **reactor** *(new)* | fast, **non-agentic** | The single voice + live conversational surface. **One direct Anthropic Messages call** per committed turn: `system` = `speaking.md`, **no tools**, small/fast model; it speaks via the returned text. Owns turn-taking. Presence gates emission only. |
| **cognition** | agentic | The *previous* reactor session, renamed. Keeps tools + delegation. Always thinks / coordinates / delegates and **prepares responses as intents**. Presence-blind. Slow — but now **off the conversational critical path**. |
| **worker sessions** | agentic | Unchanged. Task executors cognition delegates to. |

This also frees the word **"mind"** for the grown memory, the reconciliation
`arch.md`'s merge-notes already wanted.

The cut is by **cognitive tempo (System 1 / System 2)**, not "brain vs. mouth". The
reactor is a *whole small self* for the fast path (it perceives, decides whether/what
to say, and speaks); cognition is the effortful sub-faculty it consults. So the single
self isn't fragmented (which `architecture.md` §3 rightly warns against) — only its
tempo is split. Same spectrum the **reflex** tier already established (reflex = no LLM
→ reactor = one fast LLM call → cognition = agentic loop).

## Why a direct call, not ACP-with-tools-disabled  *(historical — reverted)*

> This section records the original reasoning for a direct Messages call. It was **tried
> and reverted**: the direct request hung on the songguo gateway, and the real latency
> turned out to be the model + a hang-zone adapter, not the ACP envelope. The reactor now
> rides a tools-light ACP session (see *Current state*). The `speaking.md`-as-system-prompt
> goal is met via `reactor_system_prompt()` prepended to the session.

We evaluated reusing the ACP session with tools off + explicit prompts. **Rejected** —
ACP structurally cannot meet the reactor's two goals:

- **System prompt.** `SessionOpts.system_prompt` is only **prepended to the first
  prompt** — ACP has no system-prompt slot (`foundation/acp/process.rs:42-45`;
  delivered as a prefix in `session.rs`). So `speaking.md` would ride as first-user-turn
  content *underneath* the `claude` CLI's coding-agent system prompt. The conformance
  goal (`speaking.md` as the whole framing) is unreachable.
- **Latency.** The `node`→CLI→HTTPS double indirection and the CLI's re-sent built-in
  system prompt + tool schema remain regardless of whether *host* MCP tools are
  dropped (host tools go via empty `mcp_servers`; the CLI's built-ins don't).

A **direct Messages call** makes `speaking.md` the *real* `system` prompt (nothing
underneath), sends no tools, is one HTTPS hop on a small model — meeting both goals. It
reuses the broker-minted **Bearer** key + the songguo base + `net::http_client`, so
it's a small vendor, not a new stack. (This was the "swap to direct if the CLI envelope
is a structural bottleneck" branch we agreed on — and the prepend is structural, not a
tunable.)

## Flow (per committed turn)

1. Input settles — the existing 700 ms commit-after-quiet (`RESPONSE_SETTLE`).
2. **reactor** makes one Messages call (`speaking.md` + presence + conversation tail +
   cognition's prepared intents) → text → **presence gate** → sequencer → TTS.
   Immediate and present.
3. **cognition** is fed the same turn in parallel (it always thinks), delegates as
   needed, and emits **intents** via the existing worker-intent bus (`architecture.md`
   §7 — "a worker produces an intent; the reactor articulates it"; cognition now feeds
   intents the same way).
4. reactor **articulates** cognition's / workers' intents as they land, as the single
   voice — reconciling with what it already said (don't contradict the quick ack).

Presence appears **only** as the emission gate ("hold the `say` if the room's empty");
cognition never considers it. Turn-taking / floor logic (is it my turn?) stays — that's
conversation, not presence.

## Built so far (UNBUILT — build + measure on the Mac mini)

- **`foundation/vendors/anthropic_messages.rs`** — the direct Messages vendor:
  stateless `Config::new` + `complete`, non-streaming, Bearer-authed, `/v1/messages`
  (host-root aware), unit-tested. Registered in `vendors/mod.rs`.
- **`identity::reactor_system_prompt()`** — `speaking.md` inlined as the reactor's whole
  system prompt, under a frame naming it the *voice* and cognition the *hands*.
- **`body/reactor/voice.rs`** — glue: `config_from` (credential → Messages config, small
  slot, raw model) + `speak` + the `split_enabled` env gate. (`AgentConfig`'s fields are
  public, so no accessor was needed.)
- **`body/reactor/run_reactor_turn`** — the split turn path, branched at `run_turn`'s top:
  one Messages call → sequencer (`Beat::Say`), mirroring `run_turn`'s reorg/barge-in (a
  mid-call human burst cancels the request and re-asks). Default off → the agentic path is
  byte-for-byte unchanged.
- **Cognition wiring** — `WorkerRegistry::cognize` runs a **persistent cognition worker**
  (spawn once, follow-up each turn) seeded with the turn's human request; it thinks/works
  off the floor and reports back as an ordinary `LoopInput::Worker` the reactor voices. So
  the reactor is the single fast voice; cognition (agentic) does the work in parallel. No
  MCP/role surgery — a worker is already channel-mute (it reports, never `say`s), and the
  human-only task render keeps cognition from re-ingesting its own report.

## Remaining (fix-forward + next)

- **Build + measure** (Mac mini): compile, run the spike (split vs. default latency +
  speaking-rule feel), fix-forward any blind compile errors.
- **Cognition can't sub-delegate** — the worker role only exposes `ask`, not `delegate`,
  so cognition does multi-step work inline rather than fanning out to sub-workers. Fine for
  v1; a role/MCP change restores the 3rd tier.
- ~~**Rename** `SessionRole::Reactor` → `Cognition`~~ — **superseded.** The
  reactor-communicator change instead collapsed `ReactorVoice` into `Reactor` and deleted
  the agentic `Reactor`; cognition is a `Worker`. See *Current state* above.
- **Trivial-turn cost** — cognition is handed *every* human turn (even "thanks"); the
  reactor's reconciliation suppresses double-speak, but a cheap gate (reactor decides, or
  cognition self-suppresses "nothing to do") would avoid the waste.
- **Promote the env flag** to a config-store tunable once validated.
- **Presence** as the emission gate — fold in the 3-axis model already on `main`.
- **Streaming** — token-stream the Messages reply for fast first word.

## Open forks

1. **cognition persistence** — one warm persistent cognition session per scene (as the
   reactor session is today) vs. spun per turn. Default: keep it persistent/warm.
2. ~~**fast model** — default to the `small` slot~~ — **superseded by the Contract.** The
   reactor runs a **smart** model: its job is judging the edge of what it knows, which a
   small model can't do, and it's fast regardless (one generation, short output, no tools).
3. **streaming vs. not** — non-streaming shipped first (correctness); streaming next.
