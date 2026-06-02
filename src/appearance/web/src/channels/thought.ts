// Subscriber for the human-interface /thought channel.
//
// Spec rules we obey here:
//   * GET /thought is a long-poll. The server holds the response open and
//     streams body bytes as the agent emits. Body-close ends the utterance.
//   * X-HI-Scene names the scene we want to receive on (i.e. "stream me this
//     scene's output"). Without it the server can't key the right mailbox.
//   * After body-close we re-subscribe. Each subscription is one utterance.
//
// The function is an async generator: each yielded string is a UTF-8 chunk
// of one in-flight utterance. The generator returns when the body closes.

export interface ThoughtChunk {
  /** The chunk of text the server just emitted. */
  text: string;
}

export interface SubscribeOpts {
  /** Scene we want to receive on. Sent as X-HI-Scene. */
  scene: string;
  /** Abort signal so the caller can cancel cleanly on unmount. */
  signal: AbortSignal;
}

/**
 * Open one long-poll against /thought. Yields each chunk of text as it arrives.
 * Resolves (returns) when the server closes the body — i.e. the utterance ended.
 * Throws if the request fails or is aborted; callers should treat AbortError
 * as a normal shutdown.
 */
export async function* subscribeThought(
  opts: SubscribeOpts,
): AsyncGenerator<ThoughtChunk, void, void> {
  const res = await fetch("/api/thought", {
    method: "GET",
    headers: {
      "X-HI-Scene": opts.scene,
      Accept: "text/plain, application/octet-stream",
    },
    signal: opts.signal,
    // Streaming responses must not be cached.
    cache: "no-store",
  });

  if (!res.ok) {
    throw new Error(`/thought subscribe failed: ${res.status} ${res.statusText}`);
  }

  // Some servers (or proxies) may return a non-streaming body. fall through:
  if (!res.body) {
    const text = await res.text();
    if (text.length > 0) yield { text };
    return;
  }

  const reader = res.body.getReader();
  const decoder = new TextDecoder("utf-8");

  try {
    while (true) {
      const { value, done } = await reader.read();
      if (done) return;
      if (!value || value.byteLength === 0) continue;
      const text = decoder.decode(value, { stream: true });
      if (text.length > 0) yield { text };
    }
  } finally {
    try {
      reader.releaseLock();
    } catch {
      // ignore
    }
  }
}

/**
 * Send a /thought signal to the agent.
 * Returns when the server has accepted the body (202).
 */
export async function postThought(opts: {
  scene: string;
  body: string;
  signal?: AbortSignal;
}): Promise<void> {
  const res = await fetch("/api/thought", {
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
      `/thought POST failed: ${res.status} ${res.statusText}${detail ? ` — ${detail}` : ""}`,
    );
  }
}
