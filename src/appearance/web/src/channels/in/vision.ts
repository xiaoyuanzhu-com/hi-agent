// Client for the inbound vision channel (the live camera).
//
// "Vision is video": the camera streams continuously as WebM over the WS (see
// `lib/videoStreamer`), upload-only; the backend relays the bytes and decides how
// much to look. There is no client-side sampling.
//
// `subscribeInVideo` is the read side: `GET /api/in/vision` long-polls one camera
// session per response (the live video bytes), so we yield a session and re-GET
// for the next — the same shape `out/audio` uses for TTS turns. Used to watch a
// scene's camera (e.g. the inspector); each session's bytes go to a MediaSource.

/** One camera session: a continuous video byte body the caller plays. */
export interface VideoInTurn {
  /** Content-Type for the whole session (the recorder's `video/webm;codecs=…`). */
  mime: string;
  /** The session's video bytes, streamed as they arrive. */
  body: ReadableStream<Uint8Array>;
}

export async function* subscribeInVideo(opts: {
  scene: string;
  signal: AbortSignal;
}): AsyncGenerator<VideoInTurn, void, void> {
  while (!opts.signal.aborted) {
    const res = await fetch("/api/in/vision", {
      method: "GET",
      headers: { "X-HI-Scene": opts.scene, Accept: "video/*" },
      signal: opts.signal,
      cache: "no-store",
    });
    if (!res.ok) {
      throw new Error(`/api/in/vision subscribe failed: ${res.status} ${res.statusText}`);
    }
    if (!res.body) continue;
    const mime = res.headers.get("content-type") ?? "video/webm";
    yield { mime, body: res.body };
  }
}

export async function postVision(opts: {
  scene: string;
  blob: Blob;
  mime: string;
  signal?: AbortSignal;
}): Promise<void> {
  const res = await fetch("/api/in/vision", {
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
      `/api/in/vision POST failed: ${res.status} ${res.statusText}${detail ? ` — ${detail.trim()}` : ""}`,
    );
  }
}

// The presence lane: a low-res camera still for the backend's always-on local
// face reflex (see `lib/presenceStiller`). Separate from `postVision` (one-off
// stills) and the full-fidelity video stream — these frames are never archived,
// they only feed real-time "who's here" recognition.
export async function postPresenceStill(opts: {
  scene: string;
  blob: Blob;
  signal?: AbortSignal;
}): Promise<void> {
  const res = await fetch("/api/in/vision/presence", {
    method: "POST",
    headers: {
      "Content-Type": "image/jpeg",
      "X-HI-Scene": opts.scene,
    },
    body: opts.blob,
    signal: opts.signal,
  });
  if (!res.ok) {
    throw new Error(`/api/in/vision/presence POST failed: ${res.status} ${res.statusText}`);
  }
}
