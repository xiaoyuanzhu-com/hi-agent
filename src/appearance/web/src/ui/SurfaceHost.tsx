import { useEffect, useState } from "react";
import type { SurfaceEnvelope } from "../channels/out/surface";

interface SurfaceHostProps {
  /** The surface to display, or null. New surfaces replace the current one. */
  surface: SurfaceEnvelope | null;
  onDismiss: () => void;
}

/**
 * Renders agent-authored HTML as an overlay over the calm core — a centered
 * `card` or a full-bleed `full` view. The HTML runs in a sandboxed iframe
 * (`allow-scripts` only, no same-origin) so it is isolated from the app.
 * Enter/exit are eased; a `card` dismisses on backdrop click, both via the ×.
 */
export function SurfaceHost({ surface, onDismiss }: SurfaceHostProps) {
  const [shown, setShown] = useState<SurfaceEnvelope | null>(surface);
  const [visible, setVisible] = useState(false);

  useEffect(() => {
    if (surface) {
      setShown(surface);
      const raf = requestAnimationFrame(() => setVisible(true));
      return () => cancelAnimationFrame(raf);
    }
    // exiting: fade out, then drop after the transition
    setVisible(false);
    const t = window.setTimeout(() => setShown(null), 340);
    return () => window.clearTimeout(t);
  }, [surface]);

  if (!shown) return null;
  const full = shown.mode === "full";

  return (
    <div
      className={`hi-surface ${full ? "hi-surface--full" : "hi-surface--card"}`}
      data-visible={visible}
      onClick={(e) => {
        if (!full && e.target === e.currentTarget) onDismiss();
      }}
    >
      <div className="hi-surface-frame">
        <iframe
          className="hi-surface-iframe"
          sandbox="allow-scripts"
          srcDoc={shown.html ?? ""}
          title="agent content"
        />
        <button className="hi-surface-close" onClick={onDismiss} aria-label="dismiss">
          ×
        </button>
      </div>
    </div>
  );
}
