# hi-agent

## Running locally

Use `make dev` — it runs both halves together (Ctrl-C stops both):
- **Rust backend** on `:8080` via `cargo watch -x 'run -- --port 8080'` (auto-rebuilds/restarts on Rust changes).
- **Vite dev server** on `:5173` (`npm run dev` in `src/appearance/web`) — this is the page you open in dev.

In dev the browser talks only to Vite (`:5173`), which proxies `/api/*` and `/generated/*` to the backend. Caveat: `cargo watch` restarts the backend on Rust edits, but **Vite config changes (`vite.config.ts`) are NOT hot-reloaded** — restart `make dev` (or just the Vite process) after editing it.

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
