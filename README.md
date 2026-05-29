# hi-agent

A reference implementation of the [human-interface](../human-interface/docs/human-interface.md) spec — a small Rust agent that talks over HTTP channels, delegates cognition to a Claude Code runtime (installed on first run) over ACP, and persists everything to JSONL.

## Status

design v0.1 · 2026-05-28 · v0 implementation complete · not load-tested.

## Quickstart

### Prerequisites

- Rust toolchain (2024 edition — `rustc` 1.85 or newer)
- A running binary needs no separate runtime preinstalled — on first run hi-agent
  downloads the pinned Node and `npm ci`s the ACP adapter + `claude` CLI into an
  OS cache dir, then reuses that install on every subsequent start. First run
  therefore needs network access and the system `tar`; later runs are offline.
- macOS and Linux on x86_64/aarch64 are supported for auto-install. To use your
  own runtime instead (or to develop offline), set `HI_AGENT_DEV_NODE` /
  `HI_AGENT_DEV_ADAPTER` / `HI_AGENT_DEV_CLAUDE`.

### Build and run

```sh
# 1. build the SPA so rust-embed has something to embed
cd src/appearance/web && npm ci && npm run build && cd ../../..

# 2. build the Rust binary
cargo build --release

# 3. start the agent (creates ./data on first run)
./target/release/hi-agent
```

Or, with `make`:

```sh
make build && make run
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

One Rust process per agent. Inside it: an axum HTTP server, a reactor that owns per-peer queues and a worker registry, a memory facade backed by two JSONL files, an in-process MCP hub the router/worker sessions reach over a Unix socket, and a heartbeat that injects synthetic signals when intents come due. Cognition is delegated: on first run hi-agent installs its runtime (downloading the pinned Node and `npm ci`-ing the ACP adapter + `claude` CLI into an OS cache dir), then on every start spawns the ACP adapter (via that `node`) and creates one fresh ACP session per routing turn (and one per long-lived worker). The adapter talks to a local Anthropic-compatible proxy that injects the real upstream credential, so the key never lands in any on-disk adapter config.

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
| `POST /audio`, `GET /audio` | Y when configured | STT transcribes the body and routes the text; the router may reply via `speak(channel="audio")` which is synthesized back through TTS and broadcast on the long-poll. 501 on POST when `STT_PROVIDER` is unset. |
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
| `AI_API_KEY` | — | Upstream LLM credential, injected by the local proxy. Required; read from env only, never written to disk. |
| `AI_API_BASE` | `https://api.anthropic.com` | Upstream LLM base URL the proxy forwards to. |
| `HI_AGENT_CONFIG` | `config.toml` | Path to the dev-managed config file (model / effort / permission mode) |
| `HI_AGENT_RUNTIME_DIR` | OS cache dir | Override the dir the runtime is installed into |
| `HI_AGENT_MCP_SOCK` | `<data_dir>/mcp.sock` | Unix socket the MCP hub listens on |
| `HI_AGENT_SHIM_BIN` | `current_exe()` | Program to re-exec as the MCP stdio↔socket shim |
| `RUST_LOG` | `info` | Standard `tracing-subscriber` env filter |

Managed cognition parameters (model, effort, permission mode) live in
[`config.toml`](config.toml), not in env vars; the upstream credential and base
URL come from `AI_API_KEY` / `AI_API_BASE`. The dev-only `HI_AGENT_DEV_NODE` /
`HI_AGENT_DEV_ADAPTER` /
`HI_AGENT_DEV_CLAUDE` overrides let you point at an external runtime when
developing offline or skipping the first-run download (debug use only).

### Runtime install & versioning

The Node and ACP adapter versions are pinned in
[`runtime/manifest.toml`](runtime/manifest.toml) (which also records the
per-target Node download URLs + checksums for reference); the adapter +
`claude` CLI dependency tree is pinned by the committed
[`runtime/package.json`](runtime/package.json) /
[`runtime/package-lock.json`](runtime/package-lock.json). On first run hi-agent
downloads the pinned Node release from nodejs.org (extracted with the system
`tar`) and runs `npm ci --omit=dev` against the committed lockfile into an OS
cache dir, marks the install complete, and reuses it on every later start.
`build.rs` stamps the pinned versions into the binary; `hi-agent --version`
reports the crate version alongside the runtime component versions (bundle id,
node, adapter, claude).

### Voice (optional, additive)

Speech-to-text and text-to-speech are independent capabilities. Each is off by
default; enabling either is a one-provider switch. Both happen to use
Volcengine in this release; swapping either is a single file under
`src/voice/`.

| Variable | Default | Purpose |
|---|---|---|
| `STT_PROVIDER` | `none` | `none` → `POST /audio` returns 501. `volcengine` → enable transcription. |
| `TTS_PROVIDER` | `none` | `none` → `speak(channel="audio")` returns an error string (the agent retries with text). `volcengine` → enable synthesis. |
| `VOLCENGINE_STT_APPID`, `VOLCENGINE_STT_ACCESS_TOKEN` | — | Required when `STT_PROVIDER=volcengine` |
| `VOLCENGINE_STT_CLUSTER`, `VOLCENGINE_STT_MODEL` | sensible defaults | Optional STT tuning |
| `VOLCENGINE_TTS_APPID`, `VOLCENGINE_TTS_ACCESS_TOKEN` | — | Required when `TTS_PROVIDER=volcengine` |
| `VOLCENGINE_TTS_CLUSTER`, `VOLCENGINE_TTS_VOICE`, `VOLCENGINE_TTS_ENCODING` | sensible defaults | Optional TTS tuning |

STT and TTS having separate credentials is deliberate — each capability is
self-contained, so one can be moved to a different provider without touching
the other.

CLI flags:

| Flag | Default | Purpose |
|---|---|---|
| `--port` | `8080` | HTTP port to bind |
| `--data-dir` | `./data` | Where `journal.jsonl` / `intents.jsonl` / `mcp.sock` live |

## Project layout

```
hi-agent/
├── Cargo.toml                              # crate + dev-dependencies
├── build.rs                                # embeds the SPA, stamps runtime versions
├── Dockerfile                              # multi-stage build (SPA → rust → debian-slim)
├── docker-compose.yml                      # compose layout (illustrative)
├── Makefile                                # build / dev / run / test / docker
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
│   ├── acp/                                # ACP adapter subprocess + per-session helpers
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
make dev
```

(That backgrounds `cargo watch` and `npm run dev` with a `trap` so Ctrl-C stops both. Output from the two processes is interleaved without prefixes — if that bothers you, run them in separate terminals.)

The browser talks to `:5173`. HMR works for the SPA; Rust reloads on file change via `cargo watch`.

## Docker

```sh
docker build -t hi-agent:dev .
```

On first run the binary installs its own runtime (downloads the pinned Node and
`npm ci`s the ACP adapter + `claude` CLI into a cache dir), so the image needs
no separate claude-code container. First run therefore needs network access and
the system `tar`. The image still needs `AI_API_KEY` supplied at
runtime for cognition to work.

## Risks and known unverified things

See [`docs/risks.md`](docs/risks.md). The headline item: concurrent ACP sessions in the Claude Code runtime have not been measured under load. Run `cargo run --example acp_spike` after first build to validate the concurrency assumption before trusting the architecture in production.

## License

MIT. See [`LICENSE`](LICENSE).
