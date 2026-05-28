import {
  useEffect,
  useRef,
  useState,
  type FormEvent,
  type KeyboardEvent,
} from "react";
import { Waveform } from "./Waveform";

export interface ComposerProps {
  onSend: (text: string) => void | Promise<void>;
  disabled?: boolean;
  placeholder?: string;
  /** Called when the user starts/stops the mic placeholder. */
  onMicChange?: (recording: boolean) => void;
}

/**
 * Bottom-center pill composer.
 *
 * Three controls, left to right:
 *   * Mic button — PLACEHOLDER for the /audio channel. Clicking enters a
 *     fake "recording" state, animates a waveform inside the pill, and
 *     after a short delay drops back to idle. No actual audio is captured.
 *   * Multiline textarea — autosizes. Enter sends, Shift+Enter newlines.
 *   * Send button — submits the trimmed text via onSend.
 */
export function Composer({
  onSend,
  disabled,
  placeholder,
  onMicChange,
}: ComposerProps) {
  const [text, setText] = useState("");
  const [busy, setBusy] = useState(false);
  const [focused, setFocused] = useState(false);
  const [recording, setRecording] = useState(false);
  const taRef = useRef<HTMLTextAreaElement | null>(null);
  const stopTimerRef = useRef<number | null>(null);

  // Autosize the textarea up to a max height.
  useEffect(() => {
    const el = taRef.current;
    if (!el) return;
    el.style.height = "0px";
    el.style.height = `${Math.min(el.scrollHeight, 180)}px`;
  }, [text]);

  useEffect(() => {
    onMicChange?.(recording);
  }, [recording, onMicChange]);

  useEffect(() => {
    return () => {
      if (stopTimerRef.current) window.clearTimeout(stopTimerRef.current);
    };
  }, []);

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
    if (e.key === "Enter" && !e.shiftKey) {
      e.preventDefault();
      void submit();
    }
  };

  const toggleMic = () => {
    // Placeholder for the /audio channel — flip a recording state, auto-stop
    // after a few seconds. No audio is actually captured or transmitted.
    setRecording((r) => {
      const next = !r;
      if (stopTimerRef.current) {
        window.clearTimeout(stopTimerRef.current);
        stopTimerRef.current = null;
      }
      if (next) {
        stopTimerRef.current = window.setTimeout(() => {
          setRecording(false);
          stopTimerRef.current = null;
        }, 4200);
      }
      return next;
    });
  };

  const canSend = text.trim().length > 0 && !busy && !disabled;

  return (
    <form
      onSubmit={onFormSubmit}
      style={{
        position: "fixed",
        left: "50%",
        bottom: "max(24px, env(safe-area-inset-bottom))",
        transform: "translateX(-50%)",
        width: "min(720px, calc(100vw - 32px))",
        zIndex: 25,
      }}
    >
      <div
        className="glass"
        style={{
          display: "grid",
          gridTemplateColumns: "auto 1fr auto",
          alignItems: "end",
          gap: 8,
          padding: 8,
          borderRadius: 999,
          border: `1px solid ${focused ? "var(--cyan-soft)" : "var(--line-strong)"}`,
          boxShadow: focused
            ? "var(--glow-cyan)"
            : "0 12px 40px rgba(0, 0, 0, 0.55)",
          transition: "box-shadow 220ms var(--ease-out), border-color 220ms var(--ease-out)",
        }}
      >
        <MicButton recording={recording} onClick={toggleMic} disabled={disabled} />

        <div
          style={{
            position: "relative",
            display: "flex",
            alignItems: "center",
            minHeight: 44,
            padding: "0 6px",
          }}
        >
          {recording ? (
            <Waveform
              intensity={1}
              bars={36}
              width="100%"
              height={28}
              color="var(--magenta)"
              ariaLabel="voice input (placeholder)"
            />
          ) : (
            <textarea
              ref={taRef}
              value={text}
              onChange={(e) => setText(e.target.value)}
              onKeyDown={onKeyDown}
              onFocus={() => setFocused(true)}
              onBlur={() => setFocused(false)}
              placeholder={placeholder ?? "transmit a thought…"}
              rows={1}
              disabled={disabled || busy}
              style={{
                flex: 1,
                width: "100%",
                minHeight: 28,
                maxHeight: 180,
                resize: "none",
                padding: "8px 4px",
                border: "none",
                background: "transparent",
                color: "var(--fg)",
                outline: "none",
                fontFamily: "var(--font-mono)",
                fontSize: 14,
                letterSpacing: "0.01em",
                lineHeight: 1.5,
              }}
            />
          )}
        </div>

        <SendButton canSend={canSend} busy={busy} />
      </div>
      {recording && (
        <div
          style={{
            marginTop: 8,
            textAlign: "center",
            fontFamily: "var(--font-mono)",
            fontSize: 10,
            letterSpacing: "0.24em",
            textTransform: "uppercase",
            color: "var(--magenta)",
            textShadow: "var(--glow-magenta)",
          }}
        >
          /audio · placeholder · capture not wired
        </div>
      )}
    </form>
  );
}

function MicButton({
  recording,
  onClick,
  disabled,
}: {
  recording: boolean;
  onClick: () => void;
  disabled?: boolean;
}) {
  const color = recording ? "var(--magenta)" : "var(--cyan)";
  return (
    <button
      type="button"
      onClick={onClick}
      disabled={disabled}
      aria-pressed={recording}
      aria-label={recording ? "stop voice capture" : "start voice capture"}
      title="placeholder — /audio channel not wired in v0"
      style={{
        width: 44,
        height: 44,
        borderRadius: 999,
        display: "grid",
        placeItems: "center",
        color,
        border: `1px solid ${recording ? "var(--magenta)" : "var(--line-strong)"}`,
        background: recording
          ? "rgba(255, 78, 203, 0.12)"
          : "rgba(90, 246, 255, 0.06)",
        boxShadow: recording ? "var(--glow-magenta)" : "var(--glow-cyan-soft)",
        transition: "all 200ms var(--ease-out)",
      }}
    >
      <svg width="18" height="18" viewBox="0 0 24 24" fill="none" aria-hidden>
        <rect x="9" y="3" width="6" height="13" rx="3" fill="currentColor" />
        <path
          d="M5 11a7 7 0 0 0 14 0"
          stroke="currentColor"
          strokeWidth="1.6"
          strokeLinecap="round"
        />
        <path
          d="M12 18v3"
          stroke="currentColor"
          strokeWidth="1.6"
          strokeLinecap="round"
        />
      </svg>
    </button>
  );
}

function SendButton({ canSend, busy }: { canSend: boolean; busy: boolean }) {
  return (
    <button
      type="submit"
      disabled={!canSend}
      aria-label={busy ? "transmitting" : "transmit"}
      style={{
        width: 44,
        height: 44,
        borderRadius: 999,
        display: "grid",
        placeItems: "center",
        color: canSend ? "#04060d" : "var(--fg-mute)",
        background: canSend
          ? "linear-gradient(135deg, #5af6ff, #75ffd0)"
          : "rgba(120, 180, 255, 0.08)",
        border: `1px solid ${canSend ? "var(--cyan-soft)" : "var(--line)"}`,
        boxShadow: canSend ? "var(--glow-cyan)" : "none",
        cursor: canSend ? "pointer" : "not-allowed",
        transition: "all 200ms var(--ease-out)",
      }}
    >
      {busy ? (
        <span
          aria-hidden
          style={{
            width: 14,
            height: 14,
            border: "2px solid currentColor",
            borderRightColor: "transparent",
            borderRadius: "50%",
            animation: "hi-spin 0.8s linear infinite",
          }}
        />
      ) : (
        <svg width="18" height="18" viewBox="0 0 24 24" fill="none" aria-hidden>
          <path
            d="M4 12l16-8-5 18-3-7-8-3z"
            stroke="currentColor"
            strokeWidth="1.6"
            strokeLinejoin="round"
            fill="currentColor"
            fillOpacity="0.18"
          />
        </svg>
      )}
    </button>
  );
}
