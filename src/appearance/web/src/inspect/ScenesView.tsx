import { useEffect, useMemo, useRef, useState } from "react";
import { selectedUnder, usePath } from "./router";
import {
  subscribeChannels,
  subscribeEvents,
  type Channel,
  type ChannelSignal,
  type SceneView,
} from "./api";
import { subscribeAudioTurns } from "../channels/out/audio";
import { subscribeInAudioTurns } from "../channels/in/audio";
import { subscribeInVideo, type VideoInTurn } from "../channels/in/vision";
import { AudioBus } from "../lib/audioBus";
import { VoicePlayer } from "../lib/voicePlayer";
import { PcmPlayer } from "../lib/pcmMonitor";

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

/** Sample rate carried on a PCM mime (`audio/pcm;rate=16000;…`), else 16 kHz. */
function pcmRate(mime: string): number {
  const m = /rate=(\d+)/.exec(mime);
  return m ? parseInt(m[1]!, 10) : 16000;
}

/** Stream one audio turn's body into a VoicePlayer as MediaSource chunks. */
async function pumpVoice(
  voice: VoicePlayer,
  turn: { mime: string; body: ReadableStream<Uint8Array> },
  cancelled: () => boolean,
): Promise<void> {
  const token = voice.beginTurn(turn.mime);
  const reader = turn.body.getReader();
  try {
    while (!cancelled()) {
      const { value, done } = await reader.read();
      if (done) break;
      if (value) voice.pushChunk(token, value);
    }
  } finally {
    voice.endTurn(token);
    reader.releaseLock();
  }
}

/**
 * A monitor button for one audio channel: toggles live playback of the actual
 * audio bytes. Output (the agent's voice) and encoded input clips play through a
 * MediaSource `VoicePlayer`; the live mic arrives as raw PCM, played through a
 * `PcmPlayer`. The toggle click is the user gesture that lets the AudioContext
 * start. Everything is torn down on toggle-off, scene-switch, or unmount.
 */
function AudioMonitor({ scene, direction }: { scene: string; direction: "in" | "out" }) {
  const [on, setOn] = useState(false);
  const [failed, setFailed] = useState(false);

  useEffect(() => {
    if (!on) return;
    const ctrl = new AbortController();
    let cancelled = false;
    let bus: AudioBus | null = null;
    let voice: VoicePlayer | null = null;
    let pcm: PcmPlayer | null = null;
    const ensureVoice = (): VoicePlayer => {
      if (!bus) bus = new AudioBus();
      void bus.resume();
      if (!voice)
        voice = new VoicePlayer(
          bus,
          () => {},
          () => {},
        );
      return voice;
    };

    void (async () => {
      try {
        const turns =
          direction === "out"
            ? subscribeAudioTurns({ scene, signal: ctrl.signal })
            : subscribeInAudioTurns({ scene, signal: ctrl.signal });
        for await (const turn of turns) {
          if (cancelled) break;
          if (turn.mime.startsWith("audio/pcm")) {
            pcm = new PcmPlayer(pcmRate(turn.mime));
            const reader = turn.body.getReader();
            try {
              while (!cancelled) {
                const { value, done } = await reader.read();
                if (done) break;
                if (value) pcm.push(value);
              }
            } finally {
              reader.releaseLock();
            }
            pcm.stop();
            pcm = null;
          } else {
            await pumpVoice(ensureVoice(), turn, () => cancelled);
          }
        }
      } catch {
        if (!cancelled && !ctrl.signal.aborted) setFailed(true);
      }
    })();

    return () => {
      cancelled = true;
      ctrl.abort();
      voice?.stop();
      pcm?.stop();
      bus?.close();
    };
  }, [on, scene, direction]);

  return (
    <button
      type="button"
      className={`chan-monitor ${on ? "on" : ""} ${failed ? "failed" : ""}`}
      onClick={() => {
        setFailed(false);
        setOn((v) => !v);
      }}
      title={failed ? "audio unavailable" : on ? "stop listening" : "listen to this channel"}
    >
      {on ? "🔊" : "🔈"}
    </button>
  );
}

/**
 * Play one camera session into a `<video>` via MediaSource: open a MediaSource,
 * add a SourceBuffer for the session's mime, and append WebM chunks from the body
 * as they arrive (a small append-queue/pump, like `lib/voicePlayer.ts`, drained on
 * each `updateend`). Resolves when the session ends or playback is cancelled.
 */
async function playVideoSession(
  video: HTMLVideoElement,
  turn: VideoInTurn,
  cancelled: () => boolean,
): Promise<void> {
  const reader = turn.body.getReader();
  if (typeof MediaSource === "undefined" || !MediaSource.isTypeSupported(turn.mime)) {
    // Can't decode this codec here — drain the body so the loop moves on.
    while (!cancelled()) {
      const { done } = await reader.read();
      if (done) break;
    }
    reader.releaseLock();
    return;
  }

  const ms = new MediaSource();
  const url = URL.createObjectURL(ms);
  video.src = url;
  try {
    await new Promise<void>((resolve, reject) => {
      ms.addEventListener("sourceopen", () => resolve(), { once: true });
      ms.addEventListener("error", () => reject(new Error("MediaSource error")), { once: true });
    });
    if (cancelled()) return;

    const sb = ms.addSourceBuffer(turn.mime);
    // An observer almost always joins a camera that's been running a while, so
    // the media's timestamps start seconds in. "sequence" mode ignores those
    // timestamps and lays each appended chunk right after the last, so playback
    // starts at 0 and the <video> doesn't sit black waiting at currentTime 0.
    try {
      sb.mode = "sequence";
    } catch {
      /* some browsers fix the mode; segments mode still plays a from-start join */
    }
    const queue: Uint8Array[] = [];
    let ended = false;
    const pump = () => {
      if (sb.updating) return;
      const next = queue.shift();
      if (next) {
        try {
          sb.appendBuffer(next as unknown as BufferSource);
        } catch {
          queue.length = 0; // quota/parse error — drop the rest rather than wedge
        }
      } else if (ended && ms.readyState === "open") {
        try {
          ms.endOfStream();
        } catch {
          /* already ended */
        }
      }
    };
    sb.addEventListener("updateend", pump);
    void video.play().catch(() => {
      /* autoplay race; the next append/play retries */
    });

    // Temporary diagnostic: surface whether the <video> is decoding frames or
    // just sitting black (e.g. waiting for a keyframe after a mid-stream join).
    let appended = 0;
    let ticks = 0;
    const diag = setInterval(() => {
      const buf =
        video.buffered.length > 0
          ? `${video.buffered.start(0).toFixed(2)}–${video.buffered.end(video.buffered.length - 1).toFixed(2)}`
          : "none";
      // eslint-disable-next-line no-console
      console.log(
        `[vision diag] mime=${turn.mime} appended=${appended} readyState=${video.readyState} ` +
          `videoW=${video.videoWidth} t=${video.currentTime.toFixed(2)} paused=${video.paused} buffered=${buf} ` +
          `msState=${ms.readyState}`,
      );
      if (++ticks >= 8) clearInterval(diag);
    }, 1000);

    try {
      while (!cancelled()) {
        const { value, done } = await reader.read();
        if (done) break;
        if (value) {
          appended++;
          queue.push(value);
          pump();
        }
      }
    } finally {
      clearInterval(diag);
    }
    ended = true;
    pump();
  } finally {
    reader.releaseLock();
    URL.revokeObjectURL(url);
  }
}

/**
 * A monitor for the vision input channel: toggles a live view of the camera.
 * `GET /api/in/vision` streams one camera session (WebM) per response, which we
 * play into a `<video>` via MediaSource and re-GET for the next session. The
 * MediaSource + fetch are torn down on toggle-off / scene-switch / unmount.
 */
function VisionMonitor({ scene }: { scene: string }) {
  const [on, setOn] = useState(false);
  const [failed, setFailed] = useState(false);
  const videoRef = useRef<HTMLVideoElement | null>(null);

  useEffect(() => {
    if (!on) return;
    const video = videoRef.current;
    if (!video) return;
    const ctrl = new AbortController();
    let cancelled = false;

    void (async () => {
      try {
        for await (const turn of subscribeInVideo({ scene, signal: ctrl.signal })) {
          if (cancelled) break;
          await playVideoSession(video, turn, () => cancelled);
        }
      } catch (err) {
        if (!cancelled && !ctrl.signal.aborted) {
          // eslint-disable-next-line no-console
          console.warn("[vision monitor] playback failed:", err);
          setFailed(true);
        }
      }
    })();

    return () => {
      cancelled = true;
      ctrl.abort();
      try {
        video.removeAttribute("src");
        video.load();
      } catch {
        /* ignore */
      }
    };
  }, [on, scene]);

  return (
    <>
      <button
        type="button"
        className={`chan-monitor ${on ? "on" : ""} ${failed ? "failed" : ""}`}
        onClick={() => {
          setFailed(false);
          setOn((v) => !v);
        }}
        title={failed ? "vision unavailable" : on ? "stop watching" : "watch this camera"}
      >
        {on ? "📹" : "📷"}
      </button>
      {on && <video ref={videoRef} className="chan-frame" autoPlay muted playsInline />}
    </>
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
                  {key === "audio" && (
                    <div className="chan-monitor-row">
                      <AudioMonitor scene={scene} direction={dir} />
                    </div>
                  )}
                  {key === "vision" && dir === "in" && (
                    <div className="chan-monitor-row">
                      <VisionMonitor scene={scene} />
                    </div>
                  )}
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
