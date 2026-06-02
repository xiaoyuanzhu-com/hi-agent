// Shared client for the `/api/in/*` observe streams.
//
// Each input boundary (`GET /api/in/text`, `GET /api/in/audio`) is a single
// long-lived response of newline-delimited JSON — one recognized input per line.
// Unlike the outbound channels (one item per GET, reconnect), partials arrive
// too fast for a reconnect-per-item loop, so the connection stays open and we
// parse lines off it. It is live presence, not history: the server never
// replays, so a fresh connection starts at the live edge.

/** One recognized input, as echoed by the server. */
export interface InputEvent {
  text: string;
  /** False while a rolling partial (live STT); true once the utterance settles. */
  final: boolean;
  /** The originating channel ("text" | "audio" | …); present but rarely needed. */
  channel?: string;
}

/** Parse a fetch Response body as NDJSON, yielding one parsed object per line. */
async function* readNdjson<T>(res: Response, signal: AbortSignal): AsyncGenerator<T, void, void> {
  if (!res.body) return;
  const reader = res.body.getReader();
  const decoder = new TextDecoder("utf-8");
  let buf = "";
  try {
    while (!signal.aborted) {
      const { value, done } = await reader.read();
      if (done) return;
      buf += decoder.decode(value, { stream: true });
      let nl: number;
      while ((nl = buf.indexOf("\n")) >= 0) {
        const line = buf.slice(0, nl).trim();
        buf = buf.slice(nl + 1);
        if (line.length === 0) continue;
        try {
          yield JSON.parse(line) as T;
        } catch {
          /* skip a malformed line rather than tearing down the stream */
        }
      }
    }
  } finally {
    try {
      reader.releaseLock();
    } catch {
      /* ignore */
    }
  }
}

/**
 * Open one observe stream against `path` and yield each input as it arrives.
 * Returns when the server closes the body; the caller reconnects (the loop in
 * the hook does this, same shape as the outbound subscriptions).
 */
export async function* observeInput(
  path: string,
  opts: { scene: string; signal: AbortSignal },
): AsyncGenerator<InputEvent, void, void> {
  const res = await fetch(path, {
    method: "GET",
    headers: { "X-HI-Scene": opts.scene, Accept: "application/x-ndjson" },
    signal: opts.signal,
    cache: "no-store",
  });
  if (!res.ok) {
    throw new Error(`${path} observe failed: ${res.status} ${res.statusText}`);
  }
  yield* readNdjson<InputEvent>(res, opts.signal);
}
