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

## UI architecture: headless engine + web face + native shell

**Decision (2026-07-07): the long-term target is a headless Rust *engine* supervised by a per-platform native *shell* that owns the process. The shell (SwiftUI/AppKit on macOS, XAML/WinUI on Windows, GTK on Linux) owns `main`, the run loop, and everything that touches the OS session; the Rust engine is pure cross-platform cognition + state and touches no platform GUI/OS APIs.** We accept a per-platform native cost for best-in-class native UX and a genuinely headless core.

This is not a new mode: the headless engine is *exactly the shape the app already compiles to on Linux/Docker* (server owns the thread, macOS crates `cfg`-gated). The refactor makes that the shape everywhere and deletes the macOS main-thread inversion (`run_with_tray`) from Rust, re-homing it in the shell.

### The three parts, by what each *is*

1. **Headless engine (Rust).** All state + logic: config, credentials/mode, energy, memory, and *all cognition* — vision model calls, STT/diarization, the reflex recognizer, and the biometric pipeline (face `buffalo_l`, voiceprint `CAM++`, clustering, `name_person`/`merge_people`). **Pure Rust: no objc2, no Apple frameworks.** ("Pure" = no platform-GUI code; it still links portable native deps — ONNX Runtime, ffmpeg — and spawns the node/claude ACP runtime. Those build the same on every OS.) Runs **out-of-process as a sidecar** the shell spawns and supervises.
2. **Web face (webview in the shell).** The main content-heavy, fast-moving UI. Talks to the engine over the local API. Write-once cross-platform. (Precedent: the popover face is a `WKWebView`; native and web chat were both tried and rejected in its favor.)
3. **Native shell (per platform).** Owns the process and everything needing the OS session, in two roles:
   - **App-shell primitives** — run loop, tray, global hotkey tap, native windows, popover. Move to the shell.
   - **Native-presentational surfaces** — Settings and future preference windows, built in the platform's native UI toolkit (SwiftUI first) as **clients of the engine's local API** — not in-process C-ABI FFI.

### Mechanism vs policy — the rule that keeps the engine pure

Every OS-integration *capability* splits: the raw OS touch (**mechanism**) lives in the shell; the cross-platform brain (**policy**) stays in the engine and calls the mechanism over the API. Platform-specific code was always going to be written per-OS — the only question is *which process*, and the answer is "the one holding the session + grants" = the shell.

| Capability | Mechanism → **shell** | Policy → **engine** |
|---|---|---|
| Vision | grab frames | Doubao vision call, when-to-see |
| Screen-control / reflex | screen pixels, post keystroke, read AX tree | reflex recognizer, fire policy |
| Face / voice ID | camera / mic bytes | `buffalo_l` / `CAM++` ONNX, clustering, recognition |
| desktop_context | focused app / window query | how context feeds cognition |

The biometric/ML layer is **already correctly engine-resident and cross-platform** — it does not move. Camera/mic bytes for it may even arrive via the **browser web face** (`getUserMedia` → POST), so that capture is cross-platform too. Only capabilities needing the **window-server** (screen capture, input synthesis, AX, desktop_context) *must* live in the shell.

### The engine's new interface

The engine's outbound API grows from config CRUD into a **bidirectional perceive/act protocol**, part of it **streaming** (frames, audio are continuous): shell→engine carries user input, config, and capability *results*; engine→shell carries perceive/act *requests*. This streaming perception surface (persistent channel — WebSocket / gRPC-stream / IPC) is the **biggest new design object** in the refactor — bigger than the Settings migration. Name and design it as its own thing.

### Permission model (macOS; analogous elsewhere)

- **Engine = POSIX-only, no TCC.** Runs as the same UID as the shell, so it inherits plain file access (its data dir, user-chosen paths) for free. It requires *nothing* TCC-gated — the split is load-bearing, TCC inheritance is not.
- **Shell holds all TCC grants** (Screen Recording, Accessibility, Camera, Microphone, protected folders) and brokers them over the API.
- **Bundle + co-sign the engine inside the `.app`** (same pattern already used for node/claude/ffmpeg; mandatory for Developer-ID notarization anyway). Spawn it by **bundle-relative path** (not the OS-cache auto-install path the runtime uses) so it launches under the app's responsible-process — free TCC inheritance *if ever needed*, as a safety margin, not a dependency.
- **Mic capture → shell** (resolves the one open item): keeps the engine 100% TCC-free rather than dragging a Microphone grant into it. `cpal`-in-engine was the only capability that could have stayed; the permission story tips it to the shell.

### Sequencing — two phases, don't flip ownership first

- **Phase 1 — Settings in hosted SwiftUI, Rust still owns the process.** Host a SwiftUI Settings window (via `NSHostingView` in a Rust-created window) talking to the loopback config/energy/mode API. Needs no OS grants, touches none of the hard-won tray/hotkey/capture code — proves the core↔UI API boundary at near-zero risk. **Define that config/energy/mode API boundary cleanly first, then build the client.**
- **Phase 2 — flip ownership.** Swift owns `NSApplication`; Rust demoted to sidecar; port app-shell primitives + capability mechanisms to Swift; stand up the streaming perceive/act API. This is the big, GUI-wall-bound phase — do it last.

**Boundary rule going forward:** a capability's *mechanism* (OS touch) belongs in the shell; its *policy* (cross-platform logic) belongs in the engine. A new surface is API-client-native only if it's presentational. When unsure which bucket something falls in, that's a consequential fork — ask.

**Status:** direction agreed, not yet built. First move is Phase 1 against [vendors/macos_settings.rs](src/vendors/macos_settings.rs) (today pure objc2, hand-laid frames).

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
