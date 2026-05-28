import { useCallback, useEffect, useMemo, useState } from "react";
import { postThought, subscribeThought } from "./channels/thought";
import {
  awaitApproval,
  postApproval,
  type ApprovalRequest,
} from "./channels/approval";
import { Composer } from "./ui/Composer";
import { ApprovalModal } from "./ui/ApprovalModal";
import { ParticleField } from "./ui/ParticleField";
import { Orb, type AgentState } from "./ui/Orb";
import { HUD, type StatusKind } from "./ui/HUD";
import { Transcript, type MessageData } from "./ui/Transcript";

const PEER_KEY = "hi-agent.peer";
const LOG_OPEN_KEY = "hi-agent.log-open";
const DEFAULT_PEER = "web@local";

function loadPeer(): string {
  try {
    return localStorage.getItem(PEER_KEY) || DEFAULT_PEER;
  } catch {
    return DEFAULT_PEER;
  }
}
function savePeer(peer: string) {
  try {
    localStorage.setItem(PEER_KEY, peer);
  } catch {
    /* ignore */
  }
}

function loadLogOpen(): boolean {
  try {
    return localStorage.getItem(LOG_OPEN_KEY) === "1";
  } catch {
    return false;
  }
}
function saveLogOpen(open: boolean) {
  try {
    localStorage.setItem(LOG_OPEN_KEY, open ? "1" : "0");
  } catch {
    /* ignore */
  }
}

let msgSeq = 0;
const nextMsgId = () => `m_${Date.now().toString(36)}_${(++msgSeq).toString(36)}`;

export function App() {
  const [peer, setPeer] = useState<string>(() => loadPeer());
  const [messages, setMessages] = useState<MessageData[]>([]);
  const [thoughtState, setThoughtState] = useState<StatusKind>("connecting");
  const [approval, setApproval] = useState<ApprovalRequest | null>(null);
  const [error, setError] = useState<string | null>(null);
  const [logOpen, setLogOpen] = useState<boolean>(() => loadLogOpen());
  const [streamingId, setStreamingId] = useState<string | null>(null);
  const [intensity, setIntensity] = useState(0);
  const [micOn, setMicOn] = useState(false);

  // Persist peer + log-open changes.
  useEffect(() => savePeer(peer), [peer]);
  useEffect(() => saveLogOpen(logOpen), [logOpen]);

  // Auto-dismiss surfaced error after a few seconds.
  useEffect(() => {
    if (!error) return;
    const id = window.setTimeout(() => setError(null), 5000);
    return () => window.clearTimeout(id);
  }, [error]);

  // ---- /thought subscription loop -----------------------------------------
  // Each iteration owns exactly one utterance. We start a fresh message
  // record on the first chunk, accumulate into it, and close it on body-end.
  useEffect(() => {
    const ctrl = new AbortController();
    let cancelled = false;

    (async () => {
      while (!cancelled) {
        setThoughtState("connecting");
        try {
          const id = nextMsgId();
          let accumulated = "";
          let started = false;
          let chunksThisUtterance = 0;
          let lastChunkAt = performance.now();

          for await (const chunk of subscribeThought({
            peer,
            signal: ctrl.signal,
          })) {
            if (cancelled) break;
            setThoughtState("live");
            accumulated += chunk.text;
            chunksThisUtterance += 1;
            // crude streaming-rate → orb intensity. Decays in another effect.
            const now = performance.now();
            const dt = Math.max(16, now - lastChunkAt);
            lastChunkAt = now;
            const burst = Math.min(1, 350 / dt);
            setIntensity((v) => Math.max(v, burst));

            if (!started) {
              started = true;
              setStreamingId(id);
              setMessages((m) => [
                ...m,
                {
                  id,
                  direction: "in",
                  from: chunk.from ?? "agent",
                  text: accumulated,
                  at: new Date().toISOString(),
                  streaming: true,
                },
              ]);
            } else {
              setMessages((m) =>
                m.map((msg) =>
                  msg.id === id ? { ...msg, text: accumulated } : msg,
                ),
              );
            }
          }
          // Body closed cleanly — finalize the message if we started one.
          if (started) {
            setMessages((m) =>
              m.map((msg) =>
                msg.id === id ? { ...msg, streaming: false } : msg,
              ),
            );
          }
          if (chunksThisUtterance > 0) setStreamingId(null);
        } catch (err) {
          if (cancelled || ctrl.signal.aborted) break;
          setThoughtState("error");
          const msg = err instanceof Error ? err.message : String(err);
          setError(msg);
          await new Promise((r) => setTimeout(r, 1500));
        }
      }
    })();

    return () => {
      cancelled = true;
      ctrl.abort();
    };
  }, [peer]);

  // Decay orb intensity smoothly between bursts so the visualization breathes.
  useEffect(() => {
    let raf = 0;
    let last = performance.now();
    const tick = (now: number) => {
      const dt = (now - last) / 1000;
      last = now;
      setIntensity((v) => Math.max(0, v - dt * 1.2));
      raf = requestAnimationFrame(tick);
    };
    raf = requestAnimationFrame(tick);
    return () => cancelAnimationFrame(raf);
  }, []);

  // ---- /approval subscription loop ----------------------------------------
  useEffect(() => {
    const ctrl = new AbortController();
    let cancelled = false;

    (async () => {
      while (!cancelled) {
        try {
          const req = await awaitApproval({ peer, signal: ctrl.signal });
          if (cancelled) break;
          if (req) setApproval(req);
        } catch (err) {
          if (cancelled || ctrl.signal.aborted) break;
          // eslint-disable-next-line no-console
          console.warn("[approval] subscribe error:", err);
          await new Promise((r) => setTimeout(r, 1500));
        }
      }
    })();

    return () => {
      cancelled = true;
      ctrl.abort();
    };
  }, [peer]);

  // ---- Actions ------------------------------------------------------------
  const sendThought = useCallback(
    async (text: string) => {
      try {
        await postThought({ from: peer, body: text });
        setMessages((m) => [
          ...m,
          {
            id: nextMsgId(),
            direction: "out",
            from: peer,
            text,
            at: new Date().toISOString(),
          },
        ]);
        setError(null);
      } catch (err) {
        const msg = err instanceof Error ? err.message : String(err);
        setError(msg);
      }
    },
    [peer],
  );

  const decideApproval = useCallback(
    async (allow: boolean, reason?: string) => {
      if (!approval) return;
      try {
        await postApproval(
          { id: approval.id, allow, reason },
          { from: peer },
        );
        setApproval(null);
      } catch (err) {
        const msg = err instanceof Error ? err.message : String(err);
        setError(msg);
      }
    },
    [approval, peer],
  );

  // ---- Derived state ------------------------------------------------------
  const orbState: AgentState = useMemo(() => {
    if (thoughtState === "error") return "offline";
    if (streamingId) return "speaking";
    if (micOn) return "listening";
    return "idle";
  }, [thoughtState, streamingId, micOn]);

  const channels = useMemo(
    () => [
      { name: "thought", supported: true, active: thoughtState === "live" || streamingId !== null },
      { name: "approval", supported: true, active: approval !== null },
      { name: "vision", supported: false },
      { name: "audio", supported: false, active: micOn },
      { name: "touch", supported: false },
      { name: "smell", supported: false },
      { name: "taste", supported: false },
    ],
    [thoughtState, streamingId, approval, micOn],
  );

  // The last in-flight message preview (under the orb) — shows the live
  // utterance even when the transcript log is closed.
  const livePreview = useMemo(() => {
    if (!streamingId) return null;
    return messages.find((m) => m.id === streamingId) ?? null;
  }, [streamingId, messages]);

  return (
    <div
      style={{
        position: "relative",
        width: "100vw",
        height: "100dvh",
        overflow: "hidden",
      }}
    >
      <ParticleField />
      <Orb state={orbState} intensity={intensity} />
      <GridFloor />

      <HUD
        peer={peer}
        onPeerChange={setPeer}
        status={thoughtState}
        channels={channels}
      />

      <CenterStage
        orbState={orbState}
        peer={peer}
        livePreview={livePreview?.text ?? null}
      />

      <Transcript
        messages={messages}
        open={logOpen}
        onToggle={() => setLogOpen((v) => !v)}
      />

      {error && <ErrorToast text={error} onClose={() => setError(null)} />}

      <Composer onSend={sendThought} onMicChange={setMicOn} />

      {approval && (
        <ApprovalModal request={approval} onDecide={decideApproval} />
      )}
    </div>
  );
}

/**
 * Subtle perspective grid at the bottom of the viewport — a sci-fi floor.
 * Rendered via two repeating linear-gradients with a perspective transform.
 */
function GridFloor() {
  return (
    <div
      aria-hidden
      style={{
        position: "fixed",
        left: "-20%",
        right: "-20%",
        bottom: 0,
        height: "55vh",
        perspective: 600,
        pointerEvents: "none",
        zIndex: -1,
        maskImage:
          "linear-gradient(to top, black 10%, transparent 90%)",
        WebkitMaskImage:
          "linear-gradient(to top, black 10%, transparent 90%)",
      }}
    >
      <div
        style={{
          position: "absolute",
          inset: 0,
          transform: "rotateX(62deg) translateZ(0)",
          transformOrigin: "50% 100%",
          backgroundImage:
            "linear-gradient(rgba(90, 246, 255, 0.16) 1px, transparent 1px),\n             linear-gradient(90deg, rgba(90, 246, 255, 0.16) 1px, transparent 1px)",
          backgroundSize: "60px 60px",
          opacity: 0.7,
        }}
      />
    </div>
  );
}

/**
 * Centerpiece text under the orb: state label + live preview of the
 * current utterance (visible even when the transcript log is hidden).
 */
function CenterStage({
  orbState,
  peer,
  livePreview,
}: {
  orbState: AgentState;
  peer: string;
  livePreview: string | null;
}) {
  const label = stateLabel(orbState);
  return (
    <div
      style={{
        position: "absolute",
        left: "50%",
        bottom: "calc(160px + env(safe-area-inset-bottom))",
        transform: "translateX(-50%)",
        width: "min(820px, calc(100vw - 32px))",
        textAlign: "center",
        zIndex: 5,
        pointerEvents: "none",
      }}
    >
      <div
        style={{
          fontFamily: "var(--font-mono)",
          fontSize: 11,
          letterSpacing: "0.42em",
          textTransform: "uppercase",
          color: orbState === "offline" ? "var(--danger)" : "var(--cyan)",
          textShadow:
            orbState === "offline"
              ? "var(--glow-magenta)"
              : "var(--glow-cyan-soft)",
        }}
      >
        {label}
      </div>
      {livePreview ? (
        <div
          style={{
            marginTop: 14,
            color: "var(--fg)",
            fontFamily: "var(--font-display)",
            fontSize: "clamp(15px, 2.2vw, 19px)",
            lineHeight: 1.55,
            letterSpacing: "0.01em",
            maxHeight: "22vh",
            overflow: "hidden",
            whiteSpace: "pre-wrap",
            wordBreak: "break-word",
            maskImage:
              "linear-gradient(to bottom, black 60%, transparent 100%)",
            WebkitMaskImage:
              "linear-gradient(to bottom, black 60%, transparent 100%)",
          }}
        >
          {livePreview}
          <span className="hi-caret" aria-hidden />
        </div>
      ) : (
        <div
          style={{
            marginTop: 14,
            color: "var(--fg-dim)",
            fontFamily: "var(--font-mono)",
            fontSize: 13,
            letterSpacing: "0.04em",
          }}
        >
          you are{" "}
          <span style={{ color: "var(--cyan)" }}>{peer}</span>
          {" · "}listening on /thought
        </div>
      )}
    </div>
  );
}

function stateLabel(s: AgentState): string {
  switch (s) {
    case "idle":      return "standby";
    case "listening": return "input // /audio (placeholder)";
    case "thinking":  return "processing";
    case "speaking":  return "transmitting";
    case "offline":   return "link severed";
  }
}

function ErrorToast({ text, onClose }: { text: string; onClose: () => void }) {
  return (
    <div
      role="status"
      onClick={onClose}
      style={{
        position: "fixed",
        top: 90,
        left: "50%",
        transform: "translateX(-50%)",
        zIndex: 40,
        padding: "10px 16px",
        borderRadius: 10,
        border: "1px solid var(--danger)",
        background: "rgba(20, 6, 14, 0.85)",
        backdropFilter: "var(--panel-blur)",
        WebkitBackdropFilter: "var(--panel-blur)",
        color: "var(--danger)",
        fontFamily: "var(--font-mono)",
        fontSize: 12,
        letterSpacing: "0.08em",
        boxShadow: "var(--glow-magenta)",
        cursor: "pointer",
        maxWidth: "min(640px, calc(100vw - 32px))",
        whiteSpace: "pre-wrap",
      }}
    >
      <span style={{ marginRight: 8, opacity: 0.7 }}>!!</span>
      {text}
    </div>
  );
}
