// Client for the inbound text channel.
//
// `postInText` sends a typed line to the agent (POST /api/in/text). The server
// dispatches it to the mind *and* echoes it to scene observers, so the line is
// rendered from the observe stream below — not echoed locally — keeping every
// client's UI identical.
//
// `subscribeInText` observes those typed inputs (GET /api/in/text), live.

import { observeInput, type InputEvent } from "../ndjson";

/** Observe typed inputs on this scene (live, no replay). */
export function subscribeInText(opts: {
  scene: string;
  signal: AbortSignal;
}): AsyncGenerator<InputEvent, void, void> {
  return observeInput("/api/in/text", opts);
}

/**
 * Send a text signal to the agent.
 * Returns when the server has accepted the body (202).
 */
export async function postInText(opts: {
  scene: string;
  body: string;
  signal?: AbortSignal;
}): Promise<void> {
  const res = await fetch("/api/in/text", {
    method: "POST",
    headers: {
      "Content-Type": "text/plain; charset=utf-8",
      "X-HI-Scene": opts.scene,
    },
    body: opts.body,
    signal: opts.signal,
  });

  if (!res.ok) {
    const detail = await res.text().catch(() => "");
    throw new Error(
      `/api/in/text POST failed: ${res.status} ${res.statusText}${detail ? ` — ${detail}` : ""}`,
    );
  }
}
