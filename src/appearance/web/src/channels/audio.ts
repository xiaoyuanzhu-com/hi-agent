// Client for the human-interface /audio channel (inbound STT).
//
// Capture + WAV encoding now live in `lib/micCapture` + `lib/wav` — continuous
// VAD segmentation replaced the old push-to-talk recorder, so this module is
// just the POST of a finished utterance.

export async function postAudio(opts: {
  from: string;
  blob: Blob;
  mime: string;
  signal?: AbortSignal;
}): Promise<{ transcript: string; media_path: string }> {
  const res = await fetch("/api/audio", {
    method: "POST",
    headers: {
      "Content-Type": opts.mime,
      "X-HI-From": opts.from,
    },
    body: opts.blob,
    signal: opts.signal,
  });
  if (!res.ok) {
    const detail = await res.text().catch(() => "");
    throw new Error(
      `/api/audio POST failed: ${res.status} ${res.statusText}${detail ? ` — ${detail.trim()}` : ""}`,
    );
  }
  return (await res.json()) as { transcript: string; media_path: string };
}

export interface SubscribeAudioOpts {
  /** Peer identity we receive on (sent as X-HI-To). */
  peer: string;
  signal: AbortSignal;
}

/**
 * Outbound TTS (Phase 2). GET /audio is a long-poll that returns one
 * synthesized clip per response, so we re-subscribe after each. Yields each
 * clip as it arrives; the caller just plays them in order. Turn-taking (which
 * reply to voice, when) is decided server-side — the client only renders, so
 * there's no turn metadata to read here.
 */
export async function* subscribeAudio(
  opts: SubscribeAudioOpts,
): AsyncGenerator<Blob, void, void> {
  while (!opts.signal.aborted) {
    const res = await fetch("/api/audio", {
      method: "GET",
      headers: { "X-HI-To": opts.peer, Accept: "audio/*" },
      signal: opts.signal,
      cache: "no-store",
    });
    if (!res.ok) {
      throw new Error(`/api/audio subscribe failed: ${res.status} ${res.statusText}`);
    }
    const blob = await res.blob();
    if (blob.size > 0) yield blob;
  }
}

