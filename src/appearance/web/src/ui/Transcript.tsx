import { memo, useEffect, useRef } from "react";

export type MessageDirection = "in" | "out";

export interface MessageData {
  id: string;
  direction: MessageDirection;
  from?: string;
  text: string;
  /** Local timestamp, ISO string. */
  at: string;
  /** True while the agent is still streaming this utterance. */
  streaming?: boolean;
}

interface TranscriptProps {
  messages: MessageData[];
  open: boolean;
  onToggle: () => void;
}

/**
 * Right-side glass panel listing every utterance in this session.
 *
 * Renders as a fixed drawer on desktop (slides in/out) and a bottom sheet
 * on small screens. Each entry is a single block — there are no chat
 * bubbles, just monospace transcript lines with a colored direction
 * marker, sender handle, timestamp, and the message body.
 */
export function Transcript({ messages, open, onToggle }: TranscriptProps) {
  const scrollerRef = useRef<HTMLDivElement | null>(null);

  useEffect(() => {
    const el = scrollerRef.current;
    if (!el) return;
    el.scrollTop = el.scrollHeight;
  }, [messages]);

  return (
    <>
      <button
        type="button"
        onClick={onToggle}
        aria-expanded={open}
        aria-controls="hi-transcript"
        style={{
          position: "fixed",
          top: 16,
          right: open ? "calc(min(420px, 86vw) + 16px)" : 16,
          zIndex: 30,
          height: 38,
          padding: "0 14px",
          borderRadius: 999,
          border: "1px solid var(--line-strong)",
          color: "var(--cyan)",
          background:
            "linear-gradient(180deg, rgba(20,40,80,0.55), rgba(8,12,28,0.65))",
          backdropFilter: "var(--panel-blur)",
          WebkitBackdropFilter: "var(--panel-blur)",
          fontFamily: "var(--font-mono)",
          fontSize: 12,
          letterSpacing: "0.12em",
          textTransform: "uppercase",
          display: "inline-flex",
          alignItems: "center",
          gap: 8,
          boxShadow: "var(--glow-cyan-soft)",
          transition: "right 280ms var(--ease-out)",
        }}
      >
        <span
          aria-hidden
          style={{
            width: 6,
            height: 6,
            borderRadius: "50%",
            background: "var(--cyan)",
            boxShadow: "var(--glow-cyan-soft)",
          }}
        />
        {open ? "Hide log" : "Open log"}
        <span style={{ opacity: 0.55 }}>{messages.length.toString().padStart(3, "0")}</span>
      </button>

      <aside
        id="hi-transcript"
        aria-label="transcript"
        style={{
          position: "fixed",
          top: 0,
          right: 0,
          bottom: 0,
          width: "min(420px, 86vw)",
          padding: "16px 16px 16px 0",
          zIndex: 20,
          transform: open ? "translateX(0)" : "translateX(110%)",
          transition: "transform 320ms var(--ease-out)",
          display: "flex",
          flexDirection: "column",
          pointerEvents: open ? "auto" : "none",
        }}
      >
        <div
          className="glass"
          style={{
            flex: 1,
            display: "flex",
            flexDirection: "column",
            overflow: "hidden",
          }}
        >
          <header
            style={{
              padding: "14px 16px",
              borderBottom: "1px solid var(--line)",
              display: "flex",
              alignItems: "center",
              justifyContent: "space-between",
              gap: 12,
            }}
          >
            <div style={{ display: "flex", flexDirection: "column" }}>
              <span
                style={{
                  fontFamily: "var(--font-mono)",
                  fontSize: 10,
                  letterSpacing: "0.22em",
                  color: "var(--fg-mute)",
                  textTransform: "uppercase",
                }}
              >
                session log
              </span>
              <span
                style={{
                  fontFamily: "var(--font-mono)",
                  fontSize: 13,
                  color: "var(--fg)",
                  letterSpacing: "0.05em",
                }}
              >
                /thought · /approval
              </span>
            </div>
            <button
              type="button"
              onClick={onToggle}
              aria-label="close log"
              style={{
                width: 28,
                height: 28,
                borderRadius: 8,
                color: "var(--fg-dim)",
                border: "1px solid var(--line)",
                display: "grid",
                placeItems: "center",
              }}
            >
              ×
            </button>
          </header>

          <div
            ref={scrollerRef}
            style={{
              flex: 1,
              overflowY: "auto",
              padding: "12px 14px 18px",
              display: "flex",
              flexDirection: "column",
              gap: 14,
            }}
          >
            {messages.length === 0 ? (
              <TranscriptEmpty />
            ) : (
              messages.map((m) => <TranscriptEntry key={m.id} msg={m} />)
            )}
          </div>
        </div>
      </aside>
    </>
  );
}

function TranscriptEmpty() {
  return (
    <div
      style={{
        margin: "auto",
        color: "var(--fg-mute)",
        textAlign: "center",
        fontFamily: "var(--font-mono)",
        fontSize: 12,
        lineHeight: 1.6,
        letterSpacing: "0.08em",
        padding: "32px 12px",
      }}
    >
      <div style={{ color: "var(--fg-dim)", marginBottom: 6 }}>
        nothing yet
      </div>
      transcript will appear here as<br />
      utterances arrive on /thought
    </div>
  );
}

const TranscriptEntry = memo(function TranscriptEntry({
  msg,
}: {
  msg: MessageData;
}) {
  const isOut = msg.direction === "out";
  const dotColor = isOut ? "var(--magenta)" : "var(--cyan)";
  const labelColor = isOut ? "var(--magenta)" : "var(--cyan)";
  return (
    <article
      style={{
        display: "grid",
        gridTemplateColumns: "12px 1fr",
        gap: 10,
        alignItems: "start",
      }}
    >
      <div
        aria-hidden
        style={{
          width: 8,
          height: 8,
          borderRadius: 999,
          marginTop: 6,
          background: dotColor,
          boxShadow: `0 0 10px ${isOut ? "#ff4ecbaa" : "#5af6ffaa"}`,
        }}
      />
      <div style={{ display: "flex", flexDirection: "column", gap: 4 }}>
        <div
          style={{
            display: "flex",
            alignItems: "baseline",
            gap: 10,
            fontFamily: "var(--font-mono)",
            fontSize: 10,
            letterSpacing: "0.16em",
            textTransform: "uppercase",
          }}
        >
          <span style={{ color: labelColor }}>
            {isOut ? "you →" : "← agent"}
          </span>
          <span style={{ color: "var(--fg-mute)" }}>
            {msg.from ?? (isOut ? "self" : "agent")}
          </span>
          <span style={{ color: "var(--fg-mute)", marginLeft: "auto" }}>
            {fmtTime(msg.at)}
          </span>
        </div>
        <div
          style={{
            color: "var(--fg)",
            fontSize: 14,
            lineHeight: 1.55,
            whiteSpace: "pre-wrap",
            wordBreak: "break-word",
            fontFamily: isOut ? "var(--font-display)" : "var(--font-mono)",
          }}
        >
          {msg.text}
          {msg.streaming && <span className="hi-caret" aria-hidden />}
        </div>
      </div>
    </article>
  );
});

function fmtTime(iso: string): string {
  try {
    const d = new Date(iso);
    return d.toLocaleTimeString([], {
      hour: "2-digit",
      minute: "2-digit",
      second: "2-digit",
      hour12: false,
    });
  } catch {
    return "—";
  }
}
