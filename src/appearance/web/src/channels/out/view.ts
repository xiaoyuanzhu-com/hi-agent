// Client for the outbound view channel — a scene's retained appearance state.
//
// GET /api/out/view serves the scene's whole appearance (active views in
// z-order, plus a version) and long-polls on `?since=<version>`: the first
// request returns the current state immediately (even when empty), each
// following one is held until the state changes. Refresh, a second device, or
// a server restart all converge on the same screen — the server retains and
// persists the state; the client just mirrors the latest snapshot.

/** One active view in the scene's appearance, in z-order (first = bottom). */
export interface WireView {
  id: string;
  /** URL of the compiled ESM module to import and mount under `id`. */
  module_url: string;
  /** Remaining lifetime at response time, when the view carries a TTL. */
  ttl_ms?: number;
}

/** A scene's full appearance state — one GET /api/out/view response. */
export interface ViewState {
  version: number;
  views: WireView[];
}

export interface SubscribeViewOpts {
  scene: string;
  signal: AbortSignal;
}

export async function* subscribeViewState(
  opts: SubscribeViewOpts,
): AsyncGenerator<ViewState, void, void> {
  let since: number | undefined;
  while (!opts.signal.aborted) {
    const query = since === undefined ? "" : `?since=${since}`;
    const res = await fetch(`/api/out/view${query}`, {
      method: "GET",
      headers: { "X-HI-Scene": opts.scene, Accept: "application/json" },
      signal: opts.signal,
      cache: "no-store",
    });
    if (!res.ok) {
      throw new Error(`/api/out/view subscribe failed: ${res.status} ${res.statusText}`);
    }
    const state = (await res.json()) as ViewState;
    if (!state || !Array.isArray(state.views)) continue;
    since = state.version;
    yield state;
  }
}
