# hi-agent — human interface frontend redesign

status: design · 2026-05-29 · supersedes the current "sci-fi shell" SPA

## 1. Context

`hi-agent` is a reference implementation of the human-interface spec: a Rust
process that talks over HTTP channels and delegates cognition to `claude-code`
over ACP. The browser SPA (`src/appearance/web`, React + Vite + TS, embedded
into the binary via `rust-embed`) is its face.

The current SPA is a busy "sci-fi cockpit": a morphing orb, a HUD (brand,
clock, channel grid, editable peer), a bottom composer (mic + textarea + send),
a grid floor, a particle field, a transcript drawer, and a typewriter caret.

This redesign replaces that surface. The agent is not a voice toy — it is a
full agent that returns **free-form rich content** (text, images, video, HTML,
widget/file previews). The interface must do two things well at once:

1. Be a **calm, minimal, breathing presence** for the common case (voice + text).
2. Be an **extensive, flexible host** for arbitrary rich content the agent
   generates — *the agent owns content and layout; the frontend is a thin,
   beautiful shell plus one sandboxed renderer.*

### Design goals (from the brief)
- 科技感 (tech), 呼吸感 (breathing), 互动感 (interactive), 极简 (minimal), calm.
- Intelligent, minimal interaction. **No input box and no voice button by
  default** — it listens and speaks like a human.
- A beautiful, interactive **sound-waveform visualization**.
- The agent's speech appears as **whole sentences that fade in/out** —
  smooth, elegant, calm. **Never** letter-by-letter.

## 2. Principles

- **Calm core, content on demand.** Default state is near-empty and breathing.
  Rich content rises as an overlay, takes focus, then recedes back to calm.
- **The agent owns the display.** Rich content is agent-authored HTML rendered
  in a sandbox. The frontend hand-codes only the *common* surfaces (voice + text)
  and the shell — not per-type widgets.
- **One presence, many states.** A single visual element (the dot-matrix)
  represents the agent across idle / listening / thinking / speaking, never
  unmounting, so transitions read as one continuous being.
- **Motion is slow and eased.** Breathing ≈ 0.1 Hz. Color rests near-neutral and
  blooms with activity. `prefers-reduced-motion` is honored.

## 3. The experience (state walkthrough)

```
wake → idle → listening → thinking → speaking → (content-up) → idle
                  ↑___________________________________|
```

- **wake** — first load only. A single breathing dot and one line ("tap to
  begin"). One tap grants the mic and unlocks audio playback (both are
  browser-required user gestures). After this the session is hands-free.
- **idle** — atmosphere + dot-matrix breathing gently. No chrome.
- **listening** — open mic; the dot-matrix is driven by *your* voice
  (radial spectrogram). VAD segments your speech.
- **thinking** — utterance posted; awaiting the agent. Dots settle into a slow,
  low-energy hold.
- **speaking** — TTS audio plays; the dot-matrix is driven by the *agent's*
  voice; the agent's words fade in sentence by sentence.
- **content-up** — agent surfaced a block; it animates in as a `card` or `full`
  overlay; the presence demotes to a slim dim band. Recedes on dismissal.
- **error / offline** — `/thought` (or other stream) dropped; the presence
  dims to a near-still, cool-neutral state with a single quiet status line.

## 4. Visual system

### 4.1 Atmosphere (background)
Near-black field (`#04060d`) with a slow-drifting radial gradient and faint
grain. Breathes on a ~10s cycle (subtle luminance/scale). No grid floor, no
heavy particle field. It never competes with content.

### 4.2 Presence — dot-matrix radial spectrogram
A grid of dots. Distance from center maps to pitch (low → inner rings, high →
outer); brightness maps to energy in that band; a faint idle shimmer keeps it
alive when silent. Ported from `demos/dot-matrix.html` (the "radial
spectrogram" mode), which is the agreed reference implementation.

- **Driven by a shared Web Audio analyser** whose source switches between the
  **microphone** (listening) and the **TTS playback** (speaking). When neither
  is active, a gentle synthetic envelope drives the idle breathing.
- Center-stage and large when idle/conversing; demotes to a slim, dimmed band
  (e.g. bottom third) when a content overlay is up, so it stays alive without
  competing.
- Reference controls (mode/palette/sensitivity/smoothing) are demo-only; the
  product ships **radial spectrogram, cool palette, high smoothing**, exposed as
  CSS/config tokens so they remain swappable.

### 4.3 Color & motion
Cool palette default: cyan `#5af6ff` + soft violet `#9b8cff` on near-black, with
a brighter `#a8fbff` for peaks. Palettes (`cool` / `warm` / `mono`) live as token
sets; only `cool` ships, not user-facing initially. Color saturation/brightness
tracks activity — near-neutral at rest, blooming when alive. All transitions
eased and cross-faded; no element "pops" between states.

### 4.4 Text = calm speech
`GET /thought` streams text chunks, but the UI **buffers chunks into whole
sentences** (split on sentence terminators, handling both Latin `.?!…` and CJK
`。？！` plus newlines) and **fades each completed sentence in** (rise +
opacity, ~600–800ms ease), holds it, and gently fades older sentences as new
ones arrive. **One to two sentences visible at a time**, centered with the
presence. No typewriter reveal, no caret.

## 5. Interaction model — always on, no chrome

- **The one gesture: wake.** Browsers require a user action to grant mic access
  and to unlock audio playback. First load shows the wake affordance; one tap
  satisfies both. There is no other required interaction.
- **Listening (no button).** After wake, the mic stays open. **Client-side VAD**
  detects speech, endpoints on ~700ms of trailing silence, enforces a ~300ms
  minimum, and POSTs each finalized utterance to `POST /audio` (existing STT
  path). No push-to-talk, no mic button.
- **Barge-in.** Speaking while the agent talks: the new `POST /audio` reaches
  the reactor, which already **cancels the in-flight ACP turn** (see
  `reactor.rs` interruption policy). Locally, detecting user speech during TTS
  ducks/stops playback immediately so the agent yields like a person would.
- **Keyboard fallback (hidden).** Pressing any printable key reveals a minimal
  single-line input at the bottom edge that posts to `POST /thought`; Esc or
  blur hides it. For noisy rooms and accessibility. Hidden by default — does not
  violate 极简.
- **Dismissing content.** By voice ("close that" / "go back" — agent-driven) and
  by a subtle affordance (tap outside a `card`, or a small corner control on
  `full`). The agent may also auto-recede a block via `ttl`.

## 6. Content overlay system

The agent emits **HTML** blocks rendered inside a **sandboxed `<iframe>`**
(`sandbox="allow-scripts"`, **without** `allow-same-origin`, so agent HTML is
isolated from the app origin; size-constrained; framed by atmosphere). Two
modes:

- **`card`** — centered, floating panel; atmosphere dims behind; presence
  demotes. For focused artifacts (an image, a chart, a short preview).
- **`full`** — full-bleed; presence becomes a thin edge band. For immersive
  content (a webpage, a video, a dashboard).

Enter: fade + rise + slight scale-up (~400ms). Exit: fade + recede (~300ms).
Only the current block(s) live on screen; prior blocks fall into history.

### Envelope schema
`GET /surface` (new) is a long-poll that streams JSON envelopes, one per line:

```jsonc
{
  "id": "srf_01J...",        // unique; used for update/dismiss
  "op": "show" | "dismiss",  // show a block, or recede one by id
  "mode": "card" | "full",   // for op=show
  "html": "<...>",           // for op=show; self-contained HTML/CSS/JS
  "ttl_ms": 30000            // optional; auto-dismiss after ttl
}
```

## 7. Architecture & data flow

```
  you ──voice──▶ mic ─VAD─▶ POST /audio ─STT─▶ reactor ──▶ claude-code (ACP)
                                                  │
  browser ◀── GET /thought   text chunks  ◀───────┤  → sentence buffer → calm fade
  browser ◀── GET /audio      TTS bytes    ◀───────┤  → play + analyser → drives dot-matrix   [NEW emit]
  browser ◀── GET /surface    HTML blocks  ◀───────┘  → sandboxed overlay (card / full)        [NEW channel]
```

### 7.1 Frontend (rebuild under `src/appearance/web/src`)
Thin shell + flexible host. Components:
- `Atmosphere` — animated background.
- `Presence` — dot-matrix canvas; reads the shared analyser each frame.
- `SpeechText` — sentence buffer + fade animation.
- `SurfaceHost` — sandboxed-iframe overlay manager (card/full, enter/exit, ttl).
- `WakeGate` — first-run tap; acquires mic + unlocks audio.
- `HistoryDrawer` — pull-up scrollback of past sentences + content cards
  (closed by default).
- `KeyboardFallback` — hidden single-line input.
- `useAgentSession` — coordinator hook: owns the state machine, the channel
  clients (`thought.ts`, `audio.ts`, new `surface.ts`), the VAD loop, and an
  `AudioBus` (one `AudioContext` + analyser whose source switches mic ↔ TTS).

Kept/refactored: `channels/thought.ts`, `channels/audio.ts` (add streaming/VAD
helpers), peer identity (silent `web@local` in localStorage). Removed: `Orb`,
`Waveform`, `HUD`, `Composer`, `Transcript`, `ParticleField`, grid floor,
typewriter caret.

### 7.2 Backend deltas
- **Voice out (new emit).** Thread `Option<Arc<dyn Tts>>` + the `audio_out`
  broadcast sender into the reactor. As the routing turn produces text, segment
  it into sentences, `Tts::synthesize` **per sentence**, and broadcast an
  `AudioEvent` per sentence (low latency; keeps voice aligned with the text
  fade). Set `TTS_PROVIDER=volcengine` (provider already implemented). `GET
  /audio` already serves the broadcast.
- **Content out (new channel).** Add a `SurfaceBus` (analogous to `ThoughtBus`)
  and `GET /surface` long-poll in `src/server`. The reactor emits envelopes when
  the agent requests a block. **Emit mechanism:** the agent calls a `show` tool;
  the reactor already observes `SessionUpdate::ToolCall` and routes it to the
  `SurfaceBus`. Exposing the `show` tool to the ACP session is finalized in
  planning; **fallback** if tool exposure is heavy: the agent emits a fenced
  ```` ```hi:show ```` block in the `/thought` stream that the reactor splits out.

## 8. Phasing

1. **Shell** — Atmosphere + Presence (mic-driven) + SpeechText + WakeGate +
   continuous listen (VAD → `/audio`) + barge-in + keyboard fallback. Runs on
   existing channels; the dot-matrix reacts to *your* voice immediately.
2. **Voice-out** — reactor TTS per-sentence emit + `/audio` playback + analyser
   source-switch so the dot-matrix rides the agent's voice; text/voice sync.
3. **Content** — `surface.ts` + `GET /surface` + `SurfaceBus` + reactor emit +
   `SurfaceHost` sandboxed overlay + HistoryDrawer.

Each phase is independently demoable and leaves the app in a working state.

## 9. Non-goals
- Multi-window / spatial / zoomable canvas.
- User-driven layout controls (the agent owns layout).
- Per-content-type bespoke renderers beyond text/voice (agent HTML covers them).
- Multi-peer / presence-of-others UI.

## 10. Risks & open items
- **Mic/TTS echo.** With the mic open during TTS (for barge-in), the mic may
  hear the agent. Mitigation: `echoCancellation: true` (already used) + raise the
  VAD threshold while TTS plays. Acceptable residual risk for v0.
- **Sandbox capability.** `allow-scripts` without `allow-same-origin` blocks
  most exfiltration but agent HTML is still semi-trusted; revisit a stricter CSP
  / size + network constraints if content gets richer.
- **VAD quality in-browser.** Energy-based VAD is simple but imperfect; if
  endpointing is poor, consider a small WASM VAD. Out of scope for phase 1.
- **`show` tool exposure over ACP** post-MCP-removal — finalized in planning
  (tool-call vs fenced-block).
- **Sentence segmentation** must agree between the frontend (text fade) and the
  reactor (per-sentence TTS) to keep voice and text aligned.

## 11. Decisions log
- Presence: dot-matrix radial spectrogram; cool palette; high smoothing.
- Speech: voice + text; TTS wired (volcengine); dot-matrix rides agent audio;
  whole-sentence fade.
- Content: agent-authored HTML in a sandboxed iframe; `card` / `full`.
- Keyboard fallback: included, hidden (type to reveal → `/thought`).
- Identity: silent `web@local` in localStorage; no editor.
- History: ephemeral + unobtrusive pull-up scrollback.
- Content emit: reactor catches a `show` ACP tool-call; fenced-block fallback.
- Listening: client-side VAD; ~700ms endpoint; ~300ms min; barge-in ducks TTS.
- TTS granularity: per sentence.
- Stack: React + Vite + TS + rust-embed retained; canvas presence ported from
  `demos/dot-matrix.html`.

## 12. Reference artifacts
- `demos/presence-gallery.html` — the 10-option exploration (decision record).
- `demos/dot-matrix.html` — the chosen presence, with live mic; the port source.
