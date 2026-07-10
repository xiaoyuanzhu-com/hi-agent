// Client for the attention lane.
//
// `reportAttention` tells the backend this window just came forward — became
// visible or regained focus (POST /api/in/attention). It's the first-party
// "they're checking on you" signal for presence: strictly about *our own*
// window, never anything about other apps. Fire-and-forget; a failed beat is
// harmless and swallowed.

/** Report that our window was activated (became visible / focused). */
export async function reportAttention(opts: {
  scene: string;
  signal?: AbortSignal;
}): Promise<void> {
  try {
    await fetch("/api/in/attention", {
      method: "POST",
      headers: { "X-HI-Scene": opts.scene },
      signal: opts.signal,
    });
  } catch {
    /* a dropped heartbeat is harmless — presence just misses one activation */
  }
}
