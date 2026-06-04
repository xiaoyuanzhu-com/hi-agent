import { useEffect, useMemo, useState } from "react";
import { selectedUnder, usePath } from "./router";
import {
  subscribeChannels,
  subscribeEvents,
  type Channel,
  type ChannelSignal,
  type SceneView,
} from "./api";

const BASE = "/inspect/scenes";
const MAX_PER_CHANNEL = 200;

// Channels shown in the detail view, in display order. A card per channel lets
// an operator see the expressive/sensory surface — including the quiet ones —
// not only the channels that happen to be active. Touch/smell/taste are hidden
// for now until they carry real signal.
const CHANNELS: { key: Channel; label: string }[] = [
  { key: "text", label: "Text" },
  { key: "audio", label: "Audio" },
  { key: "vision", label: "Vision" },
];

// Top-level sectioning: the view splits into what the agent perceives (in) and
// what it expresses (out), and each section lays out the channels beneath it.
const DIRECTIONS: { key: ChannelSignal["direction"]; label: string }[] = [
  { key: "in", label: "Input" },
  { key: "out", label: "Output" },
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

  // The live scene roster rides the snapshot frames on the lifecycle SSE — one
  // connection, no polling. The Scenes tab ignores the lifecycle events
  // themselves; it only needs the per-scene list the snapshot carries.
  useEffect(() => {
    return subscribeEvents({
      onSnapshot: (data) => {
        data.sort((a, b) => a.scene.localeCompare(b.scene));
        setScenes(data);
      },
    });
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

  // Bucket per (direction, channel) so each section's channel card draws only
  // its own side of the conversation.
  const byCell = useMemo(() => {
    const m = new Map<string, ChannelSignal[]>();
    for (const sig of signals) {
      const cell = `${sig.direction}:${sig.channel}`;
      const arr = m.get(cell) ?? [];
      arr.push(sig);
      if (arr.length > MAX_PER_CHANNEL) arr.shift();
      m.set(cell, arr);
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

      {DIRECTIONS.map(({ key: dir, label: dirLabel }) => (
        <section className={`chan-section ${dir}`} key={dir}>
          <h3 className="chan-section-title">{dirLabel}</h3>
          <div className="chan-grid">
            {CHANNELS.map(({ key, label }) => {
              const items = byCell.get(`${dir}:${key}`) ?? [];
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
                            <span className={`chan-body ${sig.final ? "" : "partial"}`}>{sig.body}</span>
                          </div>
                        ))}
                    </div>
                  )}
                </div>
              );
            })}
          </div>
        </section>
      ))}
    </div>
  );
}
