interface WakeGateProps {
  onWake: () => void;
  /** Shown if mic/audio could not be acquired on the first attempt. */
  error?: string | null;
  busy?: boolean;
}

/**
 * First-run gate. Browsers require a user gesture to grant the mic and unlock
 * audio playback, so the single "tap to begin" is the one unavoidable
 * interaction; after it the session is hands-free.
 */
export function WakeGate({ onWake, error, busy }: WakeGateProps) {
  return (
    <button
      type="button"
      className="hi-wake"
      onClick={onWake}
      disabled={busy}
      aria-label="tap to begin listening"
    >
      <span className="hi-wake-dot" />
      <span className="hi-wake-label">
        {error ? error : busy ? "waking…" : "tap to begin"}
      </span>
    </button>
  );
}
