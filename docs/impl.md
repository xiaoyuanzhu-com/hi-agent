# hi-agent — Implementation Plan

A sample Rust implementation of the [human-interface](../../human-interface/docs/human-interface.md) spec.

**Status:** design v0.1 · 2026-05-28 · pre-implementation

---

## Goal

Build a reference implementation of the human-interface spec — small enough to read in one sitting, complete enough to actually talk to. The agent's cognition is delegated to `claude-code` over ACP; hi-agent is a translation layer between the spec's HTTP channels and ACP, plus the runtime support the spec requires (memory, approval routing, aliveness).

This document is the implementation contract. The spec defines *what* an agent is; this defines *how* this particular agent is built.

---

## Design principles

1. **Spec-faithful on the front.** The HTTP surface obeys human-interface to the letter: channels are independent, bodies are signals, closing the body ends an utterance, long-poll is the output mechanism, no sessions on the wire.
2. **Delegate cognition.** hi-agent never calls an LLM directly. Cognition lives in `claude-code` and is reached via ACP. The translation layer owns the spec's primitives; the runtime owns capabilities.
3. **Memory as substrate.** Continuity is in explicit persistent storage, not session lifetime. Routers can be ephemeral because memory is durable.
4. **Reactor never blocks.** The Rust core stays responsive at all times. Cognition runs inside ACP sessions; the reactor only routes, dispatches, and stamps.
5. **One concern per worker.** Long-running tasks live in their own ACP worker sessions. The reactor and routers don't carry their state.
6. **Single binary.** Static-linked release, single-process, web surface embedded at build time. Docker layered on top, not required.

---

## Stack

| Layer | Choice | Notes |
|---|---|---|
| Language | Rust (2024 edition) | Single static binary, cross-platform |
| Async runtime | `tokio` | Multi-threaded scheduler |
| HTTP server | `axum` | Streaming bodies, long-poll friendly |
| ACP client | `agent-client-protocol` | Official crate from Zed |
| MCP server | `rmcp` (or equivalent) | In-process, for router toolbelt |
| Memory | JSONL files + in-memory caches | `tokio::fs`, append-only |
| Frontend | Vite + React + TypeScript | Single SPA, embedded in binary |
| Embedding | `rust-embed` | `web/dist/` → binary at compile time |
| Logging | `tracing` + `tracing-subscriber` | Structured logs |
| CLI / args | `clap` | Standard derive style |
| Errors | `thiserror` (library) + `anyhow` (binary) | Conventional split |
| Tests | `tokio::test`, `reqwest`, optional `wiremock` | Integration over unit |

We do not depend on any direct LLM SDK. All LLM-flavored work goes through ACP.

---

## Architecture

```
  peers              hi-agent  (Rust process)              claude-code subprocess
 ───────            ──────────────────────────             ──────────────────────────

  alice ──POST /thought──┐
                         │   ┌─────────────────┐    ACP    ┌────────────────────┐
   bob  ──POST /vision──▶├──▶│   axum server   │ ◀──stdio▶ │ session: router    │
                         │   └────────┬────────┘           │  (ephemeral)       │
   bob  ◀──GET /thought──┘            │                    ├────────────────────┤
                                      ▼                    │ session: worker A  │
                             ┌─────────────────┐           │  (long-lived task) │
                             │     Reactor     │           ├────────────────────┤
                             │  per-peer queue │           │ session: worker B  │
                             │  worker reg.    │           │  (long-lived task) │
                             └────────┬────────┘           ├────────────────────┤
                                      │                    │ session: ...       │
                                      ▼          MCP       │                    │
                             ┌─────────────────┐ ◀──stdio▶ │  ← tool calls      │
                             │ in-proc MCP     │           └────────────────────┘
                             │ Memory journal  │ ← continuity
                             │ Heartbeat       │ ← aliveness
                             └─────────────────┘
```

### Four primitives

1. **HTTP human-interface (front).** axum exposes one endpoint per channel. `POST` accepts a signal; `GET` long-polls one out. Headers carry sender identity. Body-close ends the utterance.
2. **ACP (back).** One `claude-code` subprocess for the lifetime of hi-agent. Many ACP sessions live inside it — some ephemeral (routers), some long-lived (workers). Created and destroyed via JSON-RPC.
3. **Memory (continuity).** JSONL files on disk: a global signal journal and per-peer state. The durable record of the relationship. Every signal in and out is written before anything reacts to it.
4. **Reactor (concurrency control).** Always-responsive Rust core. Owns the per-peer queues, the worker registry, the channel broadcasts. Never blocks on cognition.

---

## Routing layer

**Per peer (`X-HI-From`), ephemeral.** Not per channel, not single, not micro-per-signal-globally.

- A signal arrives → the reactor identifies the peer → the peer's queue takes the lock.
- The reactor spawns a fresh ACP session in the existing `claude-code` subprocess.
- The session is prompted with the new signal plus a memory snapshot of that peer's relationship (recent journal entries, running workers tagged to this peer, pending intents, open approvals).
- The router decides what to do via in-process MCP tools.
- Session closes when the prompt completes. The peer's queue releases.

### Three concerns, three places

| Concern | Lives in |
|---|---|
| Continuity (knowing previous messages) | Memory |
| Concurrency control (one peer's signals don't race) | Reactor (per-peer queue) |
| Cognition (the actual decision) | Ephemeral ACP session |

A long-running router would conflate all three into session lifetime. Memory-as-substrate makes them independent.

### Router toolbelt (via in-process MCP)

| Tool | Effect |
|---|---|
| `speak(channel, to, body)` | Emit on the named channel, addressed to a peer via `X-HI-To` |
| `spawn_worker(brief, channel_out)` | Start a new ACP session as a worker for one concern |
| `cancel_worker(id)` | Cancel a running worker |
| `list_workers()` | Snapshot of running workers and their briefs |
| `set_intent(when, what)` | Record a deferred intent in `intents.jsonl` |
| `recall(query)` | Search the journal (across all peers if needed) |
| `note(content)` | Drop a journal entry without emitting |

---

## Working layer

Workers are where heavy work happens. The router decides "this needs a worker" and spawns one.

- Each worker is one ACP session, one concern, one lifetime measured in seconds to hours.
- Workers carry the originating peer's identity. Their emissions auto-stamp `X-HI-To` to that peer.
- Workers receive `claude-code`'s default capability surface (file access, code execution, MCP, browsers, etc.) plus the `speak` tool to emit on channels. They do *not* get `spawn_worker` — only routers can spawn.
- Workers can request approval via ACP `session/request_permission`; the reactor bridges to `/approval`.
- Workers may emit on multiple channels (e.g., text on `/thought`, audio later).
- Multiple workers run in parallel. They never block each other or the routing layer.

---

## Memory

Two on-disk files, both append-only JSONL, both in `data/`:

### `journal.jsonl`

Every signal in and out, every worker spawn/cancel, every approval request/decision, every intent fire. Lines are records like:

```json
{"ts":"2026-05-28T08:30:01.123Z","kind":"signal_in","channel":"thought","from":"alice@phone","body":"hey"}
{"ts":"2026-05-28T08:30:02.001Z","kind":"signal_out","channel":"thought","to":"alice@phone","body":"hi"}
{"ts":"2026-05-28T08:30:02.500Z","kind":"worker_spawn","id":"w_01H...","peer":"alice@phone","brief":"summarize..."}
```

`kind` discriminates; payload varies. Routers read a recent slice (last N entries or last T minutes) before each decision.

### `intents.jsonl`

Pending deferred intentions.

```json
{"id":"i_01H...","created":"2026-05-28T08:31:00Z","peer":"alice@phone","when":{"type":"absolute","ts":"2026-05-28T09:00:00Z"},"what":"remind alice the meeting starts"}
```

The heartbeat scans this file on each tick and fires due intents (see Aliveness).

### Read pattern

Memory is read by the reactor (to assemble a snapshot for each routing invocation) and written by the reactor (on every signal, before invoking the router) and by the in-process MCP tools (`set_intent`, `note`). Sessions themselves do not touch the files directly — they go through the MCP server.

Forgetting curve, significance scoring, and indexing are **deferred** for v0. The journal grows unbounded. This is acceptable for a sample.

---

## Approval

The spec's `/approval` long-poll maps onto ACP's `session/request_permission`:

- A worker (or router) calls `session/request_permission` over ACP with a structured request (id, action, summary, details).
- The reactor receives it, journals it, addresses it to the peer the originating session is acting for, and emits it on the `/approval` long-poll.
- The peer's client renders distinctly (modal, push notification).
- The peer's client `POST /approval` with the decision.
- The reactor matches `id` to the pending request and relays the decision back to the ACP session.
- The session resumes (or aborts) based on the decision.

Approval is **global** — it does not belong to a specific channel. Any session can request; any peer with an open `/approval` long-poll can be the decider, scoped by `X-HI-To`.

Timeouts on outstanding approvals are not specified by the spec; v0 uses 5 minutes. After that, the request is journaled as expired and the requesting session is told to abort.

---

## Aliveness — Heartbeat

The agent must act on its own. The mechanism:

- `tokio::time::interval` ticking at 1 Hz.
- On each tick, read `intents.jsonl` and find intents whose trigger condition is now met.
- For each due intent, **inject a synthetic signal into the target peer's routing path**: a journal entry of kind `signal_in` with `channel: "intent"` and `body: <what>`. The reactor picks it up like any other signal and invokes the peer's routing layer.
- The router sees `{channel: "intent", from: "self@..."}` plus the intent's `what` and decides how to phrase the unprompted emission.

Why synthetic signals rather than directly emitting? Because routing is the layer that decides *how* to phrase things and *whether* to act. A reminder for a meeting that's already happened should not be voiced. The router catches that.

Intent triggers v0 supports:
- `{type:"absolute", ts:"..."}` — fire at this UTC instant
- `{type:"cron", expr:"0 9 * * 1-5"}` — fire on cron schedule (deferred to v0.1 if cron parsing is annoying)
- `{type:"relative", from:"...", delta:"PT5M"}` — fire 5 minutes after a referenced event (deferred to v0.1)

For v0, only absolute is required.

---

## Aliveness — Cognition contract

Per spec:
- Inputs may arrive while the agent is mid-emission. **The reactor accepts them unconditionally.**
- The agent may emit on multiple output channels concurrently. **Channels are independent broadcasts.**
- Internal architecture is implementation choice, but the process **MUST NOT** block on "current request complete."

Interruption: when a new POST arrives for a peer while their queue is running a routing turn, the reactor evaluates an interruption policy. v0 policy: cancel the in-flight router (via `session/cancel`) and re-prompt with both signals merged. Workers are not auto-cancelled; the router may cancel them explicitly.

---

## Scope

### In scope for v0

- `GET /` — homepage (the Vite-built SPA, embedded)
- `POST /thought`, `GET /thought` — text channel, in and out
- `GET /approval`, `POST /approval` — approval bridge
- `POST /touch`, `POST /smell`, `POST /taste`, `POST /vision`, `POST /audio`, `GET /audio` — return `501 Not Implemented` with a descriptive body
- Reactor + per-peer queues
- ACP plumbing: spawn `claude-code`, manage one subprocess, create/close sessions
- Per-peer ephemeral routers
- Workers via ACP, parallel
- In-process MCP server with the router toolbelt
- `journal.jsonl` + `intents.jsonl`
- Heartbeat firing absolute-time intents
- `X-HI-From` recorded on every signal
- `Authorization: Bearer ...` accepted but not validated (token logged)
- Web appearance (SPA subscribes to `/thought` and renders)

### Deferred (not in v0)

- `/vision`, `/audio` actual implementations
- `/touch`, `/smell`, `/taste` actual implementations
- Multi-peer shared tasks (Alice + Bob in one workspace)
- Authorization token validation / issuance
- Cron and relative intent triggers
- Forgetting curve / significance scoring / journal compaction
- Handle resolution / discovery
- Federation, end-to-end encryption beyond TLS
- OS sleep/wake bridge for battery-constrained devices

---

## Project layout

```
hi-agent/
├── Cargo.toml
├── Cargo.lock
├── build.rs                              # cargo:rerun-if-changed for web/dist
├── .gitignore
├── .dockerignore
├── Dockerfile
├── justfile                              # build/dev/release recipes
├── README.md
├── LICENSE
├── rustfmt.toml
├── docs/
│   └── impl.md                           # this document
├── src/
│   ├── main.rs                           # parse args, build runtime, run forever
│   ├── lib.rs                            # re-exports for tests
│   ├── types.rs                          # Signal, PeerId, IntentId, WorkerId, etc.
│   ├── server/                           # HTTP front
│   │   ├── mod.rs                        #   axum router wiring
│   │   ├── thought.rs                    #   POST/GET /thought
│   │   ├── approval.rs                   #   /approval long-poll
│   │   ├── headers.rs                    #   X-HI-From / X-HI-To parsing
│   │   └── stubs.rs                      #   501 stubs for unimplemented channels
│   ├── reactor.rs                        # per-peer queues, worker registry, dispatch
│   ├── acp/                              # ACP back
│   │   ├── mod.rs                        #   client wrapper
│   │   ├── process.rs                    #   claude-code subprocess lifecycle
│   │   └── session.rs                    #   router / worker session helpers
│   ├── mcp.rs                            # in-process MCP server (router toolbelt)
│   ├── memory/                           # continuity substrate
│   │   ├── mod.rs                        #   facade
│   │   ├── journal.rs                    #   journal.jsonl
│   │   ├── intents.rs                    #   intents.jsonl
│   │   └── snapshot.rs                   #   builds router prompt input
│   ├── heartbeat.rs                      # tick loop, intent firing
│   └── appearance/                       # the appearance feature
│       ├── mod.rs                        #   axum handlers, OG meta, channel state stamp
│       ├── embed.rs                      #   rust-embed glue
│       ├── og.rs                         #   Open Graph tags
│       └── web/                          #   the SPA — one surface of appearance
│           ├── package.json
│           ├── pnpm-lock.yaml
│           ├── vite.config.ts
│           ├── tsconfig.json
│           ├── index.html
│           ├── public/
│           └── src/
│               ├── main.tsx
│               ├── App.tsx
│               ├── channels/             # /thought and /audio subscribers
│               └── ui/
├── tests/
│   ├── http_smoke.rs                     # POST + long-poll + body-close
│   ├── interruption.rs                   # new POST aborts in-flight routing
│   └── approval_flow.rs                  # /approval round-trip
├── scripts/
│   └── curl-recipes.sh                   # demo curls from README
└── data/                                 # gitignored at runtime
    ├── journal.jsonl
    └── intents.jsonl
```

### Conventions

- `appearance` is a Rust module; `web/` is one of its surfaces. Future surfaces (Tauri shell, OG-image generator, etc.) would sit alongside `web/` under the same module.
- `src/appearance/web/node_modules/` and `src/appearance/web/dist/` are gitignored.
- `data/` is gitignored. Created at runtime if missing.
- `build.rs` declares `cargo:rerun-if-changed=src/appearance/web/dist` so changes to the SPA build trigger a re-embed in the next `cargo build`.

---

## Build, dev, and deploy

### Makefile recipes

```
build:
	cd src/appearance/web && npm ci && npm run build
	cargo build --release

dev:
	trap 'kill 0' INT TERM EXIT; \
	cargo watch -x 'run -- --port 8080' & \
	(cd src/appearance/web && npm run dev) & \
	wait

run:
	./target/release/hi-agent

test:
	cargo test
	cd src/appearance/web && npm test

docker:
	docker build -t hi-agent:dev .
```

### Dev mode

Two processes, no embedding:
- Rust binary on `:8080` (channels only).
- Vite dev server on `:5173` with HMR; proxies channel routes to `:8080` via `vite.config.ts`.
- Browser only talks to `:5173`.

`make dev` backgrounds both with a `trap 'kill 0'` so Ctrl-C stops the group. Output is interleaved without prefixes; run them in separate terminals if you need clean streams.

### Release mode

`npm run build` produces `src/appearance/web/dist/`. `cargo build --release` embeds it via `rust-embed`. Single binary serves everything on one port.

### Docker

Multi-stage Dockerfile:

```dockerfile
# Stage 1: build the SPA
FROM node:22-alpine AS web
WORKDIR /web
COPY src/appearance/web/package.json src/appearance/web/pnpm-lock.yaml ./
RUN corepack enable && pnpm install --frozen-lockfile
COPY src/appearance/web ./
RUN pnpm build

# Stage 2: build the Rust binary (embeds SPA)
FROM rust:1-bookworm AS rust
WORKDIR /build
COPY Cargo.toml Cargo.lock build.rs ./
COPY src ./src
COPY --from=web /web/dist ./src/appearance/web/dist
RUN cargo build --release

# Stage 3: minimal runtime
FROM debian:bookworm-slim
RUN apt-get update && apt-get install -y --no-install-recommends ca-certificates \
    && rm -rf /var/lib/apt/lists/*
COPY --from=rust /build/target/release/hi-agent /usr/local/bin/hi-agent
EXPOSE 8080
ENTRYPOINT ["/usr/local/bin/hi-agent"]
```

**Open question — claude-code in the image.** The binary spawns `claude-code` as a subprocess; the runtime image must have it available. Two paths:

1. Install `claude-code` into the runtime image. Requires Node in the image (claude-code ships as an npm package today).
2. Make the ACP transport configurable: hi-agent talks ACP to a separate process — either spawned locally or reached over a socket. In Docker, run hi-agent and claude-code in separate containers connected by a Unix socket or TCP.

v0 will pick one before step 2 lands. Likely **(2)**, with a `docker-compose.yml` showing both containers.

---

## Implementation steps

Each step is independently runnable. Each is strictly more capable than the last.

### Step 0 — Spike on ACP concurrent sessions

Before committing to the architecture, verify in a small program: can one `claude-code` subprocess host N concurrent ACP sessions cleanly? Spawn 10 sessions, prompt each concurrently, observe behavior. Time-boxed to a half-day. If the answer is no, the working-layer design changes (concurrency cap, or one subprocess per worker).

**Output:** a `risks.md` note recording behavior observed, plus go/no-go.

### Step 1 — Skeleton

- Cargo project; lib + main.
- axum server on `:8080`.
- Routes: `GET /` (placeholder HTML), `POST /thought` (accepts, returns 202), `GET /thought` (long-poll on a shared broadcast — currently silent), `POST /vision/audio/touch/smell/taste/audio` → 501 with body, `GET /audio` → 501.
- Headers: parse `X-HI-From`, `X-HI-To`, `Authorization`; reject signals missing `X-HI-From`.
- Tracing logs to stdout.

**Verifies:** the spec's long-poll + body-close shape works end-to-end with `curl --no-buffer`.

### Step 2 — ACP plumbing

- Spawn `claude-code` as a child process over stdio.
- ACP `initialize` handshake.
- `session/new` followed by `session/prompt` against a fresh session; print streaming `session/update` events.
- `session/cancel` works.
- `session/close` (or equivalent teardown) works.

**Verifies:** the back end is reachable. No reactor wiring yet.

### Step 3 — Per-peer routing

- Reactor data structures: `HashMap<PeerId, PeerState>`, where `PeerState` holds an `mpsc` and a "is a router running?" flag.
- On `POST /thought`: journal the signal, push onto the peer's mpsc.
- A reactor task per peer pulls from the mpsc and runs one routing turn at a time.
- A routing turn = spawn an ACP session, prompt it with `{signal, recent_journal_for_peer}`, wait for completion (consume all `session/update` text into `GET /thought` broadcast).
- No tools yet — the router can only emit text directly via its prompt response.

**Verifies:** end-to-end echo with cognition. Signals from different peers are independent.

### Step 4 — In-process MCP server with router toolbelt

- Spawn an MCP server inside the same Rust process (Unix socket or stdio pair).
- Tools: `speak`, `spawn_worker`, `cancel_worker`, `list_workers`, `set_intent`, `recall`, `note`.
- When a routing session is created, attach this MCP server to it via ACP's MCP support.
- Router system prompt: "You dispatch. You don't perform tasks. Use these tools."

**Verifies:** the router can choose to call tools instead of speaking inline.

### Step 5 — Worker sessions

- `spawn_worker(brief, channel_out)` creates a new ACP session, prompts it with the brief, registers it in the worker registry tagged with the originating peer.
- Worker's `session/update` text streams into the channel's broadcast, stamped `X-HI-To = peer`.
- Workers run in parallel with each other and with routing turns.
- Worker session closes when prompt completes; worker is dropped from registry.
- `cancel_worker(id)` calls `session/cancel` and drops the worker.

**Verifies:** the working layer. Routers stay thin; workers do the work.

### Step 6 — Memory journal + per-peer state

- Move from in-memory recent-history to disk-backed `journal.jsonl`.
- `snapshot::build_for_peer(peer)` returns the data given to each routing prompt: recent journal entries (last N minutes), peer's running workers, peer's pending approvals, peer's pending intents.
- All writes to journal go through the reactor — no session writes the file directly.

**Verifies:** continuity across restarts. Memory is the source of truth, not session context.

### Step 7 — Approval bridge

- ACP `session/request_permission` from any session.
- Reactor receives it; journals it; emits it on the `/approval` GET broadcast addressed to the peer the requesting session is acting for.
- `POST /approval` parses the decision; reactor matches by id; relays back to ACP.
- Timeout handling: after 5 minutes with no decision, the request is journaled expired and the session is told to abort.

**Verifies:** structured permission flow works end-to-end.

### Step 8 — Heartbeat + intents

- `tokio::time::interval` at 1 Hz.
- `intents.jsonl` read on each tick; due absolute-time intents are fired.
- Firing = inject a synthetic `signal_in` for the target peer with `channel: "intent"`, body = the intent's `what`.
- Routing layer handles it like any other signal.
- After firing, the intent is journaled fired and removed from the active intents file.

**Verifies:** aliveness. The agent emits unprompted.

### Step 9 — Web appearance

- Set up `src/appearance/web/` with Vite + React + TypeScript.
- SPA subscribes to `GET /thought` via fetch streaming and renders incoming text.
- Minimal UI: an area showing recent text from the agent, an input to send to `POST /thought`.
- OG meta tags rendered by `src/appearance/og.rs` based on agent state at request time.
- `rust-embed` embeds `web/dist/` at build time.
- Dev mode: Vite proxies `/thought`, `/approval`, etc. to the Rust server.

**Verifies:** the appearance contract. The homepage is alive and reflects channel state.

### Step 10 — README + curl recipes

- README with one-line summary, quickstart (curl-based), architecture diagram.
- `scripts/curl-recipes.sh` covering: open long-poll, send a message, schedule a reminder, approve an action.
- Spec compliance table.

**Verifies:** an outsider can pick the project up.

---

## Open risks

| Risk | Why it matters | Mitigation |
|---|---|---|
| Concurrent ACP sessions in claude-code | Architecture assumes one subprocess hosts many sessions concurrently. ACP supports it; claude-code's behavior under load is unverified. | Step 0 spike. Fallback: serialize router invocations behind a small semaphore (concurrency cap), accept the reduced parallelism. |
| MCP server attachment per ephemeral session | Each routing session needs the toolbelt available; per-session attach cost is unknown. | Verify in same spike. Investigate session-template / shared MCP-server reuse if needed. |
| Journal-as-context coherence | Routers depend on memory snapshots being faithful. A stale or partial snapshot would break short-term continuity. | Reactor writes to journal *before* spawning the routing session. Snapshot read is consistent by construction. |
| Interruption semantics on ACP | Spec wants a new POST to abort an in-flight emission. ACP cancellation needs to propagate cleanly. | Encode as reactor policy: cancel via `session/cancel`, log if cancellation is slow. |
| Single-binary deployment with claude-code dependency | The Rust binary is self-contained, but the runtime needs `claude-code` installed. | Document the dependency. Docker uses a separate container for claude-code, connected over a socket. |
| Web embedding rebuild | rust-embed does not auto-detect `web/dist/` changes. | `build.rs` declares `cargo:rerun-if-changed=src/appearance/web/dist`. |

---

## Out of scope for this document

- LLM prompt design (system prompts for routers, worker briefs, etc.) — captured in `docs/prompts.md` once Step 4 lands.
- Operational concerns (logs shipping, metrics, alerting) — not relevant for a sample.
- Multi-tenancy beyond per-peer routing — multi-peer shared workspaces are explicitly deferred.
- Security model beyond v0 — Authorization recorded but not validated; out of scope.

---

## References

- [human-interface spec v0.7](../../human-interface/docs/human-interface.md)
- [Agent Client Protocol](https://agentclientprotocol.com)
- [axum docs](https://docs.rs/axum)
- [rust-embed](https://docs.rs/rust-embed)
