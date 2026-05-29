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
  /** When true the dots track live audio (mic, or — Phase 2 — TTS playback);
   *  otherwise they breathe on a synthetic envelope. */
  reactive?: boolean;
  /** 0..1 — how much a content overlay is up; the presence dims as this rises. */
  demote?: number;
}

interface Palette {
  primary: string;
  hot: string;
  dim: string;
}

function paletteFor(state: PresenceState): Palette {
  switch (state) {
    case "offline":   return { primary: "#6b7da6", hot: "#9fb0d6", dim: "#3b475f" };
    case "listening": return { primary: "#7ad7ff", hot: "#cdeeff", dim: "#26354f" };
    case "speaking":  return { primary: "#5af6ff", hot: "#a8fbff", dim: "#24405a" };
    case "thinking":  return { primary: "#9b8cff", hot: "#c7bcff", dim: "#2c2c54" };
    default:          return { primary: "#5af6ff", hot: "#a8fbff", dim: "#21354f" };
  }
}

/**
 * The agent's presence — a dot-matrix radial spectrogram on a canvas.
 *
 * Distance from center maps to pitch, brightness to energy. While listening or
 * speaking it reads the live `AudioBus` (the user's mic, or — Phase 2 — the
 * agent's TTS voice); otherwise it breathes on a gentle synthetic envelope.
 * Mounted once and driven by refs so it never unmounts across state changes.
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
    const NB = busRef.current?.bandCount ?? 56;
    const bandsSmooth = new Float32Array(NB);
    let levelSmooth = 0;
    let W = 0;
    let H = 0;

    const resize = () => {
      W = canvas.clientWidth;
      H = canvas.clientHeight;
      canvas.width = Math.round(W * DPR);
      canvas.height = Math.round(H * DPR);
      ctx.setTransform(DPR, 0, 0, DPR, 0, 0);
    };
    resize();
    const ro = new ResizeObserver(resize);
    ro.observe(canvas);

    let raf = 0;
    const t0 = performance.now();

    const tick = (now: number) => {
      const t = (now - t0) / 1000;
      const liveBus = busRef.current;
      const st = stateRef.current;
      const dem = demoteRef.current;

      let level = 0;
      let bands: Float32Array | null = null;
      if (liveBus && reactiveRef.current) {
        const r = liveBus.read();
        level = r.level;
        bands = r.bands;
      } else {
        const breathe = Math.sin(t * 0.6) * 0.5 + 0.5;
        level = st === "thinking" ? 0.12 + breathe * 0.08 : 0.04 + breathe * 0.05;
      }

      const sm = reduce ? 0 : 0.78;
      levelSmooth = levelSmooth * sm + level * (1 - sm);
      for (let b = 0; b < NB; b++) {
        const target = bands ? bands[b]! : 0;
        bandsSmooth[b] = bandsSmooth[b]! * sm + target * (1 - sm);
      }

      ctx.clearRect(0, 0, W, H);
      const dim = 1 - dem * 0.82;
      const pal = paletteFor(st);
      const step = 30;
      const cols = Math.floor(W / step);
      const rows = Math.floor(H / step);
      const ox = (W - (cols - 1) * step) / 2;
      const oy = (H - (rows - 1) * step) / 2;
      const cx = W / 2;
      const cy = H / 2;
      const md = Math.min(W, H) * 0.52;

      for (let r = 0; r < rows; r++) {
        for (let c = 0; c < cols; c++) {
          const x = ox + c * step;
          const y = oy + r * step;
          const d = Math.min(1, Math.hypot(x - cx, y - cy) / md);
          const bi = Math.min(NB - 1, Math.max(0, (d * NB) | 0));
          const idle = (Math.sin(d * 7 - t * 1.5) * 0.5 + 0.5) * 0.05;
          const energy = bandsSmooth[bi]! * (0.5 + levelSmooth * 0.7);
          const b = Math.max(idle, Math.min(1, energy)) * dim;

          if (b <= 0.02) {
            ctx.globalAlpha = 0.05 * dim;
            ctx.fillStyle = pal.dim;
            ctx.shadowBlur = 0;
            ctx.beginPath();
            ctx.arc(x, y, 1, 0, 6.2832);
            ctx.fill();
            continue;
          }
          ctx.globalAlpha = 0.12 + b * 0.88;
          ctx.fillStyle = b > 0.62 ? pal.hot : pal.primary;
          ctx.shadowColor = pal.primary;
          ctx.shadowBlur = b * 11;
          ctx.beginPath();
          ctx.arc(x, y, 1 + b * 4.4, 0, 6.2832);
          ctx.fill();
        }
      }
      ctx.globalAlpha = 1;
      ctx.shadowBlur = 0;

      if (!reduce) raf = requestAnimationFrame(tick);
    };
    raf = requestAnimationFrame(tick);

    return () => {
      cancelAnimationFrame(raf);
      ro.disconnect();
    };
  }, []);

  return (
    <canvas
      ref={canvasRef}
      aria-hidden
      style={{ position: "absolute", inset: 0, width: "100%", height: "100%", display: "block" }}
    />
  );
}
