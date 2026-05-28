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
        background: "rgba(0, 0, 0, 0.45)",
        display: "flex",
        alignItems: "center",
        justifyContent: "center",
        padding: 16,
        zIndex: 50,
      }}
    >
      <div
        style={{
          width: "100%",
          maxWidth: 440,
          background: "var(--bg-elevated)",
          color: "var(--fg)",
          borderRadius: 14,
          boxShadow: "var(--shadow-md)",
          border: "1px solid var(--border)",
          padding: 20,
          display: "flex",
          flexDirection: "column",
          gap: 14,
        }}
      >
        <header style={{ display: "flex", flexDirection: "column", gap: 4 }}>
          <span
            style={{
              fontSize: 12,
              textTransform: "uppercase",
              letterSpacing: 0.6,
              color: "var(--fg-muted)",
              fontFamily: "var(--font-mono)",
            }}
          >
            approval requested
          </span>
          <h2
            id="approval-title"
            style={{ margin: 0, fontSize: 17, lineHeight: 1.3 }}
          >
            {request.action}
          </h2>
        </header>

        <p style={{ margin: 0, color: "var(--fg)", whiteSpace: "pre-wrap" }}>
          {request.summary}
        </p>

        {request.details && (
          <pre
            style={{
              margin: 0,
              padding: 10,
              borderRadius: 8,
              background: "var(--bg)",
              border: "1px solid var(--border)",
              fontSize: 12,
              fontFamily: "var(--font-mono)",
              whiteSpace: "pre-wrap",
              wordBreak: "break-word",
              maxHeight: 180,
              overflow: "auto",
            }}
          >
            {request.details}
          </pre>
        )}

        <label
          style={{
            display: "flex",
            flexDirection: "column",
            gap: 6,
            fontSize: 13,
            color: "var(--fg-muted)",
          }}
        >
          Reason (optional)
          <input
            type="text"
            value={reason}
            onChange={(e) => setReason(e.target.value)}
            placeholder="why you decided this way"
            disabled={busy}
            style={{
              padding: "8px 10px",
              borderRadius: "var(--radius)",
              border: "1px solid var(--border)",
              background: "var(--bg)",
              outline: "none",
              fontSize: 14,
            }}
          />
        </label>

        <footer
          style={{
            display: "flex",
            justifyContent: "flex-end",
            gap: 8,
            marginTop: 4,
          }}
        >
          <button
            type="button"
            disabled={busy}
            onClick={() => void decide(false)}
            style={{
              padding: "9px 14px",
              borderRadius: "var(--radius)",
              border: "1px solid var(--border)",
              background: "transparent",
              color: "var(--danger)",
              cursor: busy ? "not-allowed" : "pointer",
              fontWeight: 600,
            }}
          >
            Deny
          </button>
          <button
            type="button"
            disabled={busy}
            onClick={() => void decide(true)}
            style={{
              padding: "9px 14px",
              borderRadius: "var(--radius)",
              border: "none",
              background: "var(--ok)",
              color: "#ffffff",
              cursor: busy ? "not-allowed" : "pointer",
              fontWeight: 600,
            }}
          >
            Allow
          </button>
        </footer>

        <div
          style={{
            fontSize: 11,
            color: "var(--fg-muted)",
            fontFamily: "var(--font-mono)",
          }}
        >
          id: {request.id}
        </div>
      </div>
    </div>
  );
}
