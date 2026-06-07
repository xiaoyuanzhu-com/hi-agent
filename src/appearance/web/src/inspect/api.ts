// Inspect data layer — typed views over the endpoints the Rust backend exposes:
//   GET  /api/sessions                    → live per-scene snapshot (JSON)
//   GET  /api/sessions/events             → SSE of every lifecycle event ("session")
//   GET  /api/scenes/{scene}/channels     → SSE of one scene's channel activity ("channel")
//
// These mirror the serde shapes in `src/observatory/mod.rs` and
// `src/server/channels.rs`. Kept deliberately thin: the inspect views poll the
// snapshot and subscribe to the event streams.

export type SessionKind = "reactor" | "worker" | "summarizer";
export type WorkerState = "running" | "done" | "failed";

export interface SessionView {
  id: string;
  kind: SessionKind;
  opened_at: string;
  in_flight: boolean;
  turns: number;
}

export interface WorkerView {
  id: number;
  task: string;
  state: WorkerState;
  started_at: string;
  last_question: string | null;
  transcript_tail: string;
}

export interface AlarmView {
  note: string;
  fires_at: string;
}

export interface TurnView {
  turn: number;
  started_at: string;
  finished_at: string | null;
  stop_reason: string | null;
  reply_chars: number | null;
}

export interface SceneView {
  scene: string;
  reactor_session: SessionView | null;
  workers: WorkerView[];
  budget_chars: number;
  swap_after_chars: number;
  swap_count: number;
  last_swap_at: string | null;
  pending_alarms: AlarmView[];
  last_turn: TurnView | null;
  turns_total: number;
}

// One lifecycle event. `event` is the discriminant; the rest of the fields
// depend on it (see EventKind in the backend). We keep them loosely typed and
// read defensively in the renderer.
export interface SessionEvent {
  seq: number;
  ts: string;
  scene: string;
  event: string;
  [k: string]: unknown;
}

/** Fetch the live per-scene snapshot. Throws on network/HTTP error. */
export async function fetchSessions(signal?: AbortSignal): Promise<SceneView[]> {
  const res = await fetch("/api/sessions", { signal });
  if (!res.ok) throw new Error(`GET /api/sessions → ${res.status}`);
  return (await res.json()) as SceneView[];
}

export type Channel = "text" | "vision" | "audio" | "touch" | "smell" | "taste";
export type Direction = "in" | "out";

// One unit of channel activity for a scene — a recognized/spoken line on the
// text channel, or a metadata summary for binary/structured channels. Mirrors
// `ChannelSignal` in `src/server/channels.rs`.
export interface ChannelSignal {
  ts: string;
  channel: Channel;
  direction: Direction;
  body: string;
  final: boolean;
}

/**
 * Subscribe to one scene's merged channel-activity stream. Returns an
 * unsubscribe fn. This is live presence only — the backend replays nothing, so a
 * fresh subscriber sees activity from the moment it connects. EventSource
 * auto-reconnects. `scene` is encoded into the path (ids may contain `@`, `:`).
 */
export function subscribeChannels(
  scene: string,
  onSignal: (sig: ChannelSignal) => void,
  onStatus?: (live: boolean) => void,
): () => void {
  const es = new EventSource(`/api/scenes/${encodeURIComponent(scene)}/channels`);
  es.addEventListener("open", () => onStatus?.(true));
  es.addEventListener("error", () => onStatus?.(false));
  es.addEventListener("channel", (e) => {
    try {
      onSignal(JSON.parse((e as MessageEvent).data) as ChannelSignal);
    } catch {
      /* ignore malformed frame */
    }
  });
  return () => es.close();
}

// One raw JSON-RPC line on the ACP wire, business-logic agnostic. Mirrors
// `RawFrame` in `src/acp/tap.rs`. `raw` is the verbatim line; the rest is the
// little we parse out for grouping (the inspector keys sessions off `session_id`).
export type AcpDir = "send" | "recv" | "stderr";

export interface RawFrame {
  seq: number;
  ts: string;
  conn: number;
  scene: string;
  dir: AcpDir;
  session_id: string | null;
  method: string | null;
  id: unknown;
  raw: string;
}

/**
 * Subscribe to the raw ACP frame feed (`GET /api/acp/frames/events`). Returns an
 * unsubscribe fn. One SSE connection carries one frame type, `frame`: the
 * buffered ring replays on connect, then live frames stream. EventSource
 * auto-reconnects.
 */
export function subscribeAcpFrames(
  onFrame: (frame: RawFrame) => void,
  onStatus?: (live: boolean) => void,
): () => void {
  const es = new EventSource("/api/acp/frames/events");
  es.addEventListener("open", () => onStatus?.(true));
  es.addEventListener("error", () => onStatus?.(false));
  es.addEventListener("frame", (e) => {
    try {
      onFrame(JSON.parse((e as MessageEvent).data) as RawFrame);
    } catch {
      /* ignore malformed frame */
    }
  });
  return () => es.close();
}

export interface EventStreamHandlers {
  /** A lifecycle event arrived — append it to the event log. */
  onEvent?: (ev: SessionEvent) => void;
  /** A fresh full per-scene snapshot arrived — replace prior scene state. */
  onSnapshot?: (scenes: SceneView[]) => void;
  /** Connection liveness toggled (open → true, error/reconnecting → false). */
  onStatus?: (live: boolean) => void;
}

/**
 * Subscribe to the lifecycle stream. Returns an unsubscribe fn. One SSE
 * connection carries two frame types: `session` lifecycle events (buffered
 * history replayed on connect, then live) and periodic `snapshot` frames (the
 * full per-scene mirror). Reading scene state from the snapshot frames here
 * means the dashboard polls nothing — it holds a single connection rather than
 * leaking a `/api/sessions` request per tick into a starved HTTP/1.1 pool.
 * EventSource auto-reconnects.
 */
export function subscribeEvents(handlers: EventStreamHandlers): () => void {
  const es = new EventSource("/api/sessions/events");
  es.addEventListener("open", () => handlers.onStatus?.(true));
  es.addEventListener("error", () => handlers.onStatus?.(false));
  es.addEventListener("session", (e) => {
    try {
      handlers.onEvent?.(JSON.parse((e as MessageEvent).data) as SessionEvent);
    } catch {
      /* ignore malformed frame */
    }
  });
  es.addEventListener("snapshot", (e) => {
    try {
      handlers.onSnapshot?.(JSON.parse((e as MessageEvent).data) as SceneView[]);
    } catch {
      /* ignore malformed frame */
    }
  });
  return () => es.close();
}
