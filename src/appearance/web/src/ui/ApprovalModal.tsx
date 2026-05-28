import { useState } from "react";
import type { ApprovalRequest } from "../channels/approval";

export interface ApprovalModalProps {
  request: ApprovalRequest;
  onDecide: (allow: boolean, reason?: string) => void | Promise<void>;
}

export function ApprovalModal({ request, onDecide }: ApprovalModalProps) {
  const [reason, setReason] = useState("");
  const [busy, setBusy] = useState(false);

  const decide = async (allow: boolean) => {
    if (busy) return;
    setBusy(true);
    try {
      await onDecide(allow, reason.trim() || undefined);
    } finally {
      setBusy(false);
    }
  };

  return (
    <div
      role="dialog"
      aria-modal="true"
      aria-labelledby="approval-title"
      style={{
        position: "fixed",
        inset: 0,
        background:
          "radial-gradient(ellipse at center, rgba(255, 78, 203, 0.14), rgba(2, 4, 12, 0.78) 60%)",
        backdropFilter: "blur(8px)",
        WebkitBackdropFilter: "blur(8px)",
        display: "grid",
        placeItems: "center",
        padding: 16,
        zIndex: 60,
        animation: "hi-flicker 320ms steps(1) 1",
      }}
    >
      <div
        className="glass"
        style={{
          position: "relative",
          width: "100%",
          maxWidth: 480,
          padding: "22px 22px 18px",
          borderRadius: 18,
          border: "1px solid rgba(255, 78, 203, 0.55)",
          boxShadow: "var(--glow-magenta)",
          display: "flex",
          flexDirection: "column",
          gap: 16,
        }}
      >
        <CornerBrackets color="var(--magenta)" />

        <header style={{ display: "flex", flexDirection: "column", gap: 6 }}>
          <span
            style={{
              fontSize: 10,
              letterSpacing: "0.32em",
              textTransform: "uppercase",
              color: "var(--magenta)",
              fontFamily: "var(--font-mono)",
              textShadow: "var(--glow-magenta)",
            }}
          >
            // approval required
          </span>
          <h2
            id="approval-title"
            style={{
              margin: 0,
              fontSize: 18,
              lineHeight: 1.3,
              color: "var(--fg)",
              letterSpacing: "0.01em",
              fontFamily: "var(--font-mono)",
            }}
          >
            {request.action}
          </h2>
        </header>

        <p
          style={{
            margin: 0,
            color: "var(--fg)",
            whiteSpace: "pre-wrap",
            fontSize: 14,
            lineHeight: 1.55,
          }}
        >
          {request.summary}
        </p>

        {request.details != null && (
          <pre
            style={{
              margin: 0,
              padding: 12,
              borderRadius: 10,
              background: "rgba(2, 6, 18, 0.65)",
              border: "1px solid var(--line)",
              fontSize: 12,
              fontFamily: "var(--font-mono)",
              color: "var(--fg-dim)",
              whiteSpace: "pre-wrap",
              wordBreak: "break-word",
              maxHeight: 200,
              overflow: "auto",
            }}
          >
            {typeof request.details === "string"
              ? request.details
              : JSON.stringify(request.details, null, 2)}
          </pre>
        )}

        <label
          style={{
            display: "flex",
            flexDirection: "column",
            gap: 6,
            fontSize: 10,
            color: "var(--fg-mute)",
            fontFamily: "var(--font-mono)",
            textTransform: "uppercase",
            letterSpacing: "0.24em",
          }}
        >
          rationale (optional)
          <input
            type="text"
            value={reason}
            onChange={(e) => setReason(e.target.value)}
            placeholder="why you decided this way"
            disabled={busy}
            style={{
              padding: "10px 12px",
              borderRadius: 10,
              border: "1px solid var(--line-strong)",
              background: "rgba(2, 6, 18, 0.55)",
              color: "var(--fg)",
              outline: "none",
              fontFamily: "var(--font-mono)",
              fontSize: 13,
              letterSpacing: "0.01em",
            }}
          />
        </label>

        <footer
          style={{
            display: "flex",
            justifyContent: "flex-end",
            gap: 10,
            marginTop: 2,
          }}
        >
          <DecisionButton
            tone="deny"
            busy={busy}
            onClick={() => void decide(false)}
          >
            Deny
          </DecisionButton>
          <DecisionButton
            tone="allow"
            busy={busy}
            onClick={() => void decide(true)}
          >
            Allow
          </DecisionButton>
        </footer>

        <div
          style={{
            fontSize: 10,
            color: "var(--fg-mute)",
            fontFamily: "var(--font-mono)",
            letterSpacing: "0.12em",
          }}
        >
          id · {request.id}
        </div>
      </div>
    </div>
  );
}

function DecisionButton({
  tone,
  busy,
  onClick,
  children,
}: {
  tone: "allow" | "deny";
  busy: boolean;
  onClick: () => void;
  children: React.ReactNode;
}) {
  const isAllow = tone === "allow";
  return (
    <button
      type="button"
      onClick={onClick}
      disabled={busy}
      style={{
        position: "relative",
        padding: "11px 22px",
        borderRadius: 10,
        fontFamily: "var(--font-mono)",
        fontSize: 12,
        letterSpacing: "0.18em",
        textTransform: "uppercase",
        color: isAllow ? "#04060d" : "var(--danger)",
        background: isAllow
          ? "linear-gradient(135deg, #5af6ff, #75ffd0)"
          : "transparent",
        border: `1px solid ${isAllow ? "var(--cyan-soft)" : "var(--danger)"}`,
        boxShadow: isAllow ? "var(--glow-cyan)" : "0 0 0 transparent",
        cursor: busy ? "not-allowed" : "pointer",
        transition: "all 220ms var(--ease-out)",
        fontWeight: 600,
      }}
    >
      {children}
    </button>
  );
}

function CornerBrackets({ color }: { color: string }) {
  const size = 14;
  const thickness = 1;
  const offset = 8;
  const style = (corner: "tl" | "tr" | "bl" | "br") => {
    const base: React.CSSProperties = {
      position: "absolute",
      width: size,
      height: size,
      pointerEvents: "none",
    };
    if (corner === "tl") return {
      ...base, top: offset, left: offset,
      borderTop: `${thickness}px solid ${color}`,
      borderLeft: `${thickness}px solid ${color}`,
    };
    if (corner === "tr") return {
      ...base, top: offset, right: offset,
      borderTop: `${thickness}px solid ${color}`,
      borderRight: `${thickness}px solid ${color}`,
    };
    if (corner === "bl") return {
      ...base, bottom: offset, left: offset,
      borderBottom: `${thickness}px solid ${color}`,
      borderLeft: `${thickness}px solid ${color}`,
    };
    return {
      ...base, bottom: offset, right: offset,
      borderBottom: `${thickness}px solid ${color}`,
      borderRight: `${thickness}px solid ${color}`,
    };
  };
  return (
    <>
      <span aria-hidden style={style("tl")} />
      <span aria-hidden style={style("tr")} />
      <span aria-hidden style={style("bl")} />
      <span aria-hidden style={style("br")} />
    </>
  );
}
