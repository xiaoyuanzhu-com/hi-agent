import { useEffect, useRef, useState } from "react";

interface KeyboardFallbackProps {
  onSend: (text: string) => void;
  /** Whether the text channel is on (input line shown). Persisted by the hook. */
  open: boolean;
  /** Text pasted while the channel is open but focus is outside the input. */
  pastedText?: { id: number; text: string } | null;
  /** Turn the channel on — e.g. the user started typing while it was off. */
  onOpen: () => void;
  /** Turn the channel off — e.g. Esc. */
  onClose: () => void;
}

/**
 * The text input channel. When on, a minimal single line is shown that posts to
 * /thought; sending leaves it open (it's a channel, not a one-shot). When off,
 * the interface stays clean — but pressing any printable key turns it on and
 * seeds the first character, so a keyboard user never has to reach for a button.
 * Independent of the audio channels: usable with the mic on, off, or unavailable.
 */
export function KeyboardFallback({ onSend, open, pastedText, onOpen, onClose }: KeyboardFallbackProps) {
  const [text, setText] = useState("");
  const inputRef = useRef<HTMLInputElement | null>(null);
  const lastPasteIdRef = useRef(0);

  // Start-typing-to-open: a single printable key turns the channel on and seeds
  // the line. Only active while the channel is off.
  useEffect(() => {
    if (open) return;
    const onKey = (e: KeyboardEvent) => {
      if (e.metaKey || e.ctrlKey || e.altKey) return;
      if (e.key.length === 1 && /\S/.test(e.key)) {
        setText(e.key);
        onOpen();
      }
    };
    window.addEventListener("keydown", onKey);
    return () => window.removeEventListener("keydown", onKey);
  }, [open, onOpen]);

  useEffect(() => {
    if (open) inputRef.current?.focus();
  }, [open]);

  useEffect(() => {
    if (!open || !pastedText || pastedText.id === lastPasteIdRef.current) return;
    lastPasteIdRef.current = pastedText.id;
    setText((prev) => prev + pastedText.text);
    inputRef.current?.focus();
  }, [open, pastedText]);

  const submit = () => {
    const trimmed = text.trim();
    if (trimmed) onSend(trimmed);
    setText(""); // clear, but keep the channel open
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
            setText("");
            onClose();
          }
        }}
        placeholder="type to the agent…"
        aria-label="message the agent"
      />
    </div>
  );
}
