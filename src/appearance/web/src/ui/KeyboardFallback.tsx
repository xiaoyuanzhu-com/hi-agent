import { useEffect, useRef, useState } from "react";

interface KeyboardFallbackProps {
  onSend: (text: string) => void;
}

/**
 * Hidden text path for noisy rooms / accessibility. The interface has no input
 * box by default; pressing any printable key reveals a minimal single line that
 * posts to /thought. Esc or an empty blur dismisses it.
 */
export function KeyboardFallback({ onSend }: KeyboardFallbackProps) {
  const [open, setOpen] = useState(false);
  const [text, setText] = useState("");
  const inputRef = useRef<HTMLInputElement | null>(null);

  useEffect(() => {
    const onKey = (e: KeyboardEvent) => {
      if (open) return;
      if (e.metaKey || e.ctrlKey || e.altKey) return;
      // a single printable, non-whitespace character opens the line and seeds it
      if (e.key.length === 1 && /\S/.test(e.key)) {
        setText(e.key);
        setOpen(true);
      }
    };
    window.addEventListener("keydown", onKey);
    return () => window.removeEventListener("keydown", onKey);
  }, [open]);

  useEffect(() => {
    if (open) inputRef.current?.focus();
  }, [open]);

  const close = () => {
    setText("");
    setOpen(false);
  };
  const submit = () => {
    const trimmed = text.trim();
    if (trimmed) onSend(trimmed);
    close();
  };

  if (!open) return null;

  return (
    <div className="hi-kbd">
      <input
        ref={inputRef}
        value={text}
        spellCheck={false}
        onChange={(e) => setText(e.target.value)}
        onKeyDown={(e) => {
          if (e.key === "Enter") {
            e.preventDefault();
            submit();
          } else if (e.key === "Escape") {
            e.preventDefault();
            close();
          }
        }}
        onBlur={() => {
          if (!text.trim()) close();
        }}
        placeholder="type to the agent…"
        aria-label="message the agent"
      />
    </div>
  );
}
