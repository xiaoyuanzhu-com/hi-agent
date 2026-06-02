// Client for the inbound vision channel (camera frames).
//
// Vision mirrors audio: a continuous input channel. The client captures frames
// from the camera and POSTs each one here (POST /api/in/vision); there is no
// commit — the backend brain decides what (if anything) to do with the signal.
// The server currently just persists the frame (it can't perceive images yet),
// so this is fire-and-forget; we don't read the response body.

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
