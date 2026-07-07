# Core↔shell config API (Phase 1 of the UI-architecture refactor)

> Status: design note, not built. Scope: the **request/response config/energy/mode boundary** only — the first, low-risk slice of the core↔shell split described in [CLAUDE.md § "UI architecture: headless engine + web face + native shell"](../CLAUDE.md). The **streaming perceive/act protocol** (frames, audio, input, AX) is Phase 2 and is *not* covered here. Keeping the two apart is deliberate: this slice is plain JSON over the existing local HTTP server, needs no OS grants, and unblocks the SwiftUI Settings migration.

## Goal

Give the SwiftUI Settings window (and the web face) **one local HTTP API** to read and write every setting the current native Settings window touches, so the UI becomes a thin client of the engine and the engine stays the sole authority over `config.db`. This is the boundary the Phase-1 Settings migration is built against — define it cleanly first, then build the client.

The engine already is the authority; today the *native window mutates the store in-process*. The tray refactor removed the old HTTP config routes on purpose (`server/mod.rs:339-341`). Phase 1 reintroduces them — but as a proper, gated, secret-safe surface that both front-ends share (the web face's `OutOfEnergyHint` already consumes `/api/account/energy` + `/api/account/subscribe`; this generalizes that to all of Settings).

## Decisions

1. **Transport: plain JSON/REST over the existing local server** (`server/mod.rs build()`), handler style `async fn(State<Arc<AppState>>, …) -> impl IntoResponse` returning `Json`. No new channel, no streaming — config is request/response. The streaming protocol is a separate Phase-2 object.
2. **Loopback-gated, per-handler.** The server binds `0.0.0.0` (LAN-reachable) and has **no global auth gate** — every route is public today. These endpoints read/write **credentials and account state**, so each one must reject non-loopback peers, exactly like `account::get_link_callback` does (`ConnectInfo<SocketAddr>` + `peer.ip().is_loopback()`, `account.rs:98,103`). Factor that check into one extractor/helper and apply it to the whole config group. (Open Q below: whether to also require a shell-held token.)
3. **Secrets never leave the engine.** The read surface returns `configured: bool` (+ non-secret `base_url`, `model`) for each feature — **never the `api_key`**. This matches what the current UI shows ("configured / not set") and holds even over loopback. Writes accept a key; a blank/omitted key **keeps the existing** one (current `write_fields` semantics, `macos_account.rs:151-153`).
4. **One snapshot GET + granular writes.** `GET /api/settings` returns everything the window needs in one call (mirrors the window's `present()` re-sync). Writes are small, targeted PUTs so a single control change is one request.
5. **Explicit apply semantics in the contract.** Every setting declares whether it applies `live` or on `restart`, so the UI can render "takes effect on restart" truthfully instead of guessing. (Theme = live; language, gestures = restart — per `macos_settings.rs`.)
6. **Core persists, shell applies OS effects.** The API's job for theme is to *persist the value and return*; applying `NSAppearance` is a **shell** action (it owns the app). The engine no longer calls `apply_app_theme`. During Phase 1 (Rust still owns the process) the in-process apply can remain as a temporary bridge, but the contract is "core stores, shell applies" so Phase 2 needs no reshaping.

## Endpoints

Loopback-gated unless noted. `⟳reuse` = already exists, keep as-is.

| Method & path | Purpose | Request | Response |
|---|---|---|---|
| `GET /api/settings` | Full snapshot to populate the window | — | `Settings` (below) |
| `PUT /api/settings/appearance` | Theme / language / gestures | `{ theme?, language?, gestures? }` | `AppearanceState` (echo + `applies`) |
| `PUT /api/settings/mode` | Select active credential mode | `{ mode: "byok" \| "xiaoyuanzhu" }` | `{ mode }` |
| `PUT /api/settings/credentials/{feature}` | Set BYOK key/base_url/model | `{ api_key?, base_url?, model? }` (blank key keeps) | `FeatureStatus` (no secret) |
| `POST /api/account/energy/refresh` | Force a broker energy poll | — | `EnergySnapshot` |
| `GET /api/account/energy` | Out-of-energy hint (subset) | — | ⟳reuse `{ out_of_energy, tier, resets_at, resets_in }` |
| `GET /api/account/subscribe` | Mint signed-in subscribe URL | — | ⟳reuse `{ url, signed_in }` |
| `GET /account/link/start` | Begin device sign-in (browser) | — | ⟳reuse (302 to broker) |

`{feature}` ∈ `llm · stt · tts · vision · image · video`.

### Payload shapes

```jsonc
// GET /api/settings
{
  "appearance": {
    "theme":    { "value": "system", "options": ["system","light","dark"],        "applies": "live" },
    "language": { "value": "system", "options": ["system","en","zh-Hans"],          "applies": "restart" },
    "gestures": { "value": true,                                                     "applies": "restart" }
  },
  "account": {
    "mode": "xiaoyuanzhu",                       // active credential mode
    "identity": { "signed_in": true, "name": "…", "email": "…" },   // null fields if not signed in
    "energy": { "tier": "free", "remaining": 1234, "total": 5000,
                "resets_at": "2026-07-08T00:00:00Z", "out_of_energy": false },
    "features": [                                // BYOK status, secret-free
      { "feature": "llm",   "configured": true,  "base_url": "…", "model": "…" },
      { "feature": "stt",   "configured": false, "base_url": null, "model": null }
      // … tts, vision, image, video
    ]
  },
  "about": { "version": "…", "website": "https://hi.xiaoyuanzhu.com" }
}
```

`applies` is `"live" | "restart"`. `energy` is `null` in BYOK mode / when no snapshot exists.

## Security

- **Loopback gate is mandatory**, not optional polish: without it any device on the LAN could read `configured` flags, flip the account mode, or write BYOK keys. Mirror the existing `is_loopback()` check; put it in one place.
- **No secret egress:** the `api_key` is write-only. Confirm the snapshot serializer can't accidentally include it (the engine's `Credentials` struct holds the key inline — the DTO must be a distinct, projected type, not `#[derive(Serialize)]` on `Credentials`).
- **Shell-held token (open):** loopback also admits other local apps/users on a shared machine. If that matters, add a random per-launch token the shell learns at spawn (env/arg) and sends as a header. Decide in the open-questions pass; loopback-only is the floor.

## Mapping to existing code (implementation is mostly mechanical)

- Appearance reads/writes → `credentials::get_setting` / `set_setting` on `KEY_THEME`/`KEY_LANGUAGE`/`KEY_GESTURES`; option lists from `config::THEMES`/`LANGUAGES`; gestures via `config::flag_on`.
- Mode → read `Credentials::load(data_dir).mode`; write = set `.mode` + `.save(data_dir)`.
- Feature status/write → project `Credentials` fields to `FeatureStatus` via the per-feature `key_opt()`/`feature_key_set` predicate (`credentials.rs:200`, `macos_settings.rs:460`); write reuses `macos_account::write_fields` semantics (blank key keeps).
- Energy → snapshot from `Credentials.energy`; refresh calls `broker::poll_energy_now(data_dir)`; hint endpoint already reads `energy_state::is_out()`.
- Subscribe / sign-in → `broker::subscribe_url(data_dir, Some("/account"))`; link start already implemented.

New handlers live alongside `server/account.rs` (e.g. `server/settings.rs`), registered in `server/mod.rs` next to the existing account routes.

## What this note deliberately excludes

- **The streaming perceive/act protocol** (frames, audio, input synthesis, AX reads, desktop_context) — Phase 2, a persistent bidirectional channel, its own design object.
- **Cross-client live sync** — if the web face changes the theme, the native window won't hear about it without a push. Phase 1 has each client apply its own writes; a change-signal (SSE/WS on the config surface) is a later nicety, noted not built.
- **The ownership flip** — Phase 1 keeps Rust owning the process and hosts SwiftUI via `NSHostingView`; the API is identical before and after the flip, which is the point.

## Open questions

1. **Shell token on top of loopback?** (multi-user/shared-machine threat) — default to loopback-only unless there's a reason.
2. **Refresh as `POST …/refresh` vs `GET …/energy?refresh=1`?** — leaning POST (it has a side effect: a broker round-trip).
3. **Keep the in-process theme apply during Phase 1, or make the hosted SwiftUI window call back to a shell apply hook immediately?** — cheaper to keep in-process now; flag it as tech-debt to remove at the flip.

## Phase 1 build order

1. Lock this contract (types + routes + gating helper).
2. Add the loopback-gated `GET /api/settings` + the three writes + `POST /api/account/energy/refresh`; unit-test against a temp `--data-dir`.
3. Point the **web** Settings SPA at the new API (proves the contract with the existing client before any Swift).
4. Build the SwiftUI Settings window as an `NSHostingView` client of the same API; keep the objc2 window as fallback until it's at parity.
