// Client for the human-interface /surface channel (outbound rich content).
//
// GET /surface is a long-poll that returns one envelope per response (like
// /audio); we re-subscribe after each. The agent produces these by wrapping
// HTML in `[[surface:…]] … [[/surface]]`; the reactor strips them from the
// spoken text and routes them here.

export type SurfaceMode = "card" | "full";

export interface SurfaceEnvelope {
  id: string;
  op: "show" | "dismiss";
  mode?: SurfaceMode;
  /** Self-contained HTML, rendered in a sandboxed iframe. */
  html?: string;
  ttl_ms?: number;
}

export interface SubscribeSurfaceOpts {
  scene: string;
  signal: AbortSignal;
}

export async function* subscribeSurface(
  opts: SubscribeSurfaceOpts,
): AsyncGenerator<SurfaceEnvelope, void, void> {
  while (!opts.signal.aborted) {
    const res = await fetch("/api/surface", {
      method: "GET",
      headers: { "X-HI-Scene": opts.scene, Accept: "application/json" },
      signal: opts.signal,
      cache: "no-store",
    });
    if (!res.ok) {
      throw new Error(`/surface subscribe failed: ${res.status} ${res.statusText}`);
    }
    const env = (await res.json()) as SurfaceEnvelope;
    if (env && env.id) yield env;
  }
}
