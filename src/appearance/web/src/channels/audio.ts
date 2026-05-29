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
