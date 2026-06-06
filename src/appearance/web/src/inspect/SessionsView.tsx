import { useEffect, useMemo, useState, type ReactNode } from "react";
import { selectedUnder, usePath } from "./router";
import { subscribeAcpFrames, type AcpDir, type RawFrame } from "./api";

const BASE = "/inspect/sessions";
const MAX_FRAMES = 5000;

// 24-hour wall-clock with milliseconds — frames arrive faster than a second, so
// the ms part is what tells two adjacent frames apart.
function time(iso: string): string {
  const d = new Date(iso);
  if (Number.isNaN(d.getTime())) return iso;
  const p = (n: number, w = 2) => String(n).padStart(w, "0");
  return `${p(d.getHours())}:${p(d.getMinutes())}:${p(d.getSeconds())}.${p(d.getMilliseconds(), 3)}`;
}

// Direction from the *session's* point of view (this is the sessions page): a
// frame hi-agent sends is `in`coming to the session; one the subprocess emits is
// `out`going from it; `err` is what it wrote to stderr.
function dirLabel(dir: AcpDir): string {
  return dir === "send" ? "in" : dir === "recv" ? "out" : "err";
}

// A one-line, whitespace-collapsed preview of the raw frame — shown as the
// collapsed summary so the log scans without expanding every payload.
function preview(raw: string): string {
  const line = raw.replace(/\s+/g, " ").trim();
  return line.length > 160 ? `${line.slice(0, 160)}…` : line;
}

// A readable view for the Frame column. Some payloads carry content worth
// reading at a glance: `usage_update` becomes `usage_update 45140 / 200000, 23%`,
// and a `session/prompt` renders each prompt part as a stacked text block.
// Everything else falls back to a one-line `preview`.
function frameSummary(raw: string): ReactNode {
  try {
    const msg = JSON.parse(raw);
    const update = msg?.params?.update;
    if (update?.sessionUpdate === "usage_update") {
      const { used, size } = update;
      if (typeof used === "number" && typeof size === "number") {
        const pct = size > 0 ? Math.round((used / size) * 100) : 0;
        return `usage_update: ${used} / ${size}, ${pct}%`;
      }
    }
    if (update?.sessionUpdate === "agent_thought_chunk") {
      const text = update.content?.text;
      if (typeof text === "string") return `agent_thought_chunk: "${text}"`;
    }
    if (update?.sessionUpdate === "agent_message_chunk") {
      const text = update.content?.text;
      if (typeof text === "string") return `agent_message_chunk: "${text}"`;
    }
    if (update?.sessionUpdate === "available_commands_update") {
      return "available_commands_update";
    }
    if (update?.sessionUpdate === "tool_call") {
      const name = update._meta?.claudeCode?.toolName ?? update.title ?? update.kind;
      return `tool_call: ${name}, ${update.status}`;
    }
    if (update?.sessionUpdate === "tool_call_update") {
      const name = update._meta?.claudeCode?.toolName ?? update.title ?? update.kind;
      const input = update.rawInput;
      const detail = typeof input?.text === "string" ? `"${input.text}"` : JSON.stringify(input ?? {});
      return `tool_call_update: ${name}, ${detail}`;
    }
    if (Array.isArray(msg?.result?.configOptions)) {
      const names = msg.result.configOptions.map((o: { name?: string }) => o?.name).filter(Boolean);
      return `configOptions: ${names.join(", ")}`;
    }
    if (msg?.method === "session/request_permission") {
      const title = msg.params?.toolCall?.title;
      if (typeof title === "string") return title;
    }
    if (msg?.method === "session/prompt" && Array.isArray(msg?.params?.prompt)) {
      const parts = msg.params.prompt;
      return (
        <div className="frprompt">
          {parts.map((p: { text?: string }, i: number) => (
            <div key={i} className="frprompt-block">
              {typeof p?.text === "string" ? p.text : JSON.stringify(p)}
            </div>
          ))}
        </div>
      );
    }
  } catch {
    // fall through to the generic preview
  }
  return preview(raw);
}

// Pretty-print the verbatim line as JSON; fall back to the raw string when it
// isn't JSON (rare — stderr spew). This is a debug surface, so nothing is
// summarized or truncated.
function pretty(raw: string): string {
  try {
    return JSON.stringify(JSON.parse(raw), null, 2);
  } catch {
    return raw;
  }
}

// A short human label for a frame: its JSON-RPC method, or — for responses,
// which carry no method — whether it resolved or errored.
function frameLabel(f: RawFrame): string {
  if (f.method) return f.method;
  if (/"error"\s*:/.test(f.raw)) return "↩ error";
  if (/"result"\s*:/.test(f.raw)) return "↩ result";
  return "—";
}

// One group of frames the inspector renders as a "session": either a real ACP
// session (keyed by sessionId) or a per-scene handshake bucket holding the
// frames that carry no sessionId (`initialize`, the `session/new` request).
interface Group {
  key: string; // URL key + dedup key
  scene: string;
  sessionId: string | null; // null → handshake bucket
  frames: RawFrame[];
}

// Fold the flat frame stream into per-session groups, preserving first-seen
// order. A frame with a sessionId joins that session; one without joins its
// scene's handshake bucket. Knows nothing about the reactor — pure ACP.
function group(frames: RawFrame[]): Group[] {
  const map = new Map<string, Group>();
  for (const f of frames) {
    const key = f.session_id ? `s::${f.session_id}` : `h::${f.scene}`;
    let g = map.get(key);
    if (!g) {
      g = { key, scene: f.scene, sessionId: f.session_id, frames: [] };
      map.set(key, g);
    }
    g.frames.push(f);
  }
  return [...map.values()];
}

export function SessionsView() {
  const { path, navigate } = usePath();
  const selected = selectedUnder(path, BASE);
  const [frames, setFrames] = useState<RawFrame[]>([]);
  const [live, setLive] = useState(false);

  // One SSE connection feeds the entire view — every raw ACP frame, replayed on
  // connect then live. The session list and each detail pane are derived from
  // this single stream; no polling, no per-session endpoint.
  useEffect(() => {
    return subscribeAcpFrames(
      (f) =>
        setFrames((prev) => {
          const next = prev.length >= MAX_FRAMES ? prev.slice(prev.length - MAX_FRAMES + 1) : prev;
          return [...next, f];
        }),
      setLive,
    );
  }, []);

  const groups = useMemo(() => group(frames), [frames]);
  const current = groups.find((g) => g.key === selected) ?? null;

  return (
    <div className="acp">
      <aside className="acp-list">
        <div className="acp-list-head">
          <span>ACP sessions</span>
          <span className={`live-dot ${live ? "on" : ""}`} title={live ? "frame stream live" : "reconnecting"} />
        </div>
        {groups.length === 0 ? (
          <div className="muted pad">No ACP frames yet. They appear on a scene's first contact.</div>
        ) : (
          <ul>
            {groups.map((g) => (
              <li
                key={g.key}
                className={g.key === selected ? "sel" : ""}
                onClick={() => navigate(`${BASE}/${encodeURIComponent(g.key)}`)}
              >
                <span className={`skind ${g.sessionId ? "reactor" : "worker"}`}>{g.sessionId ? "id" : "hs"}</span>
                <span className="nm">{g.sessionId ? g.sessionId.slice(0, 20) : "handshake"}</span>
                <span className="badges">
                  <span className="mini">{g.scene}</span>
                  <span className="mini">{g.frames.length}</span>
                </span>
              </li>
            ))}
          </ul>
        )}
      </aside>

      <section className="acp-detail">
        {!current ? (
          <div className="muted pad">Select a session to see its raw ACP frames.</div>
        ) : (
          <FrameLog group={current} />
        )}
      </section>
    </div>
  );
}

function FrameLog({ group: g }: { group: Group }) {
  return (
    <div className="detail-head">
      <div className="dh-title">
        <b>{g.sessionId ? "session" : "handshake"}</b>
        <span className="muted">
          {g.scene}
          {g.sessionId ? <> · <code>{g.sessionId}</code></> : null} · {g.frames.length} frames
        </span>
      </div>

      <div className="acp-events">
        <table className="evtable frtable">
          <thead>
            <tr>
              <th>Time</th>
              <th>Dir</th>
              <th>Method</th>
              <th>#</th>
              <th>Frame</th>
              <th>JSON</th>
            </tr>
          </thead>
          <tbody>
            {g.frames.map((f) => (
              <tr key={f.seq}>
                <td className="ts">{time(f.ts)}</td>
                <td className={`frdir ${f.dir}`}>{dirLabel(f.dir)}</td>
                <td className="evname">{frameLabel(f)}</td>
                <td className="evseq">{f.seq}</td>
                <td className="evframe">{frameSummary(f.raw)}</td>
                <td className="evraw">
                  <details>
                    <summary>{preview(f.raw)}</summary>
                    <pre>{pretty(f.raw)}</pre>
                  </details>
                </td>
              </tr>
            ))}
          </tbody>
        </table>
      </div>
    </div>
  );
}
