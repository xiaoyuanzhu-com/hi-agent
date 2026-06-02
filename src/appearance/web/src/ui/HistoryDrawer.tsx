import { useState } from "react";
import type { SurfaceEnvelope } from "../channels/out/surface";

interface HistoryDrawerProps {
  /** Past surfaces, oldest first. */
  surfaces: SurfaceEnvelope[];
  onOpen: (surface: SurfaceEnvelope) => void;
}

/**
 * Unobtrusive pull-up to recall rich content the agent showed earlier. Closed
 * by default (the interface stays ephemeral); a small handle reveals a list of
 * past surfaces, newest first, each re-openable.
 */
export function HistoryDrawer({ surfaces, onOpen }: HistoryDrawerProps) {
  const [open, setOpen] = useState(false);
  const items = [...surfaces].reverse();

  return (
    <div className={`hi-history ${open ? "hi-history--open" : ""}`}>
      <button
        className="hi-history-handle"
        onClick={() => setOpen((v) => !v)}
        aria-expanded={open}
        aria-label={open ? "hide past content" : "show past content"}
      >
        <span className="hi-history-grip" />
        {items.length} shown
      </button>

      {open && (
        <ul className="hi-history-list">
          {items.map((s, i) => (
            <li key={s.id}>
              <button className="hi-history-item" onClick={() => onOpen(s)}>
                <span className="hi-history-tag">{s.mode ?? "card"}</span>
                <span className="hi-history-label">
                  surface {items.length - i}
                </span>
              </button>
            </li>
          ))}
        </ul>
      )}
    </div>
  );
}
