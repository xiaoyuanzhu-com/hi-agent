import { useEffect, useId, useMemo, useRef } from "react";

export type AgentState =
  | "idle"      // connecting OR live with nothing happening
  | "listening" // user is composing / mic open (placeholder)
  | "thinking"  // request sent, awaiting first chunk
  | "speaking"  // streaming chunks
  | "offline";  // /thought subscription failed

interface OrbProps {
  state: AgentState;
  /** 0..1 amplitude that drives wave deformation. Driven externally or by the orb's internal LFO. */
  intensity?: number;
}

/**
 * Central orb — the visual anchor of the whole shell.
 *
 * It is built from layered SVG primitives:
 *   * Radial-gradient halo behind everything (the "aura")
 *   * Three counter-rotating dashed rings
 *   * A morphing closed path drawn from N angle samples — the "skin",
 *     deformed by a low-frequency noise signal
 *   * Sound-wave concentric rings emitted while `state === "speaking"`
 *
 * State affects color, rotation speed, and noise amplitude. Always rendered;
 * never unmounts, so transitions look continuous.
 */
export function Orb({ state, intensity }: OrbProps) {
  const idBase = useId().replace(/:/g, "");
  const skinRef = useRef<SVGPathElement | null>(null);
  const innerRef = useRef<SVGPathElement | null>(null);
  const auraRef = useRef<SVGCircleElement | null>(null);

  // Single shared animation loop. Builds the deformed orb path each frame.
  const stateRef = useRef<AgentState>(state);
  const intensityRef = useRef<number>(intensity ?? 0);
  useEffect(() => {
    stateRef.current = state;
  }, [state]);
  useEffect(() => {
    if (typeof intensity === "number") intensityRef.current = intensity;
  }, [intensity]);

  useEffect(() => {
    const reduceMotion = window.matchMedia(
      "(prefers-reduced-motion: reduce)",
    ).matches;

    let raf = 0;
    let t0 = performance.now();

    const sampleCount = 64;
    const angles: { a: number; cos: number; sin: number }[] = [];
    for (let i = 0; i < sampleCount; i++) {
      const a = (i / sampleCount) * Math.PI * 2;
      angles.push({ a, cos: Math.cos(a), sin: Math.sin(a) });
    }

    const tick = (now: number) => {
      const t = (now - t0) / 1000;
      const s = stateRef.current;

      // Per-state shaping
      let amp = 0;
      let speedA = 0.7;
      let speedB = 1.3;
      switch (s) {
        case "idle":      amp = 1.6; speedA = 0.6; speedB = 1.1; break;
        case "listening": amp = 2.4; speedA = 1.0; speedB = 1.6; break;
        case "thinking":  amp = 3.2; speedA = 1.4; speedB = 2.0; break;
        case "speaking":  amp = 5.0; speedA = 1.8; speedB = 2.6; break;
        case "offline":   amp = 0.6; speedA = 0.3; speedB = 0.4; break;
      }
      // External intensity (e.g. from streaming text rate) layers on top.
      amp += intensityRef.current * 4.5;

      const baseR = 86; // orb radius in SVG units (viewBox 0..200, center 100)
      const cx = 100;
      const cy = 100;

      // Build a closed path from angle samples, perturbed by two sine waves.
      let d = "";
      for (let i = 0; i < sampleCount; i++) {
        const { a, cos, sin } = angles[i]!;
        const wob =
          Math.sin(a * 3 + t * speedA) * amp +
          Math.cos(a * 5 - t * speedB) * (amp * 0.55) +
          Math.sin(a * 7 + t * speedA * 0.7) * (amp * 0.25);
        const r = baseR + wob;
        const x = cx + cos * r;
        const y = cy + sin * r;
        d += i === 0 ? `M${x.toFixed(2)} ${y.toFixed(2)}` : `L${x.toFixed(2)} ${y.toFixed(2)}`;
      }
      d += "Z";

      if (skinRef.current) skinRef.current.setAttribute("d", d);

      // Inner skin: tighter, slower morph, smaller radius
      const innerR = baseR * 0.62;
      let di = "";
      for (let i = 0; i < sampleCount; i++) {
        const { a, cos, sin } = angles[i]!;
        const wob =
          Math.sin(a * 4 - t * speedA * 0.9) * (amp * 0.4) +
          Math.cos(a * 6 + t * speedB * 0.6) * (amp * 0.25);
        const r = innerR + wob;
        const x = cx + cos * r;
        const y = cy + sin * r;
        di += i === 0 ? `M${x.toFixed(2)} ${y.toFixed(2)}` : `L${x.toFixed(2)} ${y.toFixed(2)}`;
      }
      di += "Z";
      if (innerRef.current) innerRef.current.setAttribute("d", di);

      // Aura breathing — radius scales with intensity + slow sine.
      if (auraRef.current) {
        const breath = 1 + Math.sin(t * 0.8) * 0.04 + intensityRef.current * 0.12;
        auraRef.current.setAttribute("r", String(120 * breath));
      }

      if (!reduceMotion) raf = requestAnimationFrame(tick);
    };
    raf = requestAnimationFrame(tick);
    return () => cancelAnimationFrame(raf);
  }, []);

  const palette = useMemo(() => {
    switch (state) {
      case "offline":
        return { core: "#ff5d7a", glow: "#ff4ecb", ring: "#ff4ecb" };
      case "thinking":
        return { core: "#ffb469", glow: "#ff9b3f", ring: "#ffb469" };
      case "listening":
        return { core: "#75ffd0", glow: "#5af6ff", ring: "#75ffd0" };
      case "speaking":
        return { core: "#5af6ff", glow: "#5af6ff", ring: "#9cf2ff" };
      case "idle":
      default:
        return { core: "#5af6ff", glow: "#5af6ff", ring: "#7afbff" };
    }
  }, [state]);

  return (
    <div
      aria-hidden
      style={{
        position: "absolute",
        inset: 0,
        display: "grid",
        placeItems: "center",
        pointerEvents: "none",
        zIndex: -1,
      }}
    >
      <div
        style={{
          position: "relative",
          width: "min(58vmin, 620px)",
          aspectRatio: "1 / 1",
        }}
      >
        {/* Sound-wave rings: render four; CSS animation handles expansion.
            We toggle with data-state to start/stop emission. */}
        {[0, 1, 2, 3].map((i) => (
          <div
            key={i}
            data-state={state}
            style={{
              position: "absolute",
              inset: "12%",
              border: `1px solid ${palette.ring}`,
              borderRadius: "50%",
              opacity: 0,
              boxShadow: `0 0 24px ${palette.ring}66`,
              animation: `hi-ring-expand 2.6s ${i * 0.65}s infinite ease-out`,
              animationPlayState: state === "speaking" ? "running" : "paused",
            }}
          />
        ))}

        <svg
          viewBox="0 0 200 200"
          width="100%"
          height="100%"
          style={{ overflow: "visible", display: "block" }}
        >
          <defs>
            <radialGradient id={`g-aura-${idBase}`} cx="50%" cy="50%" r="50%">
              <stop offset="0%" stopColor={palette.glow} stopOpacity="0.55" />
              <stop offset="55%" stopColor={palette.glow} stopOpacity="0.12" />
              <stop offset="100%" stopColor={palette.glow} stopOpacity="0" />
            </radialGradient>
            <radialGradient id={`g-core-${idBase}`} cx="50%" cy="42%" r="58%">
              <stop offset="0%" stopColor="#ffffff" stopOpacity="0.95" />
              <stop offset="22%" stopColor={palette.core} stopOpacity="0.9" />
              <stop offset="70%" stopColor="#0b1a3a" stopOpacity="0.8" />
              <stop offset="100%" stopColor="#020514" stopOpacity="1" />
            </radialGradient>
            <radialGradient id={`g-inner-${idBase}`} cx="50%" cy="50%" r="50%">
              <stop offset="0%" stopColor={palette.core} stopOpacity="0.9" />
              <stop offset="60%" stopColor={palette.core} stopOpacity="0.18" />
              <stop offset="100%" stopColor={palette.core} stopOpacity="0" />
            </radialGradient>
            <filter id={`f-glow-${idBase}`} x="-50%" y="-50%" width="200%" height="200%">
              <feGaussianBlur stdDeviation="3" result="b" />
              <feMerge>
                <feMergeNode in="b" />
                <feMergeNode in="SourceGraphic" />
              </feMerge>
            </filter>
          </defs>

          {/* Aura halo */}
          <circle
            ref={auraRef}
            cx="100"
            cy="100"
            r="120"
            fill={`url(#g-aura-${idBase})`}
          />

          {/* Outer dashed ring — slow spin */}
          <g style={{ transformOrigin: "100px 100px", animation: state === "offline" ? "none" : "hi-spin 22s linear infinite" }}>
            <circle
              cx="100"
              cy="100"
              r="98"
              fill="none"
              stroke={palette.ring}
              strokeOpacity="0.42"
              strokeWidth="0.6"
              strokeDasharray="2 6"
            />
          </g>

          {/* Mid dashed ring — counter-spin */}
          <g style={{ transformOrigin: "100px 100px", animation: state === "offline" ? "none" : "hi-spin 14s linear infinite reverse" }}>
            <circle
              cx="100"
              cy="100"
              r="90"
              fill="none"
              stroke={palette.ring}
              strokeOpacity="0.55"
              strokeWidth="0.4"
              strokeDasharray="1 4"
            />
          </g>

          {/* Skin — morphing outline */}
          <path
            ref={skinRef}
            d=""
            fill="none"
            stroke={palette.ring}
            strokeOpacity="0.85"
            strokeWidth="0.9"
            filter={`url(#f-glow-${idBase})`}
          />

          {/* Core sphere */}
          <circle cx="100" cy="100" r="80" fill={`url(#g-core-${idBase})`} />

          {/* Inner skin — second morph, fills core with cyan tone */}
          <path
            ref={innerRef}
            d=""
            fill={`url(#g-inner-${idBase})`}
            opacity="0.85"
          />

          {/* Specular highlight */}
          <ellipse
            cx="78"
            cy="68"
            rx="22"
            ry="10"
            fill="#ffffff"
            opacity="0.18"
          />
        </svg>
      </div>
    </div>
  );
}
