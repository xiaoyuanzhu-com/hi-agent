import { useEffect, useRef } from "react";
import type { AudioBus } from "../lib/audioBus";
import type { ActivityMeter } from "../lib/activityMeter";

export type PresenceState =
  | "waking"
  | "idle"
  | "listening"
  | "thinking"
  | "speaking"
  | "offline";

interface PresenceProps {
  /** Live audio source; null before wake. */
  bus: AudioBus | null;
  state: PresenceState;
  /** When true the field tracks live audio (mic, or — Phase 2 — TTS playback);
   *  otherwise it breathes on a synthetic, state-dependent envelope. */
  reactive?: boolean;
  /** Live cognition cadence (streamed-chunk pulses). When not audio-reactive,
   *  this lifts the field while thinking/speaking so it rides real output, not a
   *  canned loop. Null before wake → pure synthetic breath. */
  activity?: ActivityMeter | null;
  /** 0..1 — how much a content overlay is up; the field recedes as this rises. */
  demote?: number;
}

type RGB = [number, number, number];

const mix = (a: RGB, b: RGB, t: number): RGB => [
  a[0] + (b[0] - a[0]) * t,
  a[1] + (b[1] - a[1]) * t,
  a[2] + (b[2] - a[2]) * t,
];
const rgba = (c: RGB, a: number) => `rgba(${c[0]},${c[1]},${c[2]},${a})`;

function hexToRgb(hex: string, fallback: RGB): RGB {
  const m = /^#?([0-9a-f]{6})$/i.exec(hex.trim());
  if (!m) return fallback;
  const n = parseInt(m[1]!, 16);
  return [(n >> 16) & 255, (n >> 8) & 255, n & 255];
}

interface Palette {
  bg0: RGB;
  bg1: RGB;
  // One halo colour per interaction state — this is the *only* colour in the
  // scene; the paper and the glass stay neutral white.
  idle: RGB;
  listen: RGB;
  think: RGB;
  speak: RGB;
  offline: RGB;
}

/** Read the look-and-feel from the centralized CSS tokens so a future skin can
 *  re-tint the field purely by swapping `:root` variables (no canvas changes). */
function readPalette(): Palette {
  const cs = getComputedStyle(document.documentElement);
  const v = (name: string, fb: RGB) => hexToRgb(cs.getPropertyValue(name), fb);
  return {
    bg0: v("--bg-0", [246, 248, 251]),
    bg1: v("--bg-1", [238, 241, 246]),
    idle: v("--presence-idle", [90, 155, 242]),
    listen: v("--presence-listen", [32, 194, 160]),
    think: v("--presence-think", [147, 116, 255]),
    speak: v("--presence-speak", [255, 140, 63]),
    offline: v("--presence-offline", [154, 163, 178]),
  };
}

/** The halo colour for an interaction state. The room glows blue at rest, teal
 *  while the human holds the floor, violet while the agent thinks, and warm
 *  amber while it speaks — so the colour alone reads the state. */
function stateAccent(state: PresenceState, pal: Palette): RGB {
  switch (state) {
    case "listening":
      return pal.listen;
    case "thinking":
      return pal.think;
    case "speaking":
      return pal.speak;
    case "offline":
      return pal.offline;
    case "waking":
    case "idle":
    default:
      return pal.idle;
  }
}

/** Synthetic 0..1 envelope when there is no live audio to read. */
function synthLevel(state: PresenceState, t: number): number {
  switch (state) {
    case "idle":
      return 0.1 + 0.05 * (Math.sin(t * 0.5) * 0.5 + 0.5);
    case "thinking":
      return (
        0.16 +
        0.05 * (Math.sin(t * 0.8) * 0.5 + 0.5) +
        0.03 * (Math.sin(t * 1.9 + 1.3) * 0.5 + 0.5)
      );
    case "speaking": {
      // speech-like cadence: swells with brief pauses (used only when TTS isn't
      // feeding the bus, so the room still "talks").
      const gate = Math.max(0, Math.sin(t * 1.6)) ** 0.6;
      const syl =
        (Math.sin(t * 11) * 0.5 + 0.5) * 0.6 + (Math.sin(t * 6.5) * 0.5 + 0.5) * 0.4;
      return 0.12 + gate * syl * 0.46;
    }
    case "listening":
      return 0.16;
    case "offline":
      return 0.05 + 0.02 * (Math.sin(t * 0.4) * 0.5 + 0.5);
    case "waking":
    default:
      return 0.03;
  }
}

// Soft drifting light-pools. The whole room breathes; there is never a central
// object competing with the words or a content card on top.
const POOLS = [
  { px: 0.26, py: 0.28, ax: 0.1, ay: 0.07, fx: 0.013, fy: 0.009, fr: 1 / 19, ph: 0.0, w: 1.0 },
  { px: 0.74, py: 0.34, ax: 0.09, ay: 0.09, fx: 0.011, fy: 0.015, fr: 1 / 23, ph: 2.1, w: 0.9 },
  { px: 0.5, py: 0.78, ax: 0.12, ay: 0.06, fx: 0.009, fy: 0.012, fr: 1 / 27, ph: 4.2, w: 0.85 },
];

/**
 * The agent's presence — expressed entirely in the background.
 *
 * A neutral white field with a few large, soft colour-pools that drift and
 * breathe on long cycles, seen through the frosted glass above (Atmosphere).
 * The agent's internal state lives here, carried by colour: the room glows blue
 * at idle (one slow breath), teal while the human holds the floor, violet while
 * thinking, and warm amber while it speaks — deepening/receding with the voice.
 * Audio reactivity reads the live `AudioBus` when `reactive`; otherwise a gentle
 * synthetic envelope. A content-safe vignette keeps the centre calm so words and
 * cards stay legible; `demote` makes the whole field step back when a surface is
 * up. Mounted once and driven by refs so it never unmounts across state changes.
 */
export function Presence({ bus, state, reactive = false, activity = null, demote = 0 }: PresenceProps) {
  const canvasRef = useRef<HTMLCanvasElement | null>(null);
  const stateRef = useRef(state);
  const reactiveRef = useRef(reactive);
  const demoteRef = useRef(demote);
  const busRef = useRef(bus);
  const activityRef = useRef(activity);
  useEffect(() => { stateRef.current = state; }, [state]);
  useEffect(() => { reactiveRef.current = reactive; }, [reactive]);
  useEffect(() => { demoteRef.current = demote; }, [demote]);
  useEffect(() => { busRef.current = bus; }, [bus]);
  useEffect(() => { activityRef.current = activity; }, [activity]);

  useEffect(() => {
    const canvas = canvasRef.current;
    if (!canvas) return;
    const ctx = canvas.getContext("2d");
    if (!ctx) return;

    const reduce = window.matchMedia("(prefers-reduced-motion: reduce)").matches;
    const DPR = Math.min(2, window.devicePixelRatio || 1);
    let pal = readPalette();
    let W = 0;
    let H = 0;

    const resize = () => {
      W = canvas.clientWidth;
      H = canvas.clientHeight;
      canvas.width = Math.round(W * DPR);
      canvas.height = Math.round(H * DPR);
      ctx.setTransform(DPR, 0, 0, DPR, 0, 0);
      pal = readPalette(); // re-read tokens in case a skin changed them
    };
    resize();
    const ro = new ResizeObserver(resize);
    ro.observe(canvas);

    let level = 0.1;
    let drift = 0; // integral of the (smoothed) motion rate — see tick()
    let moveScale = 1.0; // eased per-state motion rate; never stepped
    let accentCur: RGB | null = null; // crossfaded tint; never snapped
    let raf = 0;
    let last = performance.now();
    const t0 = last;

    const draw = (t: number) => {
      const st = stateRef.current;
      const dem = demoteRef.current;

      // base wash
      const bg = ctx.createLinearGradient(0, 0, 0, H);
      bg.addColorStop(0, rgba(pal.bg0, 1));
      bg.addColorStop(1, rgba(pal.bg1, 1));
      ctx.fillStyle = bg;
      ctx.fillRect(0, 0, W, H);

      // Target halo colour for the state, then crossfade toward it so a state
      // flip eases the hue rather than snapping it in one frame.
      const accentTarget = stateAccent(st, pal);
      accentCur = accentCur ? mix(accentCur, accentTarget, 0.05) : accentTarget;
      const accent = accentCur;
      const md = Math.min(W, H);
      const demoteMul = 1 - dem * 0.72;

      // Paint the pools as clean coloured *light*, not pigment: normal
      // compositing with saturated hues, so they read as a glow behind the
      // frosted glass. (multiply darkens toward grey here — looked muddy/dirty.)
      // The white veil on top desaturates ~half, so paint them strong & vivid.
      for (const pool of POOLS) {
        // drift already carries the motion rate (∫ moveScale dt), so the phase
        // stays continuous across state changes — no positional teleport.
        const dx = Math.sin(2 * Math.PI * pool.fx * drift + pool.ph) * pool.ax;
        const dy = Math.sin(2 * Math.PI * pool.fy * drift + pool.ph * 1.3) * pool.ay;
        const cx = (pool.px + dx) * W;
        const cy = (pool.py + dy) * H;
        const breath = Math.sin(2 * Math.PI * pool.fr * t + pool.ph) * 0.5 + 0.5;
        const R = md * (0.34 + breath * 0.05) * (1 + level * 0.35);
        const a = (0.4 + breath * 0.1 + level * 0.4) * pool.w * demoteMul;
        const g = ctx.createRadialGradient(cx, cy, 0, cx, cy, R);
        // Fade to the SAME hue at alpha 0 — never to "transparent" (= transparent
        // *black*), which interpolates RGB toward black and leaves a dirty grey
        // ring at the edge.
        g.addColorStop(0, rgba(accent, Math.min(0.9, a)));
        g.addColorStop(0.4, rgba(accent, a * 0.55));
        g.addColorStop(1, rgba(accent, 0));
        ctx.fillStyle = g;
        ctx.fillRect(0, 0, W, H);
      }

      // Legibility now comes from the frosted glass above, so the centre wash is
      // light — just enough to settle the field, plus the demote push when a
      // content surface is up, and a soft warm edge for depth.
      const vx = W / 2;
      const vy = H * 0.52;
      const vig = ctx.createRadialGradient(vx, vy, 0, vx, vy, md * 0.7);
      vig.addColorStop(0, rgba(pal.bg0, 0.12 + dem * 0.36));
      vig.addColorStop(0.6, rgba(pal.bg0, 0));
      vig.addColorStop(1, rgba(pal.bg1, 0.22));
      ctx.fillStyle = vig;
      ctx.fillRect(0, 0, W, H);
    };

    const tick = (now: number) => {
      const t = (now - t0) / 1000;
      const dt = Math.min(0.05, (now - last) / 1000);
      last = now;

      const liveBus = busRef.current;
      const act = activityRef.current;
      const st = stateRef.current;

      // Ease the motion rate between states, then integrate it: pools drift on a
      // continuous phase, so a state flip changes their *speed*, never their spot.
      const targetMove = st === "thinking" ? 1.5 : st === "idle" ? 0.8 : st === "waking" ? 0.5 : 1.0;
      moveScale += (targetMove - moveScale) * 0.05;
      drift += moveScale * dt;

      // Brightness target: live audio when reactive; otherwise a synthetic breath
      // lifted by real cognition cadence (streamed-chunk pulses) while
      // thinking/speaking, so the field rides actual output, not a canned loop.
      let target: number;
      if (liveBus && reactiveRef.current) {
        target = Math.min(0.6, 0.12 + liveBus.read().level * 0.42);
      } else {
        const synth = synthLevel(st, t);
        const pulse = act ? act.read() : 0;
        target =
          st === "thinking" || st === "speaking"
            ? Math.max(synth, 0.14 + pulse * 0.42)
            : synth;
      }
      level += (target - level) * 0.06; // heavy smoothing → calm

      draw(t);
      raf = requestAnimationFrame(tick);
    };

    if (reduce) {
      level = synthLevel(stateRef.current, 0);
      draw(0);
    } else {
      raf = requestAnimationFrame(tick);
    }

    return () => {
      cancelAnimationFrame(raf);
      ro.disconnect();
    };
  }, []);

  return <canvas ref={canvasRef} aria-hidden className="hi-presence" />;
}
