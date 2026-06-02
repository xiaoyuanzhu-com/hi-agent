// Subscriber for the outbound audio channel — the agent's synthesized voice.

export interface SubscribeAudioOpts {
  /** Scene we receive on (sent as X-HI-Scene). */
  scene: string;
  signal: AbortSignal;
}

/** One turn of speech: a continuous audio body the caller streams to playback. */
export interface AudioTurn {
  /** Content-Type for the whole turn (set from the stream's first event). */
  mime: string;
  /** The turn's audio bytes, streamed as they're synthesized. */
  body: ReadableStream<Uint8Array>;
}

/**
 * Outbound TTS. Each GET /api/out/audio response is one whole turn — a
 * continuous stream the backend synthesizes as one session — so we yield one
 * `AudioTurn` per response and re-subscribe for the next. The caller streams
 * `body` into playback (no per-clip reassembly). Turn-taking is decided
 * server-side; the client only renders, so there's no turn metadata to read here.
 *
 * The generator pauses at each `yield` until the caller has finished consuming
 * the turn's body, so only one turn is in flight at a time.
 */
export async function* subscribeAudioTurns(
  opts: SubscribeAudioOpts,
): AsyncGenerator<AudioTurn, void, void> {
  while (!opts.signal.aborted) {
    const res = await fetch("/api/out/audio", {
      method: "GET",
      headers: { "X-HI-Scene": opts.scene, Accept: "audio/*" },
      signal: opts.signal,
      cache: "no-store",
    });
    if (!res.ok) {
      throw new Error(`/api/out/audio subscribe failed: ${res.status} ${res.statusText}`);
    }
    if (!res.body) continue;
    const mime = res.headers.get("content-type") ?? "audio/mpeg";
    yield { mime, body: res.body };
  }
}
