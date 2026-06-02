// Admin data layer — typed views over the observatory endpoints the Rust
// backend exposes:
//   GET  /api/sessions          → live per-scene snapshot (JSON)
//   GET  /api/sessions/events   → SSE of every lifecycle event (named "session")
//
// These mirror the serde shapes in `src/observatory/mod.rs`. Kept deliberately
// thin: the admin views poll the snapshot and subscribe to the event stream.

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
  process_spawned_at: string | null;
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

/**
 * Subscribe to the lifecycle event stream. Returns an unsubscribe fn. The
 * backend replays buffered history on connect, then streams live — so a fresh
 * subscriber sees recent context immediately. EventSource auto-reconnects.
 */
export function subscribeEvents(
  onEvent: (ev: SessionEvent) => void,
  onStatus?: (live: boolean) => void,
): () => void {
  const es = new EventSource("/api/sessions/events");
  es.addEventListener("open", () => onStatus?.(true));
  es.addEventListener("error", () => onStatus?.(false));
  es.addEventListener("session", (e) => {
    try {
      onEvent(JSON.parse((e as MessageEvent).data) as SessionEvent);
    } catch {
      /* ignore malformed frame */
    }
  });
  return () => es.close();
}
