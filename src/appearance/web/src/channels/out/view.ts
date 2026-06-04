// Client for the outbound view channel (agent-authored view modules).
//
// GET /api/out/view is a long-poll that returns one envelope per response (like
// /api/out/surface did); we re-subscribe after each. The agent produces these by
// wrapping JSX in `[[view id= op=]] … [[/view]]`; the reactor compiles the source
// server-side and routes the resulting module URL here for the client to import.

export type ViewOp = "show" | "replace" | "dismiss";

export interface ViewEnvelope {
  id: string;
  op: ViewOp;
  /** URL of the compiled ESM module to import and mount (absent for dismiss). */
  module_url?: string;
  ttl_ms?: number;
}

export interface SubscribeViewOpts {
  scene: string;
  signal: AbortSignal;
}

export async function* subscribeView(
  opts: SubscribeViewOpts,
): AsyncGenerator<ViewEnvelope, void, void> {
  while (!opts.signal.aborted) {
    const res = await fetch("/api/out/view", {
      method: "GET",
      headers: { "X-HI-Scene": opts.scene, Accept: "application/json" },
      signal: opts.signal,
      cache: "no-store",
    });
    if (!res.ok) {
      throw new Error(`/api/out/view subscribe failed: ${res.status} ${res.statusText}`);
    }
    const env = (await res.json()) as ViewEnvelope;
    if (env && env.id) yield env;
  }
}
