interface WakeGateProps {
  onWake: () => void;
  /** Enter the session text-only — no mic prompt. */
  onTextOnly?: () => void;
  /** Shown if mic/audio could not be acquired on the first attempt. */
  error?: string | null;
  busy?: boolean;
}

/**
 * First-run gate. Browsers require a user gesture to unlock audio playback, so a
 * single tap is the one unavoidable interaction. The primary tap also turns on
 * the mic; a quieter "type instead" enters the session text-only (no mic
 * prompt), since audio and text are independent channels — either is enough to
 * begin, and the other can be toggled on later.
 */
export function WakeGate({ onWake, onTextOnly, error, busy }: WakeGateProps) {
  return (
    <div className="hi-wake">
      <button
        type="button"
        className="hi-wake-primary"
        onClick={onWake}
        disabled={busy}
        aria-label="tap to begin listening"
      >
        <span className="hi-wake-dot" />
        <span className="hi-wake-label">
          {error ? error : busy ? "waking…" : "tap to begin"}
        </span>
      </button>

      {onTextOnly && !busy && (
        <button
          type="button"
          className="hi-wake-alt"
          onClick={onTextOnly}
          aria-label="enter and type instead of speaking"
        >
          type instead
        </button>
      )}
    </div>
  );
}
