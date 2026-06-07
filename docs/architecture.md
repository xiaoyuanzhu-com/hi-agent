# hi-agent — Architecture

## Goal

Build a reference implementation of the [human-interface](../../human-interface/docs/human-interface.md) spec — small enough to read in one sitting, faithful enough to actually talk to. Cognition is delegated to an ACP agent subprocess; hi-agent is the **human-interface layer around it**: the channels, the presence loop, the memory, and the session orchestration the spec implies.

The guiding test for every decision is **fidelity to the human metaphor**, not simplicity at the HTTP or implementation level. Where a choice diverges from how a person would do it, that divergence is named and justified.

This document is the **durable design contract** — the architecture as it is meant to be.

## Design decisions

The critical decisions, each with its reasoning, in roughly descending importance. Later sections elaborate each — reasoning, then facts/limits, then implementation last.

| Decision | Reasoning |
|---|---|
| Cognition is delegated to an ACP subprocess; hi-agent is the human-interface layer around it | Keep presence, channels, and continuity separate from the LLM; the mind stays swappable |
| **Channels live in the reactor, not in cognition** | An ACP session is a single text duplex with no channel concept; the reactor is what gives it multi-channel reach |
| **Transport lives in the owner, not the reactor** | Keep the mind aligned to the continuous human model; HTTP is just one batch transport, swappable for WebSocket or local audio |
| **One persistent reactor session per scene, hot-swapped** | A warm, continuous mind rather than a cold per-turn rebuild; the journal is the durable backstop that makes persistence safe |
| **One subprocess per session** (session-level isolation) | Contain blast radius to a single session; no `session_id` demux. Cost: a fresh spawn + ACP `initialize` per session |
| **Working sessions are capability peers, but channel-mute** | Single-voice coherence — many sub-minds may think, but one mouth speaks |
| **Fix-forward, no real cancel** | More human than a hard cancel; fits ACP's one-in-flight-prompt-per-session constraint |
| **Emission via natural language; action/perception via tools** | "Think, then organize words"; humans don't speak JSON, but do take deliberate, answerable actions |

---

## 1. The organizing principle: continuous vs. batch is a granularity ratio

Every seam in the system is a producer handing work to a consumer. The producer hands off in an **emit-unit**; the consumer acts on an **accept-unit**.

> **Batch iff the emit-unit is finer than the accept-unit. Otherwise pass through.**
> "Continuous" is just the limit where the ratio → 1.

The verdict is always *relative to the consumer*. A sentence is passthrough into TTS (which wants words or more) but batching into cognition (which wants a whole turn). There is no absolutely-continuous component — STT wants ~100–200 ms audio frames, TTS wants words, the LLM wants a turn.

A second axis decides *how* to cut when batching is needed (not *whether*):

- **Mechanical consumer** — just needs *enough signal*. Cut by size or time. (STT: ~100 ms of audio; TTS: a word.)
- **Semantic consumer** — needs a *complete meaning*. Cut by a heuristic boundary. (Cognition: a whole turn → VAD, `?!.`, a quiet-settle timer.)

**The LLM / cognition is the only semantic consumer in the pipe.** That is why **text → cognition is the load-bearing batch boundary** of the whole system: the commit-after-quiet settle (a short timer after the last input fragment) is the adapter that turns a continuous input stream into the discrete turn cognition demands. Everything else — sentence-splitting for TTS, VAD before STT — is *incidental* batching on near-passthrough seams, justified only by provider/prosody granularity and removable in principle.

This single rule recurs at every layer below, including the wire: **HTTP is a batch transport**, and the transport adapter is its batching strategy (§6).

---

## 2. The layered stack

Five layers, each with a single responsibility and a clean contract to the layer below.

```
   participant (human / device)
        ▲ │
   wire │ ▼   HTTP / WebSocket / local audio …
 ┌──────┴───────────────────────────────────────────────┐
 │ Transport adapter  (the "reactor owner" / host)       │  wire, framing, mime,
 │   binds continuous channel signals ⇄ a concrete wire  │  long-poll, body-close
 └──────┬───────────────────────────────────────────────┘
        ▲ │   continuous channel signals (human-model vocabulary, no transport)
 ┌──────┴───────────────────────────────────────────────┐
 │ Reactor module  (per scene)                           │  connects all channels,
 │   fan-in N input channels → one prompt                │  decides & articulates,
 │   fan-out one output stream → N channels              │  always responsive
 │   ┌─────────────────────────────────────────────┐    │
 │   │ Reactor session  — the persistent brain      │    │  speaks; owns channels
 │   └─────────────────────────────────────────────┘    │
 │   ┌─────────────────────────────────────────────┐    │
 │   │ Working sessions — ephemeral, channel-mute   │    │  deliberate, use tools
 │   └─────────────────────────────────────────────┘    │
 └──────┬───────────────────────────────────────────────┘
        ▲ │   independent session handles (one subprocess each)
 ┌──────┴───────────────────────────────────────────────┐
 │ Agent session layer                                   │  one subprocess per session
 │   spawns a process per session; no session_id demux   │  (session-level isolation)
 └──────┬───────────────────────────────────────────────┘
        ▲ │   ACP JSON-RPC over stdio
 ┌──────┴───────────────────────────────────────────────┐
 │ ACP agent subprocess (claude-code)                    │  cognition
 └───────────────────────────────────────────────────────┘
```

Each boundary is a clean contract:

| Boundary | What crosses it | What is hidden |
|---|---|---|
| participant ⇄ adapter | a concrete wire protocol (HTTP today) | everything above |
| adapter ⇄ reactor | **continuous channel signals**, human-model vocabulary | transport, framing, mime, long-poll |
| reactor ⇄ session layer | **independent session handles** (prompt / read updates / drop) | the subprocess each handle owns, ACP `initialize` |
| session layer ⇄ subprocess | ACP JSON-RPC | per-session spawn, isolation |

The two rules that place responsibility:

- **Channels live in the reactor, not in cognition.** An ACP session is a single text duplex with no notion of audio/vision/surface. The reactor is the mux/demux that gives cognition multi-channel reach.
- **Transport lives in the owner, not in the reactor.** The reactor knows only continuous signals; the adapter binds them to a wire.

---

## 3. The reactor module

**The reactor is where all channels meet and where decisions are made.** It is the always-responsive presence mind: it perceives every input channel, decides what to do and when, and articulates on the output channels — turn-taking, progress replies, proactive speech. It is the *only* layer that touches channels, and it hosts the persistent reactor session that does the deciding (§5) plus the working sessions it delegates to (§7). It must be **responsive, lightweight, fast**.

Two decisions place its boundaries:

- **Channels live here, not in cognition.** Cognition is a single text duplex with no notion of audio/vision/surface; the reactor is what gives it multi-channel reach.
- **Transport does not live here.** The reactor's interface is **N continuous input signal streams in + N continuous output signal streams out**, in the human-model vocabulary of senses and expressions — with zero knowledge of HTTP, long-poll, chunked bodies, or mime. The transport adapter owns the wire. So artifacts like *utterance = HTTP body-close*, the mime that sets `Content-Type`, and per-turn frame binding (so one turn's audio never bleeds into another response) live in the adapter, not the mind. Swap HTTP for a continuous transport and the adapter shrinks toward passthrough; the reactor is unchanged.

**Reasoning — why deciding concentrates here.** A person's presence and deliberation share one locus: you perceive, decide, and speak as one self. Splitting "what to say" from "how and when to say it" across modules would fragment that. The reactor is that single locus; cognition is the slow sub-faculty it consults.

**Implementation — adapting many channels to ACP's one conversation.** ACP carries a single conversation, so the many-channel reality is mechanically reduced to it: input channels are *fanned in* to one prompt (plus the memory snapshot); the one output stream is *fanned out* back into channels by the carrier rules (§4). This fan-in/fan-out is a *consequence* of ACP's conversation form, not the point of the reactor. (Today audio fans into the text channel because STT is lossy — symbols kept, prosody discarded — and re-diverges only once we model paralinguistics.)

---

## 4. The ACP carrier contract

Cognition is reached over ACP (JSON-RPC over subprocess stdio). **ACP has no concept of channels.** It offers a text/content duplex plus a tool-call mechanism; the reactor imposes channel semantics on top. Knowing the exact vocabulary is what lets the reactor model every channel action.

### What ACP puts on the wire

- **Input** — one verb: `session/prompt`, carrying `Vec<ContentBlock>`. Content blocks are typed: **text / image / audio / resource / resource_link**. Every input channel must fan-in here.
- **Output** — `session/update` notifications, variants: `agent_message_chunk` (a content block — the agent's spoken/text output), `agent_thought_chunk` (internal reasoning), `tool_call` / `tool_call_update`, `plan`, and meta updates.
- **Agent→host requests** — the agent asking the host to *do* or *perceive* something: tool calls (incl. MCP tools the host registers), filesystem, terminal, permission.

### The three carriers

Because ACP has no "speak on audio" or "show a card" primitive, every channel action is encoded with one of three carriers:

1. **Inline text markers** in the output stream (e.g. a surface block delimited by markers). Schema-less and natural — honors "humans don't speak JSON" — but in-band signalling, parsed by a streaming extractor.
2. **Typed content blocks** (audio / image / resource), routed by type. Primarily an *input*-side lever (vision as image blocks); the model does not natively emit audio blocks for speech.
3. **Tool calls** — the brain calls a tool the reactor implements. Structured arguments, out-of-band, and **request/response** (so it can return a value).

### Emission vs. action/perception

The carrier choice follows a line that is both technically real (notification vs. request/response) and human:

- **Emission — fire-and-forget output → carriers #1/#2 (natural language + markers/typed blocks).** Speaking and showing. The brain merely *expresses*; the reactor renders to the right channel. No return value. (A person talks and gestures without invoking an API.)
- **Action / perception — needs a result or has structured arguments → carrier #3 (tool calls).** "Look at the camera now," "what is on screen," "set a timer." Request → response. (A person deliberately turns to look, picks up the cup.)

Keeping the brain's *voice* in natural language while routing *answerable* needs through tools is what preserves the "think, then organize words" separation: cognition expresses intent; the reactor articulates it.

### Channel × carrier (default convention)

| Channel | Direction | Carrier |
|---|---|---|
| text | in / out | content block (text) / output stream |
| audio | in | content block (text after STT today; audio block once paralinguistic) |
| audio | out | output stream → TTS (reactor-side) |
| surface (rich content) | out | inline markers (emission) |
| vision | in | content block (image) or a perception tool |
| action (timers, device control, …) | out | tool call (request/response) |

The convention: **emission stays markers/natural-language; anything that needs a return value or carries structured arguments is a tool call.**

---

## 5. Session lifecycle

### One persistent reactor session per scene

Each scene has exactly one reactor session, used **forever** as the brain — not re-created per turn. Its context is kept clean, lightweight, and responsive. Continuity is *in the session*, with the journal as the durable backstop (below).

### Heartbeat hot-swap (asynchronous auto-compaction)

A long-lived session would eventually rot or overflow. Instead of letting it, a **heartbeat** asynchronously:

1. summarizes the live reactor session,
2. pre-warms a replacement session seeded with that summary plus a verbatim recent tail,
3. atomically **swaps** the replacement in between turns — invisible to the conversation.

On a hard context-limit hit, the same mechanism runs as a forced **hard-stop swap**. The swap is a runtime concern; the conversation never sees it.

### The journal is the durable backstop

Every signal in and out is written to the journal before anything reacts to it. The journal — not session lifetime — is authoritative for durability, recovery, and cold-start. If a session (or its process) dies, the reactor session is rebuilt from a journal snapshot. This is what makes the persistent-session model safe.

### Fix-forward, no real cancel

There is no true interruption or cancel. New input — including a correction or a barge-in — is simply incorporated by the always-free reactor session, which corrects course. This is *more* human than a hard cancel, and it fits the one-in-flight-prompt-per-session constraint: interruptions land on the reactor session, never on a busy worker.

---

## 6. The agent session layer and the process model

The reactor never sees subprocesses. It talks to an **agent session layer** that exposes each ACP session as an **independent handle** — prompt it, read its updates, drop it to close. Each handle owns one subprocess; dropping the handle tears that process down.

**Granularity: one subprocess per session.** Every session — a scene's persistent reactor session, each ephemeral working session, the throwaway summarizer a hot-swap briefs from — runs in its own subprocess. There is no `session_id` demux: a connection hosts exactly one session, so its notifications flow straight to that handle's stream.

Consequences, all deliberate:

- **Session-level isolation.** One session's crash or OOM cannot touch another — not a sibling worker, and not the scene's reactor brain. (A scene used to be the isolation unit, with within-scene shared fate accepted; per-session isolation is strictly finer.)
- **Hard cancel is available.** A session can be force-killed by dropping its handle — its process exits, independent of every other. We still default to fix-forward/no-cancel (§5); the capability simply exists where it didn't.
- **Cost: a spawn per session.** Each session pays a subprocess spawn + ACP `initialize` + MCP `tools/list` round-trip, where pooled intra-scene `session/new` used to be near-free. That spawn cost is the live risk to watch (`risks.md`); the simpler wire and finer isolation are the accepted return.
- **The scene stays a *logical* grouping**, not a process boundary: the reactor's per-scene queue, memory slice, and the `X-HI-Scene` tag on each session's MCP attach are unchanged.

ACP permits both one-connection-many-sessions and one-process-per-session; this layer chooses one process per session, keeping the handle interface unchanged so the choice stays swappable.

---

## 7. Delegation and the worker collaboration bus

Responsiveness comes from **delegation**, not from keeping the reactor model-free. The principle:

> **If something takes more than a few trivial thoughts, use a working session.**

The reactor session keeps a clean, fast context and spins off heavy or tool-using work to ephemeral working sessions.

### Working sessions are capability peers, not children

A working session and the reactor session are **peers in capability**. Both reach the full inner substrate — user memory, learned skills, tools, the right to spawn further workers. The reactor is the *lifecycle* parent (it spawns and can tear down a worker) but does **not** gate a live worker's capabilities.

The one asymmetry: **channels are exclusive to the reactor session.** A worker cannot emit on or perceive a channel — it cannot directly speak or show. The reason is **single-voice coherence**: reading memory or skills never conflicts, so it is shared; but many sub-minds emitting to the person at once is chaos, so the channel funnels through a single serializing articulator. A worker that wants to reach the person produces an *intent*; the reactor articulates it.

### The bus is bidirectional and async

Delegation is not "call a worker, get a summary." During a run:

- the **worker** can post progress, a question, or a need-for-input ("need vendor account credentials");
- the **reactor** can inject information, guidance, or "proceed with a placeholder."

Asks are **non-blocking intents**, not blocking calls. The worker proceeds with a placeholder and reconciles later — **fix-forward on missing input**, the same spirit as fix-forward/no-cancel. The reactor decides *when, whether, and how* to voice an ask on its own social timing:

- the person said "don't bother me for an hour" → hold the ask, keep building with placeholders;
- the person said nothing and no answer arrives in a few minutes → the reactor's **social timeout** fires and it tells the worker to proceed with a placeholder.

That wait is a reactor *policy*, never a worker block. Progress-checking is therefore **emergent**, not a native feature: when the person asks "how's it going," the reactor decides to check — e.g. by consulting a worker's transcript — and articulates a clean answer. (This requires worker transcripts to be inspectable, so one worker can be seeded with another's history.)

---

## 8. Naming

Three non-overlapping words, never reused:

- **text** (message) — the direct-symbol I/O channel: words in and out, no rendering. The honest name for "decoded symbols." (Currently rides the `/thought` endpoint; renaming the wire path is a spec change to raise upstream.)
- **thought** — the model's *internal reasoning* (ACP `agent_thought_chunk`). This meaning is preserved and is **not** reused for the deliberation layer.
- **cognition** — the deliberation layer; its concrete unit is a **working session**.

Other load-bearing terms:

- **channel** — one sense or expression stream (text, audio, vision, surface, …).
- **reactor module** — the transport-agnostic Rust mux/demux and presence loop.
- **reactor session** — the persistent per-scene brain.
- **working session** — an ephemeral, channel-mute, delegated cognition unit.
- **transport adapter** (a.k.a. the reactor *owner* / host) — binds continuous channel signals to a concrete wire.
- **agent session layer** — spawns one subprocess per session, exposing independent session handles.
- **scene** — the situation a signal belongs to (with a person, a group, or alone); the context-isolation unit. One reactor session and one memory slice per scene; each session runs in its own subprocess. Participants — the humans or devices in a scene — are soft, inferred from content, not a structural key.

---

## References

- [human-interface spec](../../human-interface/docs/human-interface.md)
- [Agent Client Protocol](https://agentclientprotocol.com) · [schema](https://agentclientprotocol.com/protocol/schema)
