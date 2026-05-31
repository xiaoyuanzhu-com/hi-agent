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

const rgba = (c: RGB, a: number) => `rgba(${c[0]},${c[1]},${c[2]},${a})`;
const hsla = (h: number, s: number, l: number, a: number) => `hsla(${h},${s}%,${l}%,${a})`;

function hexToRgb(hex: string, fallback: RGB): RGB {
  const m = /^#?([0-9a-f]{6})$/i.exec(hex.trim());
  if (!m) return fallback;
  const n = parseInt(m[1]!, 16);
  return [(n >> 16) & 255, (n >> 8) & 255, n & 255];
}

interface Palette {
  bg0: RGB;
  bg1: RGB;
}

/** Read the neutral paper tokens from CSS. (Halo colours are the curated
 *  PALETTE below — not tokens — so the spectrum stays hand-tuned.) */
function readPalette(): Palette {
  const cs = getComputedStyle(document.documentElement);
  const v = (name: string, fb: RGB) => hexToRgb(cs.getPropertyValue(name), fb);
  return {
    bg0: v("--bg-0", [246, 248, 251]),
    bg1: v("--bg-1", [238, 241, 246]),
  };
}

// ── Curated halo palette ─────────────────────────────────────────────────────
// A hand-picked "tech" spectrum (in the spirit of Apple Intelligence / OpenAI
// voice): a cohesive cyan → blue → indigo → magenta → pink sweep, all cool and
// vivid, so *any* combination stays harmonious. Halos only ever draw from this —
// never a free random hue — so nothing clashes. Each state picks a small subset;
// a halo is born on one entry and drifts toward another in the same subset over
// its life. Edit these HSL triples to re-skin the whole field.
type HSL = [number, number, number];
const C = {
  cyan: [186, 90, 58] as HSL,
  blue: [214, 92, 62] as HSL,
  indigo: [250, 84, 66] as HSL,
  magenta: [296, 74, 64] as HSL,
  pink: [336, 90, 68] as HSL,
  slate: [214, 16, 62] as HSL, // offline — desaturated
};
const STATE_COLORS: Record<PresenceState, HSL[]> = {
  idle: [C.blue, C.indigo],       // resting — calm cool
  listening: [C.cyan, C.blue],    // human has the floor — bright cool
  thinking: [C.indigo, C.magenta],// cognition — purple swirl
  speaking: [C.magenta, C.pink],  // agent voice — warm glow
  offline: [C.slate],             // disconnected
  waking: [C.blue],
};

const rand = (a: number, b: number) => a + Math.random() * (b - a);
const pick = <T,>(arr: T[]): T => arr[Math.floor(Math.random() * arr.length)]!;

/**
 * A living halo. Short-lived by design: over its `life` it fades in, grows,
 * drifts and shifts hue, then shrinks and fades back out. Born with properties
 * sampled from the current state's colour but jittered per-individual, so no two
 * are alike and the field stays alive as halos are continually born and die.
 */
interface Halo {
  x: number; y: number;        // normalized birth position
  vx: number; vy: number;      // slow linear drift (per second)
  wax: number; way: number;    // wander amplitude
  wpx: number; wpy: number;    // wander phase
  born: number; life: number;  // seconds
  rMax: number;                // peak radius (× min(W,H))
  h0: number; h1: number;      // hue birth → death (its colour gradient)
  sat: number; lig: number;
  w: number;                   // intensity weight
}

function spawnHalo(t: number, state: PresenceState): Halo {
  const set = STATE_COLORS[state] ?? STATE_COLORS.idle;
  const c0 = pick(set); // born on this colour…
  const c1 = pick(set); // …drifts toward this one (same subset → stays in key)
  return {
    x: rand(0.1, 0.9),
    y: rand(0.12, 0.88),
    vx: rand(-0.016, 0.016),
    vy: rand(-0.016, 0.016),
    wax: rand(0.01, 0.045),
    way: rand(0.01, 0.045),
    wpx: rand(0, Math.PI * 2),
    wpy: rand(0, Math.PI * 2),
    born: t,
    life: rand(7, 15),
    rMax: rand(0.12, 0.28),
    h0: c0[0] + rand(-5, 5), // tiny jitter only — never off-palette
    h1: c1[0] + rand(-5, 5),
    sat: Math.min(100, c0[1] + rand(-3, 5)),
    lig: c0[2] + rand(-3, 3),
    w: rand(0.7, 1),
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

/**
 * The agent's presence — expressed entirely in the background.
 *
 * A neutral white field behind frosted glass (Atmosphere), populated by a small
 * shifting cast of *living halos*: each is born somewhere random, fades in,
 * grows, drifts, shifts hue, then shrinks and fades out — so the field is never
 * static. Their colour is sampled from the current interaction state (blue at
 * rest, teal while the human holds the floor, violet while thinking, amber while
 * speaking), jittered per-individual, so the field reads the state while every
 * halo is its own. Brightness and population ride live audio (`reactive`, via
 * the `AudioBus`) or the cognition cadence (`activity`), else a gentle synthetic
 * breath. A light vignette keeps the centre calm for words; `demote` steps the
 * field back when a surface is up. Mounted once and driven by refs so it never
 * unmounts across state changes.
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
    let halos: Halo[] = [];
    let raf = 0;
    const t0 = performance.now();

    const draw = (t: number) => {
      const dem = demoteRef.current;

      // base wash — neutral white paper
      const bg = ctx.createLinearGradient(0, 0, 0, H);
      bg.addColorStop(0, rgba(pal.bg0, 1));
      bg.addColorStop(1, rgba(pal.bg1, 1));
      ctx.fillStyle = bg;
      ctx.fillRect(0, 0, W, H);

      const md = Math.min(W, H);
      const demoteMul = 1 - dem * 0.72;

      // Each halo: a vivid radial of clean coloured light. Its life maps to a
      // single sine arc — born small & dim, swelling bright at mid-life, then
      // shrinking and fading away. Tight core + quick fade keeps the colour
      // concentrated (the glass above does the softening); the edge fades to the
      // SAME hue at alpha 0, so there's never a grey ring.
      for (const o of halos) {
        const age = t - o.born;
        const u = age / o.life; // 0..1 over its life
        const env = Math.sin(Math.PI * u) ** 0.7; // fade in → out
        if (env <= 0.002) continue;
        const grow = 0.5 + 0.5 * Math.sin(Math.PI * u); // small → big → small
        const x = (o.x + o.vx * age + o.wax * Math.sin(o.wpx + age * 0.24)) * W;
        const y = (o.y + o.vy * age + o.way * Math.sin(o.wpy + age * 0.2)) * H;
        const R = md * o.rMax * (0.55 + 0.45 * grow);
        const hue = o.h0 + (o.h1 - o.h0) * u; // drifts hue across its life
        const a = (0.64 + level * 0.4) * env * o.w * demoteMul;
        const g = ctx.createRadialGradient(x, y, 0, x, y, R);
        g.addColorStop(0, hsla(hue, o.sat, o.lig, Math.min(0.98, a)));
        g.addColorStop(0.5, hsla(hue, o.sat, o.lig, a * 0.52));
        g.addColorStop(1, hsla(hue, o.sat, o.lig, 0));
        ctx.fillStyle = g;
        ctx.fillRect(0, 0, W, H);
      }

      // Legibility comes from the frosted glass above, so the centre wash is
      // light — just enough to settle the field, plus the demote push when a
      // content surface is up, and a soft edge for depth.
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

      const liveBus = busRef.current;
      const act = activityRef.current;
      const st = stateRef.current;

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

      // Tend the population: cull the dead, and let a few more be born when the
      // field is livelier. New halos sample the *current* state's colour, so a
      // state change re-tints the field organically as old halos age out — no
      // snap, because the change rides in on the next births.
      halos = halos.filter((o) => t - o.born < o.life);
      const targetCount = 4 + Math.round(level * 4);
      if (halos.length < targetCount) halos.push(spawnHalo(t, st));

      draw(t);
      raf = requestAnimationFrame(tick);
    };

    if (reduce) {
      // A still frame: a few halos frozen mid-life so the field isn't blank.
      level = synthLevel(stateRef.current, 0);
      for (let i = 0; i < 5; i++) {
        const o = spawnHalo(0, stateRef.current);
        o.born = -o.life * rand(0.3, 0.6); // placed past their fade-in
        halos.push(o);
      }
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
