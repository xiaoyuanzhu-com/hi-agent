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

type RGB = [number, number, number]; // 0..1
type HSL = [number, number, number];

function hslToRgb(h: number, s: number, l: number): RGB {
  s /= 100;
  l /= 100;
  const k = (n: number) => (n + h / 30) % 12;
  const a = s * Math.min(l, 1 - l);
  const f = (n: number) => l - a * Math.max(-1, Math.min(k(n) - 3, Math.min(9 - k(n), 1)));
  return [f(0), f(8), f(4)];
}

// ── Curated pool palette ─────────────────────────────────────────────────────
// A cohesive cyan → blue → indigo → magenta → pink sweep; each state names a
// small subset so the field always reads the interaction state. Kept *muted* on
// purpose: the frosted glass over the field (Atmosphere, backdrop-filter
// saturate(1.7)) re-saturates it, so vivid canvas colours would read as neon.
const C = {
  cyan: [186, 70, 62] as HSL,
  blue: [214, 72, 64] as HSL,
  indigo: [250, 62, 68] as HSL,
  magenta: [296, 56, 66] as HSL,
  pink: [336, 66, 70] as HSL,
  slate: [214, 15, 66] as HSL, // offline — desaturated
};
const STATE_COLORS: Record<PresenceState, HSL[]> = {
  idle: [C.blue, C.indigo],        // resting — calm cool
  listening: [C.cyan, C.blue],     // human has the floor — bright cool
  thinking: [C.indigo, C.magenta], // cognition — purple
  speaking: [C.magenta, C.pink],   // agent voice — warm
  offline: [C.slate, C.slate],     // disconnected
  waking: [C.blue, C.blue],
};

/** The three pool colours for a state, flattened to 9 RGB components: born
 *  colour, drift colour, and a slightly brightened accent. The glass above turns
 *  these into soft watercolour blooms. Flat so the per-frame cross-fade and the
 *  uniform upload touch plain numbers (no tuple indexing, no allocation). */
function flatColors(s: PresenceState): Float32Array {
  const set = STATE_COLORS[s] ?? STATE_COLORS.idle;
  const a = set[0]!;
  const b = set[set.length - 1]!;
  const c0 = hslToRgb(...a);
  const c1 = hslToRgb(...b);
  const c2 = hslToRgb(b[0], b[1], Math.min(86, b[2] + 12));
  return new Float32Array([c0[0], c0[1], c0[2], c1[0], c1[1], c1[2], c2[0], c2[1], c2[2]]);
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

const VERT = `attribute vec2 p;void main(){gl_Position=vec4(p,0.0,1.0);}`;

// Watercolour pools: a few large, slowly drifting pigment blooms with soft
// feathered edges, melting into the neutral paper. Matte by design — no
// highlights; the frosted glass above does all the softening.
const FRAG = `precision highp float;
uniform vec2 uRes;
uniform float uTime, uPresence, uDemote;
uniform vec3 uA, uB, uC;
float hash(vec2 p){p=fract(p*vec2(123.34,456.21));p+=dot(p,p+45.32);return fract(p.x*p.y);}
float noise(vec2 p){vec2 i=floor(p),f=fract(p);float a=hash(i),b=hash(i+vec2(1,0)),c=hash(i+vec2(0,1)),d=hash(i+vec2(1,1));vec2 u=f*f*(3.-2.*f);return mix(mix(a,b,u.x),mix(c,d,u.x),u.y);}
float fbm(vec2 p){float v=0.,a=.5;for(int i=0;i<5;i++){v+=a*noise(p);p*=2.02;a*=.5;}return v;}
void main(){
  vec2 uv = gl_FragCoord.xy / uRes;
  float t = uTime;
  float asp = uRes.x / uRes.y;
  vec2 fp = uv + 0.05 * vec2(fbm(uv*3.0 + t), fbm(uv*3.0 + vec2(2.0) - t));
  vec2 q0 = vec2(0.26 + 0.12*sin(t*0.31),     0.30 + 0.10*cos(t*0.27));
  vec2 q1 = vec2(0.78 + 0.10*cos(t*0.24+1.0), 0.38 + 0.12*sin(t*0.29));
  vec2 q2 = vec2(0.40 + 0.14*sin(t*0.21+2.0), 0.74 + 0.10*cos(t*0.26));
  vec2 q3 = vec2(0.66 + 0.12*cos(t*0.27+3.0), 0.64 + 0.12*sin(t*0.23));
  vec2 a0=(fp-q0)*vec2(asp,1.0), a1=(fp-q1)*vec2(asp,1.0);
  vec2 a2=(fp-q2)*vec2(asp,1.0), a3=(fp-q3)*vec2(asp,1.0);
  float b0=exp(-dot(a0,a0)*4.0), b1=exp(-dot(a1,a1)*4.5);
  float b2=exp(-dot(a2,a2)*4.2), b3=exp(-dot(a3,a3)*4.8);
  vec3 paper = vec3(0.968, 0.974, 0.99);
  float P = uPresence;
  vec3 col = paper;
  col = mix(col, uA, clamp(b0,0.0,1.0)*0.85*P);
  col = mix(col, uB, clamp(b1,0.0,1.0)*0.85*P);
  col = mix(col, uC, clamp(b2,0.0,1.0)*0.80*P);
  col = mix(col, uA, clamp(b3,0.0,1.0)*0.78*P);
  // keep the centre calm for words as a content surface rises
  float centre = 1.0 - smoothstep(0.2, 0.85, length(uv - vec2(0.5, 0.52)));
  col = mix(col, paper, centre * uDemote * 0.55);
  gl_FragColor = vec4(col, 1.0);
}`;

const FPS = 30; // ambient field — 30fps is plenty and halves GPU work vs 60
const DPR_CAP = 1.75;
const SPEED = 0.32; // gentle drift

/**
 * The agent's presence — expressed entirely in the background.
 *
 * A neutral paper field behind frosted glass (Atmosphere), painted by a handful
 * of large, slowly drifting *watercolour pools*: soft pigment blooms with
 * feathered edges that bleed into one another and into the paper. Their colour
 * is sampled from the current interaction state (blue at rest, cyan while the
 * human holds the floor, violet while thinking, magenta while speaking) and
 * cross-fades on state change. Intensity rides live audio (`reactive`, via the
 * `AudioBus`) or the cognition cadence (`activity`), else a gentle synthetic
 * breath; `demote` recedes the centre when a surface is up so words stay legible.
 *
 * Rendered in WebGL (one full-screen fragment shader). Power-aware: capped at
 * 30fps and DPR ≤ 1.75, and paused entirely while the document is hidden.
 * Honours `prefers-reduced-motion` with a single static frame. Mounted once and
 * driven by refs so it never unmounts across state changes.
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

    const reduce = window.matchMedia("(prefers-reduced-motion: reduce)").matches;
    const DPR = Math.min(DPR_CAP, window.devicePixelRatio || 1);

    const gl = canvas.getContext("webgl", { antialias: false, alpha: false });
    if (!gl) {
      // No WebGL: paint a flat paper wash so the field isn't blank.
      const ctx = canvas.getContext("2d");
      if (ctx) {
        const g = ctx.createLinearGradient(0, 0, 0, canvas.clientHeight);
        g.addColorStop(0, "#f6f8fb");
        g.addColorStop(1, "#eef1f6");
        ctx.fillStyle = g;
        ctx.fillRect(0, 0, canvas.width, canvas.height);
      }
      return;
    }

    const compile = (type: number, src: string) => {
      const sh = gl.createShader(type)!;
      gl.shaderSource(sh, src);
      gl.compileShader(sh);
      if (!gl.getShaderParameter(sh, gl.COMPILE_STATUS)) {
        console.error("Presence shader:", gl.getShaderInfoLog(sh));
      }
      return sh;
    };
    const prog = gl.createProgram()!;
    gl.attachShader(prog, compile(gl.VERTEX_SHADER, VERT));
    gl.attachShader(prog, compile(gl.FRAGMENT_SHADER, FRAG));
    gl.linkProgram(prog);
    gl.useProgram(prog);

    const buf = gl.createBuffer();
    gl.bindBuffer(gl.ARRAY_BUFFER, buf);
    gl.bufferData(gl.ARRAY_BUFFER, new Float32Array([-1, -1, 3, -1, -1, 3]), gl.STATIC_DRAW);
    const loc = gl.getAttribLocation(prog, "p");
    gl.enableVertexAttribArray(loc);
    gl.vertexAttribPointer(loc, 2, gl.FLOAT, false, 0, 0);

    const uRes = gl.getUniformLocation(prog, "uRes");
    const uTime = gl.getUniformLocation(prog, "uTime");
    const uPresence = gl.getUniformLocation(prog, "uPresence");
    const uDemote = gl.getUniformLocation(prog, "uDemote");
    const uA = gl.getUniformLocation(prog, "uA");
    const uB = gl.getUniformLocation(prog, "uB");
    const uC = gl.getUniformLocation(prog, "uC");

    const resize = () => {
      const w = Math.round(canvas.clientWidth * DPR);
      const h = Math.round(canvas.clientHeight * DPR);
      if (canvas.width !== w || canvas.height !== h) {
        canvas.width = w;
        canvas.height = h;
      }
    };
    resize();
    const ro = new ResizeObserver(resize);
    ro.observe(canvas);

    let level = 0.1;
    const cur = flatColors(stateRef.current); // 9 RGB components, cross-faded in place
    const t0 = performance.now();

    const paint = (vt: number) => {
      const dem = demoteRef.current;
      const presence = (0.5 + level * 0.6) * (1 - dem * 0.6);
      gl.viewport(0, 0, canvas.width, canvas.height);
      gl.uniform2f(uRes, canvas.width, canvas.height);
      gl.uniform1f(uTime, vt);
      gl.uniform1f(uPresence, presence);
      gl.uniform1f(uDemote, dem);
      gl.uniform3f(uA, cur[0]!, cur[1]!, cur[2]!);
      gl.uniform3f(uB, cur[3]!, cur[4]!, cur[5]!);
      gl.uniform3f(uC, cur[6]!, cur[7]!, cur[8]!);
      gl.drawArrays(gl.TRIANGLES, 0, 3);
    };

    if (reduce) {
      // A single static frame so the field isn't blank, but no animation loop.
      level = synthLevel(stateRef.current, 0);
      paint(6.0);
      return () => ro.disconnect();
    }

    let raf = 0;
    let last = 0;
    let prev = 0;
    let vt = 0; // virtual (animation) time — drives drift, scaled by SPEED

    const frame = (now: number) => {
      raf = requestAnimationFrame(frame);
      if (document.hidden) { prev = now; return; } // pause when not visible
      if (now - last < 1000 / FPS - 1) return; // throttle to FPS
      const dt = prev ? (now - prev) / 1000 : 0;
      prev = now;
      last = now;
      vt += dt * SPEED;

      const t = (now - t0) / 1000;
      const st = stateRef.current;

      // Intensity target: live audio when reactive; else a synthetic breath
      // lifted by real cognition cadence while thinking/speaking.
      let target: number;
      const liveBus = busRef.current;
      if (liveBus && reactiveRef.current) {
        target = Math.min(0.6, 0.12 + liveBus.read().level * 0.42);
      } else {
        const synth = synthLevel(st, t);
        const pulse = activityRef.current ? activityRef.current.read() : 0;
        target =
          st === "thinking" || st === "speaking"
            ? Math.max(synth, 0.14 + pulse * 0.42)
            : synth;
      }
      level += (target - level) * 0.06; // heavy smoothing → calm

      // Cross-fade pool colours toward the current state's — so a state change
      // re-tints the field organically rather than snapping.
      const tgt = flatColors(st);
      for (let k = 0; k < 9; k++) {
        const c = cur[k]!;
        cur[k] = c + (tgt[k]! - c) * 0.05;
      }

      paint(vt);
    };
    raf = requestAnimationFrame(frame);

    return () => {
      cancelAnimationFrame(raf);
      ro.disconnect();
    };
  }, []);

  return <canvas ref={canvasRef} aria-hidden className="hi-presence" />;
}
