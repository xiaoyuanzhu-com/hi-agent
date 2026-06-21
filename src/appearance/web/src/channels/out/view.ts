// Client for the outbound view channel — a scene's retained appearance state.
//
// GET /api/out/view serves the scene's whole appearance (active views in
// z-order, plus a version) and long-polls on `?since=<version>`: the first
// request returns the current state immediately (even when empty), each
// following one is held until the state changes. Refresh, a second device, or
// a server restart all converge on the same screen — the server retains and
// persists the state; the client just mirrors the latest snapshot.

/** Where a participant's content sits on the stage. Mirrors the Rust `Region`
 * (snake_case on the wire). `fill` owns the whole frame and its own background. */
export type Region =
  | "center"
  | "top"
  | "bottom"
  | "left"
  | "right"
  | "top_left"
  | "top_right"
  | "bottom_left"
  | "bottom_right"
  | "fill";

/** How big a participant's content wants to be. Mirrors the Rust `SizeClass`. */
export type SizeClass = "compact" | "auto" | "wide" | "fill";

/** A view's declared placement, carried verbatim from the Rust `Geometry`. Every
 * field is optional — an absent geometry (or field) reads as the floor layout
 * (centered, auto-sized, host-owned captions). Field names match the wire. */
export interface Geometry {
  region?: Region;
  size?: SizeClass;
  owns_captions?: boolean;
}

/** One active view in the scene's appearance, in z-order (first = bottom). */
export interface WireView {
  id: string;
  /** URL of the compiled ESM module to import and mount under `id`. */
  module_url: string;
  /** Declared placement; absent = the client's floor layout. */
  geometry?: Geometry;
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

/** Clear the scene's appearance — close all views, back to the default room.
 * The server bumps the version, so every device's long-poll converges on empty
 * (there is no optimistic local change; the next snapshot drives the UI). */
export async function clearViewState(scene: string): Promise<void> {
  const res = await fetch("/api/out/view", {
    method: "DELETE",
    headers: { "X-HI-Scene": scene },
  });
  if (!res.ok) {
    throw new Error(`/api/out/view clear failed: ${res.status} ${res.statusText}`);
  }
}
