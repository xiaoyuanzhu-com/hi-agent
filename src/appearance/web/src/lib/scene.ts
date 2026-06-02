// Scene identity. A scene names the situation a signal belongs to — the
// context-isolation key the backend routes by (X-HI-Scene, the same token both
// inbound and on the GET long-polls). The browser must carry a stable scene so
// its signals and the agent's replies stay in one context. Per the design it is
// silent and not user-facing: a persisted default, no editor UI.
//
// On the first visit we mint a unique id and persist it, so each browser keeps a
// stable scene across reloads until explicitly changed via setScene(). Falls
// back to a shared default only when storage is unavailable (e.g. private mode),
// where persistence isn't possible anyway.

const SCENE_KEY = "hi-agent.scene";
const DEFAULT_SCENE = "web@local";

function mintScene(): string {
  const uuid =
    typeof crypto !== "undefined" && "randomUUID" in crypto
      ? crypto.randomUUID()
      : Math.random().toString(36).slice(2) + Date.now().toString(36);
  return `web-${uuid}@local`;
}

export function getScene(): string {
  try {
    const stored = localStorage.getItem(SCENE_KEY);
    if (stored) return stored;
    const minted = mintScene();
    localStorage.setItem(SCENE_KEY, minted);
    return minted;
  } catch {
    return DEFAULT_SCENE;
  }
}

export function setScene(scene: string): void {
  try {
    localStorage.setItem(SCENE_KEY, scene);
  } catch {
    /* ignore — private mode etc. */
  }
}
