import { useEffect, useMemo, useRef, useState } from "react";
import {
  fetchSessions,
  subscribeEvents,
  type SceneView,
  type SessionEvent,
} from "./api";

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

export function AcpView() {
  const [scenes, setScenes] = useState<SceneView[]>([]);
  const [events, setEvents] = useState<SessionEvent[]>([]);
  const [selected, setSelected] = useState<string | null>(null);
  const [live, setLive] = useState(false);
  const selectedRef = useRef<string | null>(null);
  selectedRef.current = selected;

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
        // Auto-select the first scene once one appears.
        const first = data[0];
        if (!selectedRef.current && first) setSelected(first.scene);
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

  const current = scenes.find((s) => s.scene === selected) ?? null;
  const sceneEvents = useMemo(
    () => events.filter((e) => e.scene === selected).slice().reverse(),
    [events, selected],
  );

  return (
    <div className="acp">
      <aside className="acp-list">
        <div className="acp-list-head">
          <span>Scenes</span>
          <span className={`live-dot ${live ? "on" : ""}`} title={live ? "event stream live" : "reconnecting"} />
        </div>
        {scenes.length === 0 ? (
          <div className="muted pad">No active scenes yet. They appear on a scene's first turn.</div>
        ) : (
          <ul>
            {scenes.map((s) => {
              const inFlight = s.reactor_session?.in_flight;
              const running = s.workers.filter((w) => w.state === "running").length;
              return (
                <li
                  key={s.scene}
                  className={s.scene === selected ? "sel" : ""}
                  onClick={() => setSelected(s.scene)}
                >
                  <span className={`dot ${inFlight ? "busy" : s.reactor_session ? "idle" : "cold"}`} />
                  <span className="nm">{s.scene}</span>
                  <span className="badges">
                    {running > 0 && <span className="mini">{running}⚙</span>}
                    <span className="mini">{s.turns_total}t</span>
                  </span>
                </li>
              );
            })}
          </ul>
        )}
      </aside>

      <section className="acp-detail">
        {!current ? (
          <div className="muted pad">Select a scene.</div>
        ) : (
          <>
            <SceneDetail scene={current} />
            <div className="acp-events">
              <h3>Events <span className="muted">({sceneEvents.length})</span></h3>
              {sceneEvents.length === 0 ? (
                <div className="muted pad">No events for this scene yet.</div>
              ) : (
                <div className="evlist">
                  {sceneEvents.map((d) => (
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
          </>
        )}
      </section>
    </div>
  );
}

function SceneDetail({ scene: v }: { scene: SceneView }) {
  const rs = v.reactor_session;
  const pct = v.swap_after_chars > 0 ? Math.min(100, (100 * v.budget_chars) / v.swap_after_chars) : 0;
  return (
    <div className="detail-head">
      <div className="dh-title">
        <b>{v.scene}</b>
        <span className="muted">
          process up {ago(v.process_spawned_at)} · {v.turns_total} turns · {v.swap_count} swaps
        </span>
      </div>

      <div className="cards">
        <div className="card">
          <h4>Reactor session</h4>
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

      <div className="card">
        <h4>Workers <span className="muted">({v.workers.length})</span></h4>
        {v.workers.length === 0 ? (
          <div className="muted">none</div>
        ) : (
          v.workers.map((w) => (
            <div className="worker" key={w.id}>
              <div className="wt">
                <span className="task">#{w.id} {w.task}</span>
                <span className={`wbadge ${w.state}`}>{w.state}</span>
                <span className="muted started">{ago(w.started_at)}</span>
              </div>
              {w.transcript_tail && <div className="tail">{w.transcript_tail}</div>}
              {w.last_question && <div className="q">⁇ {w.last_question}</div>}
            </div>
          ))
        )}
      </div>

      {v.pending_alarms.length > 0 && (
        <div className="card">
          <h4>Pending alarms <span className="muted">({v.pending_alarms.length})</span></h4>
          {v.pending_alarms.map((a, i) => (
            <div className="alarm" key={i}>⏰ {a.note || "(no note)"} · fires {time(a.fires_at)}</div>
          ))}
        </div>
      )}
    </div>
  );
}
