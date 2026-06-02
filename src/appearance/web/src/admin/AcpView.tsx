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

// A one-line human summary of an event's payload (beyond its name).
function eventDetail(d: SessionEvent): string {
  const s = (k: string) => String(d[k] ?? "");
  switch (d.event) {
    case "session_opened":
      return `reactor ${s("id").slice(0, 12)}`;
    case "session_closed":
      return `${s("kind")} ${s("id").slice(0, 12)} closed`;
    case "turn_started":
      return `turn ${s("turn")}`;
    case "turn_finished":
      return `turn ${s("turn")} → ${s("stop_reason") || "?"} (${s("reply_chars")} chars)`;
    case "hot_swap":
      return `${s("old_id").slice(0, 8)} → ${s("new_id").slice(0, 8)} · brief ${s("briefing_chars")}`;
    case "worker_spawned":
      return `#${s("id")} ${s("task")}`;
    case "worker_finished":
      return `#${s("id")} ${s("state")} (${s("summary_chars")} chars)`;
    case "worker_question":
      return `#${s("id")}: ${s("question")}`;
    case "alarm_scheduled":
      return `+${s("delay_s")}s · ${s("note")}`;
    case "alarm_fired":
      return s("note");
    default:
      return "";
  }
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
                      <span className="ts">{time(d.ts)}</span>
                      <span className={`evname ${d.event}`}>{d.event}</span>
                      <span className="evdetail">{eventDetail(d)}</span>
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
