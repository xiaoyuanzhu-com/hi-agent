// Scene identity. A scene names the situation a signal belongs to — the
// context-isolation key the backend routes by (X-HI-Scene, the same token both
// inbound and on the GET long-polls). The browser must carry a stable scene so
// its signals and the agent's replies stay in one context. Per the design it is
// silent and not user-facing: a persisted default, no editor UI.
//
// On the first visit we mint a unique id and persist it, so each browser keeps a
// stable scene across reloads until explicitly changed via setScene(). When
// storage is unavailable (e.g. private mode) we still mint a valid id for the
// session — it just won't survive a reload.

const SCENE_KEY = "hi-agent.scene";

// A short, plain scene id: 8 chars of [a-z0-9], always starting with a letter.
function mintScene(): string {
  const letters = "abcdefghijklmnopqrstuvwxyz";
  const alphanum = letters + "0123456789";
  const pick = (set: string) => set[Math.floor(Math.random() * set.length)];
  let id = pick(letters);
  for (let i = 1; i < 8; i++) id += pick(alphanum);
  return id;
}

export function getScene(): string {
  try {
    const stored = localStorage.getItem(SCENE_KEY);
    if (stored) return stored;
  } catch {
    /* storage unavailable — fall through to a fresh, unpersisted id */
  }
  const minted = mintScene();
  setScene(minted);
  return minted;
}

export function setScene(scene: string): void {
  try {
    localStorage.setItem(SCENE_KEY, scene);
  } catch {
    /* ignore — private mode etc. */
  }
}
