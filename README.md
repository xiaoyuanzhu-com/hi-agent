# hi-agent

A reference implementation of the [human-interface](../human-interface/docs/human-interface.md) spec — a small Rust agent that talks over HTTP channels, delegates cognition to `claude-code` over ACP, and persists everything to JSONL.

## Status

design v0.1 · 2026-05-28 · v0 implementation complete · not load-tested.

## Quickstart

### Prerequisites

- Rust toolchain (2024 edition — `rustc` 1.85 or newer)
- Node 22+ and `pnpm` (via `corepack enable`)
- `claude-code` available on `PATH`, or set `CLAUDE_CODE_BIN`

### Build and run

```sh
# 1. build the SPA so rust-embed has something to embed
cd src/appearance/web && pnpm install && pnpm build && cd ../../..

# 2. build the Rust binary
cargo build --release

# 3. start the agent (creates ./data on first run)
./target/release/hi-agent
```

Or, with `just`:

```sh
just build && just run
```

### Verify it's alive

```sh
curl -X POST http://127.0.0.1:8080/thought \
  -H 'X-HI-From: alice@phone' \
  --data-binary 'hello'
```

You should see `202 Accepted` and a fresh line in `data/journal.jsonl`. To watch the agent talk back, open a long-poll in another terminal first:

```sh
curl -N -H 'X-HI-To: alice@phone' http://127.0.0.1:8080/thought
```

## Curl recipes

The full set lives in [`scripts/curl-recipes.sh`](scripts/curl-recipes.sh). The most useful four:

```sh
# Open a long-poll on /thought as alice@phone (Ctrl-C to close)
curl -N -H 'X-HI-To: alice@phone' http://127.0.0.1:8080/thought

# Send a thought
curl -X POST -H 'X-HI-From: alice@phone' \
  --data-binary 'hey, are you there?' \
  http://127.0.0.1:8080/thought

# Schedule a reminder (the router decides whether to call set_intent)
curl -X POST -H 'X-HI-From: alice@phone' \
  --data-binary 'remind me at 21:00 to call mom' \
  http://127.0.0.1:8080/thought

# Approve a pending action (id comes from the /approval long-poll JSON)
curl -X POST -H 'X-HI-From: alice@phone' \
  -H 'Content-Type: application/json' \
  -d '{"id":"<approval-uuid>","allow":true}' \
  http://127.0.0.1:8080/approval
```

## Architecture

One Rust process per agent. Inside it: an axum HTTP server, a reactor that owns per-peer queues and a worker registry, a memory facade backed by two JSONL files, an in-process MCP hub the router/worker sessions reach over a Unix socket, and a heartbeat that injects synthetic signals when intents come due. Cognition is delegated: hi-agent spawns one `claude-code` subprocess at startup and creates one fresh ACP session per routing turn (and one per long-lived worker).

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

See [`docs/impl.md`](docs/impl.md) for the full architecture document.

## Spec compliance (v0)

| Spec requirement | Status | Notes |
|---|---|---|
| `GET /` homepage | Y | Embedded Vite SPA, OG meta injected at request time |
| `POST /thought` | Y | Body bytes are the signal; close-of-body ends the utterance; `X-HI-From` required (400 otherwise) |
| `GET /thought` long-poll | Y | Filters by `X-HI-To`; broadcast fan-out from the reactor |
| `POST /approval` | Y | JSON `{id, allow, reason?}`; reactor relays decision into ACP `session/request_permission` |
| `GET /approval` long-poll | Y | JSON event; 5-minute timeout on the requesting side |
| `POST /vision` | 501 | Per v0 scope; body describes the omission |
| `POST /audio`, `GET /audio` | 501 | Per v0 scope |
| `POST /touch`, `POST /smell`, `POST /taste` | 501 | Per v0 scope |
| Per-peer ephemeral routing | Y | One ACP session per routing turn, scoped by `X-HI-From` |
| Workers (parallel ACP sessions) | Y | `spawn_worker` MCP tool; one session per worker; auto-stamp `X-HI-To` |
| Memory: `journal.jsonl` + `intents.jsonl` | Y | Append-only journal; intents file rewritten atomically on add/remove |
| Heartbeat (1 Hz, absolute intents) | Y | Synthetic `signal_in` on `channel: intent`, injected via the reactor |
| `X-HI-From` recorded | Y | Required on every inbound; journaled before dispatch |
| `Authorization: Bearer ...` | accepted/logged | Parsed and logged; not validated in v0 |
| Cron / relative intents | deferred | Per `docs/impl.md` Scope |
| Forgetting curve / significance / compaction | deferred | Per `docs/impl.md` Scope |
| Federation / e2e encryption / handle discovery | deferred | Per `docs/impl.md` Scope |

## Configuration

Env vars consulted at startup:

| Variable | Default | Purpose |
|---|---|---|
| `CLAUDE_CODE_BIN` | `claude-code` | Program to spawn for the ACP subprocess |
| `CLAUDE_CODE_ARGS` | (empty) | Whitespace-split args appended to the ACP launch |
| `HI_AGENT_MCP_SOCK` | `<data_dir>/mcp.sock` | Unix socket the MCP hub listens on |
| `HI_AGENT_SHIM_BIN` | `current_exe()` | Program to re-exec as the MCP stdio↔socket shim |
| `RUST_LOG` | `info` | Standard `tracing-subscriber` env filter |

CLI flags:

| Flag | Default | Purpose |
|---|---|---|
| `--port` | `8080` | HTTP port to bind |
| `--data-dir` | `./data` | Where `journal.jsonl` / `intents.jsonl` / `mcp.sock` live |

## Project layout

```
hi-agent/
├── Cargo.toml                              # crate + dev-dependencies
├── build.rs                                # rerun-if-changed for the SPA
├── Dockerfile                              # multi-stage build (SPA → rust → debian-slim)
├── docker-compose.yml                      # sibling-container layout for claude-code (illustrative)
├── justfile                                # build / dev / run / test / docker
├── Procfile.dev                            # `cargo watch` + Vite dev server
├── docs/
│   ├── impl.md                             # architecture and step plan
│   └── risks.md                            # unverified-things register (Step 0 spike output)
├── examples/
│   └── acp_spike.rs                        # concurrency probe (run before trusting the architecture)
├── scripts/
│   └── curl-recipes.sh                     # demo curls for every channel
├── src/
│   ├── main.rs                             # CLI; re-exec branch for the MCP shim
│   ├── lib.rs                              # `run(Config)` — wires everything
│   ├── types.rs                            # PeerId, Channel, Signal, JournalEntry, Intent
│   ├── server/                             # axum router + extractors + handlers
│   ├── reactor.rs                          # per-peer queues, worker registry, interruption
│   ├── acp/                                # claude-code subprocess + per-session helpers
│   ├── mcp.rs                              # in-process MCP hub + the seven tools
│   ├── memory/                             # journal, intents, snapshot builder
│   ├── heartbeat.rs                        # 1 Hz tick; absolute-intent firing
│   └── appearance/                         # web surface (Rust handlers + embedded Vite SPA)
└── tests/
    ├── http_smoke.rs                       # route surface + header rejection + journaling
    ├── interruption.rs                     # #[ignore] — needs claude-code, see body
    └── approval_flow.rs                    # #[ignore] — needs claude-code, see body
```

## Development

Two processes — the Rust binary on `:8080` and the Vite dev server on `:5173`, with Vite proxying channel routes to `:8080`:

```sh
just dev
```

(That runs `overmind start -f Procfile.dev`. If you don't use overmind, run the two lines from `Procfile.dev` in separate terminals.)

The browser talks to `:5173`. HMR works for the SPA; Rust reloads on file change via `cargo watch`.

## Docker

```sh
docker build -t hi-agent:dev .
```

The image is self-contained for the Rust binary but **does not include `claude-code`**. The v0 strategy is a sibling container reached over a Unix socket; see [`docker-compose.yml`](docker-compose.yml) for the layout. The exact `claude-code` image and command are not pinned in this repo — treat the compose file as illustrative and adjust to whatever `claude-code` distribution you have access to.

## Risks and known unverified things

See [`docs/risks.md`](docs/risks.md). The headline items: concurrent ACP sessions in `claude-code` have not been measured under load, and the Docker sibling-container story is illustrative rather than tested. Run `cargo run --example acp_spike` after first build to validate the concurrency assumption before trusting the architecture in production.

## License

MIT. See [`LICENSE`](LICENSE).
