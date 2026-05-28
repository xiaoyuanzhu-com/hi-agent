import { useEffect, useRef } from "react";

interface WaveformProps {
  /** Visual "energy" of the waveform. 0..1. When > 0 bars dance live. */
  intensity: number;
  /** Number of bars. */
  bars?: number;
  /** Total width (CSS units). Bars distribute evenly. */
  width?: number | string;
  /** Height in px. */
  height?: number;
  /** Stroke/fill color. */
  color?: string;
  /** When provided, renders this static set of normalized heights (0..1).
   *  Useful for "memory" waveforms attached to a past message. */
  staticBars?: number[];
  ariaLabel?: string;
}

/**
 * Symmetric spectrum-style bars.
 *
 * Live mode: bars animate via a pseudo-LFO driven by `intensity`. Each bar
 * picks its own phase so the field reads like a real spectrum, not a sine.
 *
 * Static mode (`staticBars` provided): bars hold the given heights — used
 * to attach a frozen waveform to a logged transcript entry.
 *
 * This is a placeholder for the not-yet-implemented /audio channel; once
 * audio lands, feed real FFT magnitudes via `staticBars` per frame.
 */
export function Waveform({
  intensity,
  bars = 28,
  width = 220,
  height = 36,
  color = "var(--cyan)",
  staticBars,
  ariaLabel,
}: WaveformProps) {
  const rootRef = useRef<HTMLDivElement | null>(null);
  const intensityRef = useRef<number>(intensity);
  useEffect(() => {
    intensityRef.current = intensity;
  }, [intensity]);

  // Stable per-bar phase offsets so each bar reads independently.
  const phasesRef = useRef<number[]>([]);
  if (phasesRef.current.length !== bars) {
    phasesRef.current = new Array(bars)
      .fill(0)
      .map((_, i) => (i * 137.508) % (Math.PI * 2));
  }

  useEffect(() => {
    if (staticBars) return; // static mode → no RAF
    const root = rootRef.current;
    if (!root) return;
    let raf = 0;
    let t0 = performance.now();

    const reduceMotion = window.matchMedia(
      "(prefers-reduced-motion: reduce)",
    ).matches;

    const tick = (now: number) => {
      const t = (now - t0) / 1000;
      const energy = intensityRef.current;
      const children = root.children;
      for (let i = 0; i < children.length; i++) {
        const phase = phasesRef.current[i] ?? 0;
        // base hum so it never dies completely, scaled by energy
        const base = 0.12 + energy * 0.4;
        const wob =
          Math.sin(t * 4.5 + phase) * 0.35 +
          Math.sin(t * 2.1 + phase * 1.7) * 0.2;
        const h = Math.max(0.05, Math.min(1, base + wob * (0.35 + energy * 0.55)));
        (children[i] as HTMLElement).style.transform = `scaleY(${h.toFixed(3)})`;
      }
      if (!reduceMotion) raf = requestAnimationFrame(tick);
    };
    raf = requestAnimationFrame(tick);
    return () => cancelAnimationFrame(raf);
  }, [staticBars]);

  const renderBars: number[] = staticBars
    ? staticBars.slice(0, bars).concat(new Array(Math.max(0, bars - staticBars.length)).fill(0.1))
    : new Array(bars).fill(0.2);

  return (
    <div
      ref={rootRef}
      role="img"
      aria-label={ariaLabel ?? "waveform"}
      style={{
        display: "inline-flex",
        alignItems: "center",
        justifyContent: "space-between",
        gap: 2,
        width,
        height,
      }}
    >
      {renderBars.map((h, i) => (
        <span
          key={i}
          style={{
            display: "block",
            flex: 1,
            height: "100%",
            background: color,
            borderRadius: 2,
            transformOrigin: "center",
            transform: `scaleY(${h})`,
            opacity: 0.7,
            boxShadow: `0 0 6px ${color}66`,
            transition: staticBars ? "none" : undefined,
          }}
        />
      ))}
    </div>
  );
}
