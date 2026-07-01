# hi-agent

## Decide and proceed; don't gate on low-value questions

Bias to action. Make the engineering calls you can make yourself and start building in the same turn the approach is agreed — don't stack up confirmation questions or re-ask "go?". Just decide: sensible defaults, anything already implied by stated preferences or memory, and choices that are cheap to reverse. Reserve questions for forks that are genuinely consequential, hard to undo, or a matter of the user's preference — and batch those into a single ask. Never invent an option the user wouldn't want and then ask them to rule it out.

## Making changes: always in a worktree

Do all work for a task in its own fresh git worktree branched from `origin/main` — never edit the primary checkout directly. When the work is done and the user gives the go: commit, fetch + rebase, then push `<branch>:main`. Once the push lands, delete the worktree and its branch — never keep one around.

    git fetch origin
    # create a worktree off origin/main; make all changes there

    # --- when ready (after the user's "go") ---
    git fetch origin && git rebase origin/main
    git push origin <branch>:main

    # --- after the push lands: tear it down ---
    git worktree remove <path> && git branch -d <branch>

## Working alongside uncommitted changes

The working tree may hold the user's in-progress work that is unrelated to your task. Don't entangle with it: keep your changes in new files where possible, put additive config (e.g. a new Cargo dependency) in its own separate block rather than interleaved with theirs, and at commit time stage only the files/hunks your task owns — never `git add -A`. Leave their WIP untouched in the tree for them to commit.

## Running locally

Use `make dev` — it runs both halves together (Ctrl-C stops both):
- **Rust backend** on `:12358` via `cargo watch -x 'run -- --port 12358'` (auto-rebuilds/restarts on Rust changes).
- **Vite dev server** on `:12359` (`npm run dev` in `src/appearance/web`) — this is the page you open in dev.

In dev the browser talks only to Vite (`:12359`), which proxies `/api/*` and `/generated/*` to the backend. Caveat: `cargo watch` restarts the backend on Rust edits, but **Vite config changes (`vite.config.ts`) are NOT hot-reloaded** — restart `make dev` (or just the Vite process) after editing it.

Other targets: `make build` (npm ci + build SPA, then `cargo build --release`), `make run` (release binary), `make test` (cargo + vitest), `make docker`.

## Dev vs. prod serving (important)

The two environments serve the web app differently, and this asymmetry has bitten us before:
- **Prod**: `cargo build --release` bundles the built SPA (`src/appearance/web/dist/`) into the binary via `RustEmbed` ([src/appearance/embed.rs](src/appearance/embed.rs)). The Rust server serves `GET /`, `/assets/*`, and `/generated/*` all **same-origin**, and [index()](src/appearance/mod.rs) injects the import map into the HTML.
- **Dev**: Vite serves the page; the Rust `index()`/import-map injection does **not** run. Dev mirrors prod via the Vite proxy (`/generated`) + a serve-only import-map plugin ([vite.config.ts](src/appearance/web/vite.config.ts)). If a view 404s or its bare imports don't resolve in dev, suspect this seam first.

Agent views are NOT self-contained bundles: the compiled `.mjs` keeps bare imports (`react`, `@hi/ui`, `@hi/core`, `motion/react`) resolved via the page import map to the host's shared instances — required so host and view share one React instance (hooks/context cross the boundary). Do not bundle these deps into views. See the shims in [src/appearance/web/src/shared/](src/appearance/web/src/shared/).

## Deployment shapes (intended)

The app targets two install shapes:
1. **Docker on a server** — `make docker` builds the image; users run it server-side.
2. **Bundled desktop app** (e.g. macOS) — a packaged native install for the desktop. _Desktop bundling is not wired up in-repo yet (no Tauri/Electron)._

The managed runtime (Node + ACP adapter + claude + esbuild) auto-installs into the OS cache on first run, so a bundled app needs no separate runtime install. On a dev box with `node` + `claude-agent-acp` + `claude` on PATH, the **system runtime** is used instead (esbuild is then provisioned separately — see [runtime::ensure_view_esbuild](src/runtime/mod.rs)).

## macOS entry shape (tray vs. headless)

On macOS the binary's default shape is a **desktop app**: AppKit owns the main thread and shows a menu-bar status item (Open / Quit), while the HTTP server + reactor run on a background thread ([run_with_tray](src/lib.rs); status item in [vendors/macos_tray.rs](src/vendors/macos_tray.rs)). Everywhere else (Linux/Docker) tokio keeps the main thread as before. Still one binary — this is the main-thread inversion the distribution model accepted as the cost of a tray; no shell crate, no Tauri.

The tray **auto-skips when `SSH_CONNECTION` is set** (no window server over SSH) or with `--no-tray`, falling back to the server-owns-main-thread path. So the SSH journey-testing command below is unchanged. The visible icon can only be tested from a real desktop session (same GUI-session wall as screencast/hotkey); over SSH you can verify compile, tests, and that startup logs `tray skipped (headless)` and still binds.

## Testing user journeys live (Mac mini)

Journeys in [docs/user-journeys/](docs/user-journeys/) are specs of *intended* behavior — test them against a real running instance, not by code-reading. Standing setup: clone at `~/projects/hi-agent` on the Mac mini (`ssh macmini`), `cargo build --release`, run from the repo root. Model credentials are no longer in `.env`: the default `xiaoyuanzhu` mode auto-bootstraps a broker account and mints the LLM key OOTB, so a fresh box just works; to force BYOK keys (or tune agent behaviour) headlessly, write into the config store (`sqlite3 data/config.db` — the `app_settings` KV holds the mode flag + cognition tunables; `credential` rows hold vendor keys) or set them in Settings. The `.env` now carries only infra knobs (auth, dirs, `RUST_LOG`, etc.):

    nohup ./target/release/hi-agent --port 12358 > server.log 2>&1 &

Talk to it over the text channel — Claude plays the boss; the human is only pulled in for account-side steps (QR/device auth, credentials) and for observing effects in external apps (e.g. what actually landed in the Feishu group):

    curl -X POST -H "X-HI-Scene: boss" --data-binary "..." localhost:12358/api/in/text
    curl -H "X-HI-Scene: boss" localhost:12358/api/out/text   # long-poll; one utterance per GET

Method — the parts that keep the test honest:

- **Don't lead the witness.** Speak like a terse, normal boss; never script journey-expected behaviors into the prompt. Test recovery by *creating the situation* (kill its processes, restart the host, plant a failure) and watching — not by mentioning it.
- **Trust but verify every claim.** Ground truth lives outside the conversation: `server.log`, `GET /api/sessions`, the scene transcripts (`data/claude-config/projects/*/<session>.jsonl` — `tool_use` entries show what it actually ran), and its workspace artifacts/ledgers.
- **Keep the harness out of the experiment.** A watcher whose own command line contains the probe string becomes a decoy (`pgrep -f "[f]oo"` avoids self-match); a long-poll `--max-time` that aborts mid-utterance triggers at-least-once redelivery on the next poll.
- To speed pulses up for a test session, set the `pulse` tunable in the config store (`sqlite3 data/config.db "INSERT INTO app_settings(key,value) VALUES('pulse','120') ON CONFLICT(key) DO UPDATE SET value='120'"`) or the Agent section of Settings; reset it afterwards (default 30m).
- Findings go back into the journey doc (实测缺口 / 复测 sections). When behavior and journey disagree, that's a bug in one or the other — resolve explicitly.
