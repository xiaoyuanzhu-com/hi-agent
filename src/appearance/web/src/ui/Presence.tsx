import type { CSSProperties } from "react";

export type PresenceState =
  | "waking"
  | "idle"
  | "listening"
  | "thinking"
  | "speaking"
  | "offline";

interface PresenceProps {
  state: PresenceState;
  /** 0..1 — how much a content overlay is up; the glow recedes as this rises so
   *  words and cards stay legible. */
  demote?: number;
}

/**
 * The agent's presence — a calm, matte, static skin.
 *
 * The background is the theme's warm paper (a soft gradient) with a single very
 * faint state-glow breathing into it: sage while the human holds the floor,
 * amber while thinking, terracotta while speaking, near-nothing at rest. It is
 * all CSS — the colours come from the `--glow-*` / `--bg-*` theme tokens (see
 * global.css), so re-skinning (or the light/dark swap) is a token change alone.
 * The glow colour cross-fades on state change; `demote` fades it back when a
 * content surface is up.
 *
 * (Replaces the earlier WebGL watercolour field — no canvas, no audio, no
 * per-frame loop; honours `prefers-reduced-motion` via the global rule, which
 * simply stops the slow breath and leaves a static glow.)
 */
export function Presence({ state, demote = 0 }: PresenceProps) {
  const presence = 1 - demote * 0.6;
  return (
    <div aria-hidden className="hi-presence" data-state={state}>
      <div
        className="hi-presence-glow"
        style={{ "--presence": presence.toFixed(3) } as CSSProperties}
      />
    </div>
  );
}
