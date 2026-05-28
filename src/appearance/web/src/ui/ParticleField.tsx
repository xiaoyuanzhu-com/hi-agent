import { useEffect, useRef } from "react";

/**
 * Full-bleed particle field rendered into a single <canvas>.
 *
 * Render strategy:
 *   * One canvas pinned behind everything else (position: fixed, z = -2).
 *   * ~120 particles depending on viewport area, capped to keep frame budget.
 *   * Slow drift; depth (z) scales size + alpha for parallax feel.
 *   * Faint cyan links between particles within a short radius.
 *   * Respects prefers-reduced-motion (renders one static frame).
 */

interface Particle {
  x: number;
  y: number;
  z: number;       // 0.2 .. 1
  vx: number;
  vy: number;
  hue: number;     // shifts particles between cyan / magenta hints
}

export function ParticleField() {
  const canvasRef = useRef<HTMLCanvasElement | null>(null);

  useEffect(() => {
    const canvas = canvasRef.current;
    if (!canvas) return;
    const ctx = canvas.getContext("2d", { alpha: true });
    if (!ctx) return;

    let dpr = Math.min(window.devicePixelRatio || 1, 2);
    let width = 0;
    let height = 0;
    let particles: Particle[] = [];
    let raf = 0;

    const reduceMotion = window.matchMedia(
      "(prefers-reduced-motion: reduce)",
    ).matches;

    const seed = (count: number) => {
      particles = new Array(count).fill(0).map(() => ({
        x: Math.random() * width,
        y: Math.random() * height,
        z: 0.2 + Math.random() * 0.8,
        vx: (Math.random() - 0.5) * 0.18,
        vy: (Math.random() - 0.5) * 0.18,
        hue: Math.random() < 0.85 ? 188 : 312, // cyan dominant, magenta accent
      }));
    };

    const resize = () => {
      dpr = Math.min(window.devicePixelRatio || 1, 2);
      width = window.innerWidth;
      height = window.innerHeight;
      canvas.width = Math.floor(width * dpr);
      canvas.height = Math.floor(height * dpr);
      canvas.style.width = `${width}px`;
      canvas.style.height = `${height}px`;
      ctx.setTransform(dpr, 0, 0, dpr, 0, 0);
      // density scales with area but capped
      const density = Math.min(160, Math.max(60, Math.floor((width * height) / 16000)));
      seed(density);
    };

    const draw = () => {
      ctx.clearRect(0, 0, width, height);

      // Radial vignette gradient backdrop. Slight bias toward top center
      // so the orb feels "lit from above".
      const grad = ctx.createRadialGradient(
        width * 0.5,
        height * 0.42,
        Math.min(width, height) * 0.05,
        width * 0.5,
        height * 0.55,
        Math.max(width, height) * 0.85,
      );
      grad.addColorStop(0, "rgba(20, 40, 80, 0.55)");
      grad.addColorStop(0.45, "rgba(10, 16, 36, 0.4)");
      grad.addColorStop(1, "rgba(4, 6, 13, 1)");
      ctx.fillStyle = grad;
      ctx.fillRect(0, 0, width, height);

      // Particles
      for (const p of particles) {
        p.x += p.vx * p.z;
        p.y += p.vy * p.z;
        if (p.x < -8) p.x = width + 8;
        if (p.x > width + 8) p.x = -8;
        if (p.y < -8) p.y = height + 8;
        if (p.y > height + 8) p.y = -8;

        const r = 0.6 + p.z * 1.8;
        const alpha = 0.15 + p.z * 0.55;
        ctx.beginPath();
        ctx.fillStyle = `hsla(${p.hue}, 95%, 70%, ${alpha})`;
        ctx.arc(p.x, p.y, r, 0, Math.PI * 2);
        ctx.fill();
      }

      // Faint connections — only between near-foreground points.
      const linkDist = 110;
      const linkDist2 = linkDist * linkDist;
      ctx.lineWidth = 1;
      for (let i = 0; i < particles.length; i++) {
        const a = particles[i]!;
        if (a.z < 0.55) continue;
        for (let j = i + 1; j < particles.length; j++) {
          const b = particles[j]!;
          if (b.z < 0.55) continue;
          const dx = a.x - b.x;
          const dy = a.y - b.y;
          const d2 = dx * dx + dy * dy;
          if (d2 > linkDist2) continue;
          const t = 1 - d2 / linkDist2;
          const alpha = t * 0.12 * Math.min(a.z, b.z);
          ctx.strokeStyle = `rgba(120, 200, 255, ${alpha})`;
          ctx.beginPath();
          ctx.moveTo(a.x, a.y);
          ctx.lineTo(b.x, b.y);
          ctx.stroke();
        }
      }

      if (!reduceMotion) raf = requestAnimationFrame(draw);
    };

    resize();
    draw();
    window.addEventListener("resize", resize);
    return () => {
      window.removeEventListener("resize", resize);
      cancelAnimationFrame(raf);
    };
  }, []);

  return (
    <canvas
      ref={canvasRef}
      aria-hidden
      style={{
        position: "fixed",
        inset: 0,
        zIndex: -2,
        pointerEvents: "none",
        background: "var(--bg-0)",
      }}
    />
  );
}
