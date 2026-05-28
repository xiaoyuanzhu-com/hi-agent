import { useState, type KeyboardEvent, type FormEvent } from "react";

export interface ComposerProps {
  onSend: (text: string) => void | Promise<void>;
  disabled?: boolean;
  placeholder?: string;
}

export function Composer({ onSend, disabled, placeholder }: ComposerProps) {
  const [text, setText] = useState("");
  const [busy, setBusy] = useState(false);

  const submit = async () => {
    const trimmed = text.trim();
    if (trimmed.length === 0 || busy || disabled) return;
    setBusy(true);
    try {
      await onSend(trimmed);
      setText("");
    } finally {
      setBusy(false);
    }
  };

  const onFormSubmit = (e: FormEvent) => {
    e.preventDefault();
    void submit();
  };

  const onKeyDown = (e: KeyboardEvent<HTMLTextAreaElement>) => {
    // Enter sends, Shift+Enter inserts a newline.
    if (e.key === "Enter" && !e.shiftKey) {
      e.preventDefault();
      void submit();
    }
  };

  return (
    <form
      onSubmit={onFormSubmit}
      style={{
        display: "flex",
        gap: 8,
        padding: 12,
        background: "var(--bg-elevated)",
        borderTop: "1px solid var(--border)",
        alignItems: "flex-end",
      }}
    >
      <textarea
        value={text}
        onChange={(e) => setText(e.target.value)}
        onKeyDown={onKeyDown}
        placeholder={placeholder ?? "Send a thought…"}
        rows={1}
        disabled={disabled || busy}
        style={{
          flex: 1,
          resize: "none",
          minHeight: 40,
          maxHeight: 160,
          padding: "10px 12px",
          borderRadius: "var(--radius)",
          border: "1px solid var(--border)",
          background: "var(--bg)",
          outline: "none",
          fontFamily: "inherit",
          fontSize: 15,
          lineHeight: 1.4,
        }}
      />
      <button
        type="submit"
        disabled={disabled || busy || text.trim().length === 0}
        style={{
          padding: "10px 16px",
          borderRadius: "var(--radius)",
          border: "none",
          background:
            disabled || text.trim().length === 0
              ? "var(--border)"
              : "var(--accent)",
          color: "var(--accent-fg)",
          cursor:
            disabled || busy || text.trim().length === 0
              ? "not-allowed"
              : "pointer",
          fontWeight: 600,
        }}
      >
        {busy ? "Sending…" : "Send"}
      </button>
    </form>
  );
}
