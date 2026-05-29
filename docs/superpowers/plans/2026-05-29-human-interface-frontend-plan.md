# Implementation Plan — human interface frontend redesign

Plan for [the design spec](../specs/2026-05-29-human-interface-frontend-design.md).
Branch: `redesign-human-interface-frontend`.

## Conventions
- Each task names the files it touches and a verification step. Keep the app
  building (`npm run build` in `src/appearance/web`, then `cargo build`) and in a
  working state at the end of every phase.
- No new frontend dependencies — canvas + CSS only.
- TDD the pure/testable backend logic (sentence segmentation, surface bus);
  frontend canvas/animation is verified by build + manual smoke.
- Dev loop: `make dev` (Vite :5173 proxy → Rust :8080). Add `/surface` to the
  Vite proxy channel list in Phase 3.

---

## Phase 1 — the calm shell (listen + text), runs on existing channels

Goal: wake → hands-free listening (your voice drives the dot-matrix) → STT →
agent text fades in as whole sentences → barge-in. No TTS, no content yet.

1. **Design tokens & resets** — rewrite `src/ui/global.css`: cool palette,
   motion vars, breathing keyframes; remove `.hi-caret` typewriter + sci-fi
   leftovers. Keep reduced-motion block.
2. **`lib/audioBus.ts`** — one `AudioContext` + `AnalyserNode`; `setSource(mic |
   mediaElement | none)`; `read()` → `{ level, freq[] }` (log-banded, ported from
   `demos/dot-matrix.html`). Created lazily after the wake gesture.
3. **`lib/vad.ts`** — energy-based voice-activity detection over the analyser:
   start-of-speech, ~700ms trailing-silence endpoint, ~300ms minimum; emits
   `onUtteranceStart` / `onUtteranceEnd`. Unit-test the threshold/endpoint state
   machine with synthetic level sequences (vitest).
4. **`lib/sentences.ts`** — incremental sentence splitter (Latin `.?!…` + CJK
   `。？！` + newline); `push(chunk) → completed[]`, `flush()`. Vitest covered.
   (Mirror of the Rust splitter in Phase 2.)
5. **`ui/Presence.tsx`** — canvas dot-matrix radial spectrogram ported from the
   demo; reads `audioBus.read()` each frame; `state` prop drives demote/dim;
   honors reduced-motion. Single rAF.
6. **`ui/Atmosphere.tsx`** — animated background (drifting radial gradient +
   grain), ~10s breathing. CSS-driven.
7. **`ui/SpeechText.tsx`** — consumes completed sentences; fades each in
   (rise+opacity ~700ms), holds 1–2, gently fades older.
8. **`channels/audio.ts`** — add `startContinuousCapture({ vad, onUtterance })`
   that records the active utterance (reuse WAV encode) and `postAudio`s it;
   keep existing `postAudio`. **`channels/thought.ts`** unchanged.
9. **`lib/peer.ts`** — silent `web@local` get/persist (localStorage). No UI.
10. **`hooks/useAgentSession.ts`** — coordinator: state machine
    (`waking|idle|listening|thinking|speaking|offline`), owns the `/thought`
    subscribe loop (existing), the VAD→capture→`/audio` loop, the audioBus source
    switching (mic while listening), and the sentence buffer feeding SpeechText.
11. **`ui/WakeGate.tsx`** — first-run breathing dot + "tap to begin"; on tap:
    `getUserMedia` + `audioCtx.resume()`, then start the session.
12. **`ui/KeyboardFallback.tsx`** — hidden; any printable key reveals a minimal
    bottom input → `postThought`; Esc/blur hides.
13. **`App.tsx`** — rewrite to compose Atmosphere + Presence + SpeechText +
    WakeGate + KeyboardFallback via `useAgentSession`. Delete `Orb`, `Waveform`,
    `HUD`, `Composer`, `Transcript`, `ParticleField`, `GridFloor`.
14. **Build & smoke** — `npm run build`; `cargo build`; run `make dev`, wake,
    speak → dots react → transcript posts → agent text fades in; interrupt
    mid-reply and confirm the turn cancels.

Verification (Phase 1 done): the above smoke passes; no dead imports; build green.

---

## Phase 2 — voice out (TTS)

Goal: the agent talks; the dot-matrix rides its real voice; text & voice stay in
sync, per sentence.

Backend:
1. **`src/voice/segment.rs`** (or in `voice/mod.rs`) — sentence splitter mirroring
   the frontend; unit tests. Used to chunk the reply for TTS.
2. **`src/reactor.rs`** — thread `Option<Arc<dyn Tts>>` + `broadcast::Sender<AudioEvent>`
   into `ReactorInner` (via `start(...)`/`server::build` seams). In
   `run_routing_turn`, accumulate text, and at each sentence boundary
   `tts.synthesize(sentence)` → `audio_out.send(AudioEvent{ to: peer, ... })`.
   Skip cleanly when `tts` is `None`. Journal `SignalOut{ channel: Audio,
   media_path }` for parity.
3. **`.env`** — `TTS_PROVIDER=volcengine`.

Frontend:
4. **`channels/audio.ts`** — `subscribeAudio({ peer, signal })` long-poll on
   `GET /audio`; yields blobs.
5. **`hooks/useAgentSession.ts`** — on agent turn, play each audio blob through a
   `MediaElementAudioSourceNode` routed into `audioBus`; set `state=speaking` and
   `audioBus.setSource(playback)`; gate the matching sentence's fade-in to the
   audio start so text & voice align.
6. **Barge-in** — while speaking, VAD still runs; detected speech ducks/stops
   playback locally (the new `/audio` POST cancels the turn server-side).

Verification: agent reply is spoken; dots track the agent's voice; with
`TTS_PROVIDER` unset it silently degrades to Phase-1 behavior.

---

## Phase 3 — content overlay

Goal: agent surfaces HTML blocks as card/full overlays; history recall.

Backend:
1. **`src/server/surface_bus.rs`** — per-peer envelope bus (model on
   `thought_bus.rs`); `push(peer, envelope)` + `subscribe(peer)` stream. Tests
   modeled on `tests/thought_race.rs`.
2. **`src/server/mod.rs`** — `GET /surface` route + `SurfaceBus` in `AppState` /
   `ServerSeams`.
3. **`src/types.rs`** — `SurfaceEnvelope { id, op, mode, html, ttl_ms }` (serde).
4. **`src/reactor.rs`** — on `SessionUpdate::ToolCall` named `show`, parse args →
   `surface_bus.push`. Finalize `show`-tool exposure to the ACP session; if
   heavy, parse a fenced ` ```hi:show ` block from the text stream instead
   (documented fallback). Keep text outside blocks flowing to `/thought`.

Frontend:
5. **`channels/surface.ts`** — `GET /surface` subscriber yielding envelopes.
6. **`vite.config.ts`** — add `/surface` to `HI_CHANNELS`.
7. **`ui/SurfaceHost.tsx`** — sandboxed `<iframe sandbox="allow-scripts">`
   overlay; `card`/`full`; enter (fade+rise+scale ~400ms) / exit (~300ms);
   `ttl_ms` auto-dismiss; dismiss affordance.
8. **`ui/Presence.tsx`** — demote/dim when a surface is up.
9. **`ui/HistoryDrawer.tsx`** — pull-up scrollback of past sentences + content
   cards; closed by default.
10. **`hooks/useAgentSession.ts`** — wire surface stream → SurfaceHost + history.

Verification: agent `show(html, mode)` → overlay animates in; ttl/dismiss works;
history recalls prior blocks.

---

## Testing strategy
- Vitest: `sentences.ts`, `vad.ts` state machine.
- Rust: `segment.rs` unit tests; `surface_bus` race test (mirror
  `thought_race.rs`).
- Manual smoke per phase (above). Each phase ends green and demoable.

## Risks
Carried from spec §10: mic/TTS echo, iframe sandbox trust, in-browser VAD
quality, `show` tool exposure post-MCP, frontend/Rust sentence-split agreement.
