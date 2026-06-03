import { useEffect, useMemo, useState } from "react";
import { selectedUnder, usePath } from "./router";
import {
  fetchSessions,
  subscribeChannels,
  type Channel,
  type ChannelSignal,
  type SceneView,
} from "./api";

const BASE = "/admin/scenes";
const POLL_MS = 1500;
const MAX_PER_CHANNEL = 200;

// Every channel a scene can carry, in display order. The detail view shows a
// card per channel so an operator sees the full sensory/expressive surface —
// including the quiet ones — not only the channels that happen to be active.
const CHANNELS: { key: Channel; label: string }[] = [
  { key: "text", label: "Text" },
  { key: "audio", label: "Audio" },
  { key: "vision", label: "Vision" },
  { key: "touch", label: "Touch" },
  { key: "smell", label: "Smell" },
  { key: "taste", label: "Taste" },
];

function time(iso: string): string {
  try {
    return new Date(iso).toLocaleTimeString();
  } catch {
    return iso;
  }
}

export function ScenesView() {
  const { path, navigate } = usePath();
  const selected = selectedUnder(path, BASE);
  const [scenes, setScenes] = useState<SceneView[]>([]);

  // Poll the observatory snapshot for the live scene list (observatory-only, as
  // the Scenes tab is about channels — sessions are the ACP tab's concern).
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

  return (
    <div className="acp">
      <aside className="acp-list">
        <div className="acp-list-head">
          <span>Scenes</span>
        </div>
        {scenes.length === 0 ? (
          <div className="muted pad">No active scenes yet. They appear on a scene's first turn.</div>
        ) : (
          <ul>
            {scenes.map((s) => {
              const inFlight = s.reactor_session?.in_flight;
              return (
                <li
                  key={s.scene}
                  className={s.scene === selected ? "sel" : ""}
                  onClick={() => navigate(`${BASE}/${encodeURIComponent(s.scene)}`)}
                >
                  <span className={`dot ${inFlight ? "busy" : s.reactor_session ? "idle" : "cold"}`} />
                  <span className="nm">{s.scene}</span>
                </li>
              );
            })}
          </ul>
        )}
      </aside>

      <section className="acp-detail">
        {!selected ? (
          <div className="muted pad">Select a scene to inspect its channels.</div>
        ) : (
          <SceneChannels key={selected} scene={selected} />
        )}
      </section>
    </div>
  );
}

/**
 * Live presence across one scene's channels. Subscribes to the merged channel
 * stream and buckets signals by channel; each channel renders a rolling feed.
 * Keyed by scene at the call site so switching scenes remounts a fresh stream.
 */
function SceneChannels({ scene }: { scene: string }) {
  const [signals, setSignals] = useState<ChannelSignal[]>([]);
  const [live, setLive] = useState(false);

  useEffect(() => {
    setSignals([]);
    return subscribeChannels(
      scene,
      (sig) => {
        // Text `final` markers carry no body — they only close an utterance, so
        // they're not worth a feed line; keep everything else.
        if (sig.channel === "text" && sig.final && sig.body === "") return;
        setSignals((prev) => {
          const next = prev.length >= MAX_PER_CHANNEL * CHANNELS.length ? prev.slice(1) : prev;
          return [...next, sig];
        });
      },
      setLive,
    );
  }, [scene]);

  const byChannel = useMemo(() => {
    const m = new Map<Channel, ChannelSignal[]>();
    for (const sig of signals) {
      const arr = m.get(sig.channel) ?? [];
      arr.push(sig);
      if (arr.length > MAX_PER_CHANNEL) arr.shift();
      m.set(sig.channel, arr);
    }
    return m;
  }, [signals]);

  return (
    <div className="detail-head">
      <div className="dh-title">
        <b>{scene}</b>
        <span className="muted">
          live channels
          <span className={`live-dot ${live ? "on" : ""}`} title={live ? "channel stream live" : "reconnecting"} />
        </span>
      </div>

      <div className="chan-grid">
        {CHANNELS.map(({ key, label }) => {
          const items = byChannel.get(key) ?? [];
          return (
            <div className="card chan" key={key}>
              <h4>
                {label} <span className="muted">({items.length})</span>
              </h4>
              {items.length === 0 ? (
                <div className="muted chan-idle">idle</div>
              ) : (
                <div className="chan-feed">
                  {items
                    .slice()
                    .reverse()
                    .map((sig, i) => (
                      <div className="chan-line" key={`${sig.ts}-${i}`}>
                        <span className="ts">{time(sig.ts)}</span>
                        <span className={`dir ${sig.direction}`}>{sig.direction === "in" ? "◂ in" : "▸ out"}</span>
                        <span className={`chan-body ${sig.final ? "" : "partial"}`}>{sig.body}</span>
                      </div>
                    ))}
                </div>
              )}
            </div>
          );
        })}
      </div>
    </div>
  );
}
