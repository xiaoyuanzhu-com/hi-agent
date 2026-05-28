// Subscriber + responder for the human-interface /approval channel.
//
// One subscription delivers one approval request. After we decide (or the
// body closes for any other reason) the caller re-subscribes.
//
// v0 wire format (per impl.md): one JSON object per long-poll, containing at
// least { id, action, summary }. Optional fields: details, requested_at.

export interface ApprovalRequest {
  id: string;
  action: string;
  summary: string;
  details?: unknown;
  requested_at?: string;
  /** Whatever else the server included. */
  [key: string]: unknown;
}

export interface ApprovalDecision {
  id: string;
  allow: boolean;
  reason?: string;
}

export interface ApprovalSubscribeOpts {
  peer: string;
  signal: AbortSignal;
}

/**
 * Wait for one approval request addressed to `peer`. Resolves with the
 * parsed request when one arrives, or `null` if the body closed empty
 * (timeout-style end of long-poll — re-subscribe).
 */
export async function awaitApproval(
  opts: ApprovalSubscribeOpts,
): Promise<ApprovalRequest | null> {
  const res = await fetch("/approval", {
    method: "GET",
    headers: {
      "X-HI-To": opts.peer,
      Accept: "application/json",
    },
    signal: opts.signal,
    cache: "no-store",
  });

  if (!res.ok) {
    throw new Error(`/approval subscribe failed: ${res.status} ${res.statusText}`);
  }

  const text = (await res.text()).trim();
  if (text.length === 0) return null;

  // The server may send a single JSON object, or newline-delimited JSON
  // (one per event). For v0 only the first line matters — body-close ends
  // the event.
  const firstLine = text.split(/\r?\n/, 1)[0] ?? text;
  try {
    const parsed = JSON.parse(firstLine) as ApprovalRequest;
    if (typeof parsed.id !== "string" || typeof parsed.action !== "string") {
      throw new Error("malformed approval payload");
    }
    return parsed;
  } catch (err) {
    throw new Error(
      `could not parse approval payload: ${err instanceof Error ? err.message : String(err)}`,
    );
  }
}

/**
 * Send the decision for an approval request.
 */
export async function postApproval(
  decision: ApprovalDecision,
  opts: { from: string; signal?: AbortSignal } = { from: "web@local" },
): Promise<void> {
  const res = await fetch("/approval", {
    method: "POST",
    headers: {
      "Content-Type": "application/json",
      "X-HI-From": opts.from,
    },
    body: JSON.stringify(decision),
    signal: opts.signal,
  });

  if (!res.ok) {
    const detail = await res.text().catch(() => "");
    throw new Error(
      `/approval POST failed: ${res.status} ${res.statusText}${detail ? ` — ${detail}` : ""}`,
    );
  }
}
