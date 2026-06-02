// Client for the inbound audio channel.
//
// Live mic input streams continuously over the `/api/in/audio/stream` WebSocket
// (see `lib/audioStreamer`), upload-only; the upstream STT does the endpointing
// and the server publishes recognized speech to the scene's observe stream.
//
// `subscribeInAudio` observes that recognized speech (GET /api/in/audio): partial
// frames (`final: false`) for the live rolling line, settled sentences
// (`final: true`) for committed utterances. Every client — mic-holder or not —
// renders from this same stream.
//
// `postAudio` is the one-shot batch path for `POST /api/in/audio` — kept for
// non-streaming callers (e.g. uploading a finished clip); not used by the live mic.

import { observeInput, type InputEvent } from "../ndjson";

/** One recognized-speech result: rolling partial (`final:false`) or settled. */
export type TranscriptEvent = InputEvent;

/** Observe recognized speech on this scene (live, no replay). */
export function subscribeInAudio(opts: {
  scene: string;
  signal: AbortSignal;
}): AsyncGenerator<TranscriptEvent, void, void> {
  return observeInput("/api/in/audio", opts);
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
