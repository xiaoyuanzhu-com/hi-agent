// Client for the inbound audio channel.
//
// "Audio is audio": the inbound audio channel carries audio bytes, not text. Live
// mic input streams continuously over the `/api/in/audio/stream` WebSocket (see
// `lib/audioStreamer`), upload-only; the server transcribes it and posts the
// recognized text to the *text* channel (observe via `/api/in/text`).
//
// `subscribeInAudioTurns` observes the raw audio bytes on `GET /api/in/audio` —
// one source (a mic stream or a posted clip) per response, the inbound mirror of
// `out/audio`. The `Content-Type` tells the caller how to decode each turn
// (`audio/pcm;rate=16000;channels=1` for the live mic, the clip's own type for a
// posted clip). Used to *listen in* on a scene's audio (e.g. the inspector).
//
// `postAudio` is the one-shot batch path for `POST /api/in/audio` — kept for
// non-streaming callers (e.g. uploading a finished clip); not used by the live mic.

/** One source of inbound audio: a continuous byte body the caller plays. */
export interface AudioInTurn {
  /** Content-Type for the whole source (set from the response headers). */
  mime: string;
  /** The source's audio bytes, streamed as they arrive. */
  body: ReadableStream<Uint8Array>;
}

/**
 * Observe the live audio bytes on this scene. Each `GET /api/in/audio` response
 * is one source — a continuous stream — so we yield one `AudioInTurn` per
 * response and re-subscribe for the next. The generator pauses at each `yield`
 * until the caller finishes consuming the body, so only one source is in flight
 * at a time.
 */
export async function* subscribeInAudioTurns(opts: {
  scene: string;
  signal: AbortSignal;
}): AsyncGenerator<AudioInTurn, void, void> {
  while (!opts.signal.aborted) {
    const res = await fetch("/api/in/audio", {
      method: "GET",
      headers: { "X-HI-Scene": opts.scene, Accept: "audio/*" },
      signal: opts.signal,
      cache: "no-store",
    });
    if (!res.ok) {
      throw new Error(`/api/in/audio subscribe failed: ${res.status} ${res.statusText}`);
    }
    if (!res.body) continue;
    const mime = res.headers.get("content-type") ?? "application/octet-stream";
    yield { mime, body: res.body };
  }
}

export async function postAudio(opts: {
  scene: string;
  blob: Blob;
  mime: string;
  signal?: AbortSignal;
}): Promise<{ transcript: string; media_path: string }> {
  const res = await fetch("/api/in/audio", {
    method: "POST",
    headers: {
      "Content-Type": opts.mime,
      "X-HI-Scene": opts.scene,
    },
    body: opts.blob,
    signal: opts.signal,
  });
  if (!res.ok) {
    const detail = await res.text().catch(() => "");
    throw new Error(
      `/api/in/audio POST failed: ${res.status} ${res.statusText}${detail ? ` — ${detail.trim()}` : ""}`,
    );
  }
  return (await res.json()) as { transcript: string; media_path: string };
}
