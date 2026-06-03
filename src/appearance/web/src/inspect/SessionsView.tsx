import { useEffect, useMemo, useState } from "react";
import { selectedUnder, usePath } from "./router";
import {
  fetchSessions,
  subscribeEvents,
  type SceneView,
  type SessionEvent,
  type WorkerView,
} from "./api";

const BASE = "/inspect/sessions";
const MAX_EVENTS = 2000;
const POLL_MS = 1500;

function ago(iso: string | null): string {
  if (!iso) return "—";
  const s = Math.max(0, (Date.now() - new Date(iso).getTime()) / 1000);
  if (s < 60) return `${s.toFixed(0)}s ago`;
  if (s < 3600) return `${(s / 60).toFixed(0)}m ago`;
  return `${(s / 3600).toFixed(1)}h ago`;
}
function time(iso: string): string {
  try {
    return new Date(iso).toLocaleTimeString();
  } catch {
    return iso;
  }
}

// Fields shown in the event header (or redundant there); everything else is
// rendered as raw payload below.
const META_KEYS = new Set(["seq", "ts", "scene", "event"]);

// Render one payload value exactly as it arrived — this is a debug surface, so
// nothing is summarized or truncated: strings verbatim with whitespace and
// newlines preserved, scalars plainly, objects/arrays as pretty JSON.
function EventValue({ value }: { value: unknown }) {
  if (value === null || value === undefined) return <span className="evnull">null</span>;
  if (typeof value === "string") return <pre className="evstr">{value}</pre>;
  if (typeof value === "number" || typeof value === "boolean")
    return <span className="evscalar">{String(value)}</span>;
  return <pre className="evstr">{JSON.stringify(value, null, 2)}</pre>;
}

// The structured payload of one event: every non-meta field as a key/value row,
// in the order the backend serialized them.
function EventPayload({ d }: { d: SessionEvent }) {
  const keys = Object.keys(d).filter((k) => !META_KEYS.has(k));
  if (keys.length === 0) return null;
  return (
    <div className="evpayload">
      {keys.map((k) => (
        <div className="evfield" key={k}>
          <span className="evk">{k}</span>
          <EventValue value={d[k]} />
        </div>
      ))}
    </div>
  );
}

// One ACP session, flattened out of the per-scene snapshot. A scene contributes
// its reactor session plus one row per live worker; the scene is part of the key
// because worker ids are only unique within a scene.
type Session =
  | { key: string; scene: string; kind: "reactor"; scene_view: SceneView }
  | { key: string; scene: string; kind: "worker"; scene_view: SceneView; worker: WorkerView };

function flatten(scenes: SceneView[]): Session[] {
  const out: Session[] = [];
  for (const v of scenes) {
    if (v.reactor_session) {
      out.push({ key: `${v.scene}::reactor`, scene: v.scene, kind: "reactor", scene_view: v });
    }
    for (const w of v.workers) {
      out.push({ key: `${v.scene}::w${w.id}`, scene: v.scene, kind: "worker", scene_view: v, worker: w });
    }
  }
  return out;
}

export function SessionsView() {
  const { path, navigate } = usePath();
  const selected = selectedUnder(path, BASE);
  const [scenes, setScenes] = useState<SceneView[]>([]);
  const [events, setEvents] = useState<SessionEvent[]>([]);
  const [live, setLive] = useState(false);

  // Poll the live snapshot.
  useEffect(() => {
    let cancelled = false;
    const ctrl = new AbortController();
    const tick = async () => {
      try {
        const data = await fetchSessions(ctrl.signal);
        if (cancelled) return;
        data.sort((a, b) => a.scene.localeCompare(b.scene));
        setScenes(data);
      } catch {
        /* transient — next tick retries */
      }
    };
    void tick();
    const h = window.setInterval(tick, POLL_MS);
    return () => {
      cancelled = true;
      ctrl.abort();
      window.clearInterval(h);
    };
  }, []);

  // Subscribe to the lifecycle event stream.
  useEffect(() => {
    return subscribeEvents(
      (ev) =>
        setEvents((prev) => {
          const next = prev.length >= MAX_EVENTS ? prev.slice(prev.length - MAX_EVENTS + 1) : prev;
          return [...next, ev];
        }),
      setLive,
    );
  }, []);

  const sessions = useMemo(() => flatten(scenes), [scenes]);
  const current = sessions.find((s) => s.key === selected) ?? null;

  return (
    <div className="acp">
      <aside className="acp-list">
        <div className="acp-list-head">
          <span>Sessions</span>
          <span className={`live-dot ${live ? "on" : ""}`} title={live ? "event stream live" : "reconnecting"} />
        </div>
        {sessions.length === 0 ? (
          <div className="muted pad">No active sessions yet. They appear on a scene's first turn.</div>
        ) : (
          <ul>
            {sessions.map((s) => (
              <li
                key={s.key}
                className={s.key === selected ? "sel" : ""}
                onClick={() => navigate(`${BASE}/${encodeURIComponent(s.key)}`)}
              >
                <span className={`dot ${sessionDot(s)}`} />
                <span className={`skind ${s.kind}`}>{s.kind === "reactor" ? "rx" : "wk"}</span>
                <span className="nm">{sessionLabel(s)}</span>
                <span className="badges">
                  <span className="mini">{s.scene}</span>
                </span>
              </li>
            ))}
          </ul>
        )}
      </aside>

      <section className="acp-detail">
        {!current ? (
          <div className="muted pad">Select a session.</div>
        ) : current.kind === "reactor" ? (
          <ReactorDetail scene={current.scene_view} events={events} />
        ) : (
          <WorkerDetail scene={current.scene} worker={current.worker} events={events} />
        )}
      </section>
    </div>
  );
}

function sessionLabel(s: Session): string {
  if (s.kind === "reactor") return `reactor ${s.scene_view.reactor_session?.id.slice(0, 16) ?? ""}`;
  return `#${s.worker.id} ${s.worker.task}`;
}

function sessionDot(s: Session): string {
  if (s.kind === "reactor") return s.scene_view.reactor_session?.in_flight ? "busy" : "idle";
  return s.worker.state === "running" ? "busy" : s.worker.state === "failed" ? "cold" : "idle";
}

function ReactorDetail({ scene: v, events }: { scene: SceneView; events: SessionEvent[] }) {
  const rs = v.reactor_session;
  const pct = v.swap_after_chars > 0 ? Math.min(100, (100 * v.budget_chars) / v.swap_after_chars) : 0;
  // A reactor session owns the scene-level lifecycle, so surface every event for
  // the scene except worker-specific ones (those live on the worker detail).
  const sceneEvents = useMemo(
    () =>
      events
        .filter((e) => e.scene === v.scene && !String(e.event).startsWith("worker_"))
        .slice()
        .reverse(),
    [events, v.scene],
  );

  return (
    <div className="detail-head">
      <div className="dh-title">
        <b>reactor</b>
        <span className="muted">
          {v.scene} · process up {ago(v.process_spawned_at)} · {v.turns_total} turns · {v.swap_count} swaps
        </span>
      </div>

      <div className="cards">
        <div className="card">
          <h4>Session</h4>
          {rs ? (
            <>
              <div className="kv">
                <span className={`pill ${rs.in_flight ? "inflight" : ""}`}>{rs.in_flight ? "in-flight" : "idle"}</span>
                <code>{rs.id.slice(0, 16)}</code>
              </div>
              <div className="kv muted">{rs.turns} turns · opened {ago(rs.opened_at)}</div>
            </>
          ) : (
            <div className="muted">not opened</div>
          )}
        </div>

        <div className="card">
          <h4>Context budget</h4>
          <div className="kv">
            <b>{v.budget_chars.toLocaleString()}</b> <span className="muted">/ {v.swap_after_chars.toLocaleString()} chars</span>
          </div>
          <div className={`bar ${pct > 80 ? "hot" : ""}`}>
            <i style={{ width: `${pct}%` }} />
          </div>
          <div className="kv muted">last swap {ago(v.last_swap_at)}</div>
        </div>

        <div className="card">
          <h4>Last turn</h4>
          {v.last_turn ? (
            <div className="kv">
              turn {v.last_turn.turn} ·{" "}
              {v.last_turn.finished_at
                ? `${v.last_turn.stop_reason ?? "?"} · ${v.last_turn.reply_chars ?? 0} chars`
                : "running…"}
            </div>
          ) : (
            <div className="muted">—</div>
          )}
        </div>
      </div>

      {v.pending_alarms.length > 0 && (
        <div className="card">
          <h4>Pending alarms <span className="muted">({v.pending_alarms.length})</span></h4>
          {v.pending_alarms.map((a, i) => (
            <div className="alarm" key={i}>⏰ {a.note || "(no note)"} · fires {time(a.fires_at)}</div>
          ))}
        </div>
      )}

      <EventLog events={sceneEvents} />
    </div>
  );
}

function WorkerDetail({
  scene,
  worker: w,
  events,
}: {
  scene: string;
  worker: WorkerView;
  events: SessionEvent[];
}) {
  const workerEvents = useMemo(
    () =>
      events
        .filter((e) => e.scene === scene && String(e.event).startsWith("worker_") && Number(e.id) === w.id)
        .slice()
        .reverse(),
    [events, scene, w.id],
  );

  return (
    <div className="detail-head">
      <div className="dh-title">
        <b>worker #{w.id}</b>
        <span className="muted">{scene} · started {ago(w.started_at)}</span>
      </div>

      <div className="card">
        <h4>Task <span className={`wbadge ${w.state}`}>{w.state}</span></h4>
        <div className="kv">{w.task}</div>
        {w.transcript_tail && <div className="tail">{w.transcript_tail}</div>}
        {w.last_question && <div className="q">⁇ {w.last_question}</div>}
      </div>

      <EventLog events={workerEvents} />
    </div>
  );
}

function EventLog({ events }: { events: SessionEvent[] }) {
  return (
    <div className="acp-events">
      <h3>Events <span className="muted">({events.length})</span></h3>
      {events.length === 0 ? (
        <div className="muted pad">No events yet.</div>
      ) : (
        <div className="evlist">
          {events.map((d) => (
            <div className="ev" key={d.seq}>
              <div className="evhead">
                <span className="ts">{time(d.ts)}</span>
                <span className={`evname ${d.event}`}>{d.event}</span>
                <span className="evseq">#{d.seq}</span>
              </div>
              <EventPayload d={d} />
            </div>
          ))}
        </div>
      )}
    </div>
  );
}
