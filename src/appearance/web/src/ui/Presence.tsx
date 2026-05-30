import { useEffect, useRef } from "react";
import type { AudioBus } from "../lib/audioBus";

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
  accent: RGB;
  hot: RGB;
  think: RGB;
}

/** Read the look-and-feel from the centralized CSS tokens so a future skin can
 *  re-tint the field purely by swapping `:root` variables (no canvas changes). */
function readPalette(): Palette {
  const cs = getComputedStyle(document.documentElement);
  const v = (name: string, fb: RGB) => hexToRgb(cs.getPropertyValue(name), fb);
  return {
    bg0: v("--bg-0", [9, 11, 15]),
    bg1: v("--bg-1", [4, 5, 10]),
    accent: v("--presence-accent", [120, 150, 200]),
    hot: v("--presence-hot", [200, 216, 245]),
    think: v("--presence-think", [140, 138, 196]),
  };
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
 * A near-black field with a few large, soft light-pools that drift and breathe
 * on long cycles. The agent's internal state lives here: the room is nearly
 * still at idle (one slow breath), wanders a touch faster and cooler while
 * thinking, and brightens/recedes with the voice while listening or speaking.
 * Audio reactivity reads the live `AudioBus` when `reactive`; otherwise a gentle
 * synthetic envelope. A content-safe vignette keeps the centre calm so words and
 * cards stay legible; `demote` makes the whole field step back when a surface is
 * up. Mounted once and driven by refs so it never unmounts across state changes.
 */
export function Presence({ bus, state, reactive = false, demote = 0 }: PresenceProps) {
  const canvasRef = useRef<HTMLCanvasElement | null>(null);
  const stateRef = useRef(state);
  const reactiveRef = useRef(reactive);
  const demoteRef = useRef(demote);
  const busRef = useRef(bus);
  useEffect(() => { stateRef.current = state; }, [state]);
  useEffect(() => { reactiveRef.current = reactive; }, [reactive]);
  useEffect(() => { demoteRef.current = demote; }, [demote]);
  useEffect(() => { busRef.current = bus; }, [bus]);

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
    let drift = 0;
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

      const accent0 = st === "thinking" ? pal.think : pal.accent;
      const accent =
        st === "speaking"
          ? mix(accent0, pal.hot, 0.18)
          : st === "offline"
            ? mix(pal.accent, [120, 128, 150], 0.7)
            : accent0;
      const md = Math.min(W, H);
      const moveScale = st === "thinking" ? 1.5 : st === "idle" ? 0.8 : st === "waking" ? 0.5 : 1.0;
      const demoteMul = 1 - dem * 0.72;

      ctx.globalCompositeOperation = "lighter";
      for (const pool of POOLS) {
        const dx = Math.sin(2 * Math.PI * pool.fx * drift * moveScale + pool.ph) * pool.ax;
        const dy = Math.sin(2 * Math.PI * pool.fy * drift * moveScale + pool.ph * 1.3) * pool.ay;
        const cx = (pool.px + dx) * W;
        const cy = (pool.py + dy) * H;
        const breath = Math.sin(2 * Math.PI * pool.fr * t + pool.ph) * 0.5 + 0.5;
        const R = md * (0.34 + breath * 0.05) * (1 + level * 0.35);
        const a = (0.05 + breath * 0.05 + level * 0.16) * pool.w * demoteMul;
        const g = ctx.createRadialGradient(cx, cy, 0, cx, cy, R);
        g.addColorStop(0, rgba(accent, Math.min(0.4, a)));
        g.addColorStop(0.45, rgba(accent, a * 0.4));
        g.addColorStop(1, "transparent");
        ctx.fillStyle = g;
        ctx.fillRect(0, 0, W, H);
      }
      ctx.globalCompositeOperation = "source-over";

      // content-safe vignette: keep the centre (words / cards) calm and readable
      const vx = W / 2;
      const vy = H * 0.52;
      const vig = ctx.createRadialGradient(vx, vy, 0, vx, vy, md * 0.7);
      vig.addColorStop(0, rgba(pal.bg1, 0.3 + dem * 0.28));
      vig.addColorStop(0.6, rgba(pal.bg1, 0));
      vig.addColorStop(1, rgba(pal.bg1, 0.28));
      ctx.fillStyle = vig;
      ctx.fillRect(0, 0, W, H);
    };

    const tick = (now: number) => {
      const t = (now - t0) / 1000;
      const dt = Math.min(0.05, (now - last) / 1000);
      last = now;

      const liveBus = busRef.current;
      const st = stateRef.current;
      const target =
        liveBus && reactiveRef.current
          ? Math.min(0.6, 0.12 + liveBus.read().level * 0.42)
          : synthLevel(st, t);
      level += (target - level) * 0.06; // heavy smoothing → calm
      drift += dt; // pools drift continuously but slowly

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
