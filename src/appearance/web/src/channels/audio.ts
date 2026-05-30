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
  const res = await fetch("/audio", {
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
      `/audio POST failed: ${res.status} ${res.statusText}${detail ? ` — ${detail.trim()}` : ""}`,
    );
  }
  return (await res.json()) as { transcript: string; media_path: string };
}

export interface SubscribeAudioOpts {
  /** Peer identity we receive on (sent as X-HI-To). */
  peer: string;
  signal: AbortSignal;
}

/** One synthesized clip plus the cognition-turn that produced it. */
export interface AudioClip {
  blob: Blob;
  /** Monotonic turn id (from the `X-HI-Turn` header); 0 if absent. */
  turn: number;
}

/**
 * Outbound TTS (Phase 2). GET /audio is a long-poll that returns one
 * synthesized clip per response, so we re-subscribe after each. Yields each
 * clip with its turn id as it arrives; the caller voices only the latest turn
 * and discards superseded drafts.
 */
export async function* subscribeAudio(
  opts: SubscribeAudioOpts,
): AsyncGenerator<AudioClip, void, void> {
  while (!opts.signal.aborted) {
    const res = await fetch("/audio", {
      method: "GET",
      headers: { "X-HI-To": opts.peer, Accept: "audio/*" },
      signal: opts.signal,
      cache: "no-store",
    });
    if (!res.ok) {
      throw new Error(`/audio subscribe failed: ${res.status} ${res.statusText}`);
    }
    const turn = Number.parseInt(res.headers.get("X-HI-Turn") ?? "", 10);
    const blob = await res.blob();
    if (blob.size > 0) yield { blob, turn: Number.isNaN(turn) ? 0 : turn };
  }
}

