# reactor / cognition / worker — the tempo split

> **Status: in progress on `feat/reactor-cognition-split`.** The direct-LLM wire is
> written (unbuilt — no local toolchain, Mac mini down); the reactor-loop rewiring is
> the remaining work and wants a compiler. This doc is the design contract for it.

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

## Built so far

- **`foundation/vendors/anthropic_messages.rs`** — the direct Messages vendor:
  stateless `Config::new(token, base_url, model)` + `complete(cfg, system, &[Turn])`,
  non-streaming, Bearer-authed, `/v1/messages` endpoint (host-root aware), unit-tested
  (request shape, endpoint construction, reply parsing). Registered in `vendors/mod.rs`.
  **Unbuilt** — verify on the Mac mini.

## Remaining (wants a compiler — iterate, don't write blind)

1. **`AgentConfig` accessor** returning `(upstream_base_url, upstream_key,
   small.or(model))` for the reactor — the **raw** model id (never the `[1m]`
   context-window suffix `with_context_window` adds; that's a CLI-ism).
2. **Reactor turn path**: on a committed turn, assemble the reactor prompt and call the
   Messages vendor instead of spawning an ACP session; feed the returned text to the
   sequencer. The delivery seam is already model-agnostic — the sequencer voices any
   string (canned status lines already do, `reactor/mod.rs`), and the current
   `SessionUpdate::Text` drop (`reactor/mod.rs:1768`) becomes "consume as speech" on the
   reactor path.
3. **Rename** the ACP reactor session → **cognition** (`SessionRole::Reactor` →
   `Cognition`, `open_session`, observatory `SessionKind`).
4. **reactor ↔ cognition intent bus** — reuse the §7 worker-intent mechanism; add
   single-voice reconciliation so a landed intent doesn't contradict an earlier ack.
5. **Config**: a fast-model key, defaulting to the existing `small` slot
   (`LlmCredentials.small`, `credentials.rs`).
6. **Presence** as the emission gate — fold in the 3-axis model already on `main`.
7. **Streaming** (follow-up): token-stream the Messages reply for fast first word.

## Open forks

1. **cognition persistence** — one warm persistent cognition session per scene (as the
   reactor session is today) vs. spun per turn. Default: keep it persistent/warm.
2. **fast model** — default to the `small` slot (Haiku-class); optionally route
   nuance-heavy turns to a Sonnet-class mid model. Default: `small` for everything first.
3. **streaming vs. not** — non-streaming shipped first (correctness); streaming next.
