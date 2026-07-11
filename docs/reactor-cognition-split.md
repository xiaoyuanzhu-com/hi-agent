# reactor / cognition / worker — the tempo split

> **Status: phases 1–2 on `origin/main` (`feat/reactor-cognition-split`), UNBUILT.**
> The fast reactor voice *and* the cognition-as-worker wiring are implemented (blind — no
> local toolchain, Mac mini down); build + measure + fix-forward. Env-gated
> (`HI_AGENT_REACTOR_SPLIT`, default off → today's agentic path unchanged). Design contract.

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

## Why a direct call, not ACP-with-tools-disabled

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
- **Rename** `SessionRole::Reactor` → `Cognition` (+ `SessionKind`, MCP `X-HI-Role`
  routing) — deferred as risky-blind; cosmetic, and split mode doesn't drive the ACP
  "reactor" session anyway.
- **Trivial-turn cost** — cognition is handed *every* human turn (even "thanks"); the
  reactor's reconciliation suppresses double-speak, but a cheap gate (reactor decides, or
  cognition self-suppresses "nothing to do") would avoid the waste.
- **Promote the env flag** to a config-store tunable once validated.
- **Presence** as the emission gate — fold in the 3-axis model already on `main`.
- **Streaming** — token-stream the Messages reply for fast first word.

## Open forks

1. **cognition persistence** — one warm persistent cognition session per scene (as the
   reactor session is today) vs. spun per turn. Default: keep it persistent/warm.
2. **fast model** — default to the `small` slot (Haiku-class); optionally route
   nuance-heavy turns to a Sonnet-class mid model. Default: `small` for everything first.
3. **streaming vs. not** — non-streaming shipped first (correctness); streaming next.
