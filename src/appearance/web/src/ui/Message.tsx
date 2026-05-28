import { memo } from "react";

export type MessageDirection = "in" | "out";

export interface MessageData {
  id: string;
  direction: MessageDirection;
  /** Whoever the server stamps as the source, or our peer id for outbound. */
  from?: string;
  text: string;
  /** Local timestamp, ISO string. */
  at: string;
}

export const Message = memo(function Message({ msg }: { msg: MessageData }) {
  const isOut = msg.direction === "out";
  return (
    <article
      style={{
        display: "flex",
        flexDirection: "column",
        alignItems: isOut ? "flex-end" : "flex-start",
        gap: 4,
      }}
    >
      <div
        style={{
          maxWidth: "min(560px, 88%)",
          padding: "8px 12px",
          borderRadius: 12,
          background: isOut ? "var(--accent)" : "var(--bg-elevated)",
          color: isOut ? "var(--accent-fg)" : "var(--fg)",
          border: isOut ? "none" : "1px solid var(--border)",
          boxShadow: isOut ? "none" : "var(--shadow-sm)",
          whiteSpace: "pre-wrap",
          wordBreak: "break-word",
        }}
      >
        {msg.text}
      </div>
      <div
        style={{
          fontSize: 11,
          color: "var(--fg-muted)",
          fontFamily: "var(--font-mono)",
          padding: "0 4px",
        }}
      >
        {msg.from ?? (isOut ? "me" : "agent")} ·{" "}
        {new Date(msg.at).toLocaleTimeString()}
      </div>
    </article>
  );
});
