import { useCallback, useEffect, useMemo, useRef, useState } from "react";
import { postThought, subscribeThought } from "./channels/thought";
import {
  awaitApproval,
  postApproval,
  type ApprovalRequest,
} from "./channels/approval";
import { Message, type MessageData } from "./ui/Message";
import { Composer } from "./ui/Composer";
import { ApprovalModal } from "./ui/ApprovalModal";

const PEER_KEY = "hi-agent.peer";
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
    // ignore (private mode, etc.)
  }
}

type ConnState = "connecting" | "live" | "error";

let msgSeq = 0;
const nextMsgId = () => `m_${Date.now().toString(36)}_${(++msgSeq).toString(36)}`;

export function App() {
  const [peer, setPeer] = useState<string>(() => loadPeer());
  const [messages, setMessages] = useState<MessageData[]>([]);
  const [thoughtState, setThoughtState] = useState<ConnState>("connecting");
  const [approval, setApproval] = useState<ApprovalRequest | null>(null);
  const [error, setError] = useState<string | null>(null);

  const scrollerRef = useRef<HTMLDivElement | null>(null);

  // Persist peer changes.
  useEffect(() => {
    savePeer(peer);
  }, [peer]);

  // Autoscroll on new messages.
  useEffect(() => {
    const el = scrollerRef.current;
    if (!el) return;
    el.scrollTop = el.scrollHeight;
  }, [messages]);

  // ---- /thought subscription loop -----------------------------------------
  useEffect(() => {
    const ctrl = new AbortController();
    let cancelled = false;

    (async () => {
      while (!cancelled) {
        setThoughtState("connecting");
        try {
          // One subscription = one utterance. Build a single message from
          // its chunks, then re-subscribe on body-close.
          const id = nextMsgId();
          let accumulated = "";
          let started = false;

          for await (const chunk of subscribeThought({
            peer,
            signal: ctrl.signal,
          })) {
            if (cancelled) break;
            setThoughtState("live");
            accumulated += chunk.text;
            if (!started) {
              started = true;
              setMessages((m) => [
                ...m,
                {
                  id,
                  direction: "in",
                  from: chunk.from ?? "agent",
                  text: accumulated,
                  at: new Date().toISOString(),
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
          // Body closed cleanly. Loop and re-subscribe.
        } catch (err) {
          if (cancelled || ctrl.signal.aborted) break;
          setThoughtState("error");
          const msg = err instanceof Error ? err.message : String(err);
          setError(msg);
          // Backoff before re-subscribing on hard failure.
          await new Promise((r) => setTimeout(r, 1500));
        }
      }
    })();

    return () => {
      cancelled = true;
      ctrl.abort();
    };
  }, [peer]);

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
          // If null (empty body), just re-subscribe.
        } catch (err) {
          if (cancelled || ctrl.signal.aborted) break;
          // Soft-fail: log and re-subscribe after a delay.
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

  const statusLabel = useMemo(() => {
    switch (thoughtState) {
      case "connecting":
        return "connecting";
      case "live":
        return "live";
      case "error":
        return "offline";
    }
  }, [thoughtState]);

  return (
    <div
      style={{
        display: "flex",
        flexDirection: "column",
        height: "100dvh",
        maxWidth: 760,
        margin: "0 auto",
        background: "var(--bg)",
      }}
    >
      <Header peer={peer} onPeerChange={setPeer} status={statusLabel} />

      <main
        ref={scrollerRef}
        style={{
          flex: 1,
          overflowY: "auto",
          padding: "16px",
          display: "flex",
          flexDirection: "column",
          gap: 10,
        }}
      >
        {messages.length === 0 ? (
          <EmptyState peer={peer} />
        ) : (
          messages.map((m) => <Message key={m.id} msg={m} />)
        )}
      </main>

      {error && (
        <div
          role="status"
          style={{
            padding: "8px 14px",
            background: "rgba(185, 28, 28, 0.08)",
            color: "var(--danger)",
            fontSize: 13,
            borderTop: "1px solid var(--border)",
            fontFamily: "var(--font-mono)",
            whiteSpace: "pre-wrap",
          }}
        >
          {error}
        </div>
      )}

      <Composer onSend={sendThought} />

      {approval && (
        <ApprovalModal request={approval} onDecide={decideApproval} />
      )}
    </div>
  );
}

function Header({
  peer,
  onPeerChange,
  status,
}: {
  peer: string;
  onPeerChange: (next: string) => void;
  status: string;
}) {
  const [draft, setDraft] = useState(peer);
  useEffect(() => setDraft(peer), [peer]);

  const commit = () => {
    const next = draft.trim() || DEFAULT_PEER;
    if (next !== peer) onPeerChange(next);
    else setDraft(next);
  };

  return (
    <header
      style={{
        display: "flex",
        gap: 10,
        padding: "10px 14px",
        alignItems: "center",
        borderBottom: "1px solid var(--border)",
        background: "var(--bg-elevated)",
        position: "sticky",
        top: 0,
        zIndex: 10,
      }}
    >
      <div
        style={{
          display: "flex",
          alignItems: "center",
          gap: 8,
          fontWeight: 600,
        }}
      >
        <span
          aria-hidden
          style={{
            width: 8,
            height: 8,
            borderRadius: "50%",
            background:
              status === "live"
                ? "var(--ok)"
                : status === "connecting"
                  ? "var(--fg-muted)"
                  : "var(--danger)",
            display: "inline-block",
          }}
        />
        hi-agent
      </div>
      <span
        style={{
          fontSize: 12,
          color: "var(--fg-muted)",
          fontFamily: "var(--font-mono)",
        }}
      >
        {status}
      </span>
      <div style={{ flex: 1 }} />
      <label
        style={{
          display: "flex",
          alignItems: "center",
          gap: 6,
          fontSize: 12,
          color: "var(--fg-muted)",
        }}
      >
        <span style={{ fontFamily: "var(--font-mono)" }}>peer</span>
        <input
          type="text"
          value={draft}
          onChange={(e) => setDraft(e.target.value)}
          onBlur={commit}
          onKeyDown={(e) => {
            if (e.key === "Enter") {
              e.preventDefault();
              (e.currentTarget as HTMLInputElement).blur();
            }
          }}
          aria-label="peer identity"
          spellCheck={false}
          style={{
            padding: "5px 8px",
            borderRadius: 6,
            border: "1px solid var(--border)",
            background: "var(--bg)",
            color: "var(--fg)",
            fontFamily: "var(--font-mono)",
            fontSize: 12,
            width: 140,
            outline: "none",
          }}
        />
      </label>
    </header>
  );
}

function EmptyState({ peer }: { peer: string }) {
  return (
    <div
      style={{
        margin: "auto",
        textAlign: "center",
        color: "var(--fg-muted)",
        fontSize: 14,
        maxWidth: 360,
        display: "flex",
        flexDirection: "column",
        gap: 8,
        padding: 24,
      }}
    >
      <div style={{ fontSize: 15, color: "var(--fg)", fontWeight: 600 }}>
        listening on /thought
      </div>
      <div>
        you are <code style={{ fontFamily: "var(--font-mono)" }}>{peer}</code>.
        send a thought below, or wait for the agent to speak.
      </div>
    </div>
  );
}
