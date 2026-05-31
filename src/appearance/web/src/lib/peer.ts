// Peer identity. The backend routes outbound thoughts/audio by peer (X-HI-To),
// so the browser must have a stable identity. Per the design it is silent and
// not user-facing: a persisted default, no editor UI.
//
// On the first visit we mint a unique id and persist it, so each browser keeps
// a stable identity across reloads — used as X-HI-From — until explicitly
// changed via setPeer(). Falls back to a shared default only when storage is
// unavailable (e.g. private mode), where persistence isn't possible anyway.

const PEER_KEY = "hi-agent.peer";
const DEFAULT_PEER = "web@local";

function mintPeer(): string {
  const uuid =
    typeof crypto !== "undefined" && "randomUUID" in crypto
      ? crypto.randomUUID()
      : Math.random().toString(36).slice(2) + Date.now().toString(36);
  return `web-${uuid}@local`;
}

export function getPeer(): string {
  try {
    const stored = localStorage.getItem(PEER_KEY);
    if (stored) return stored;
    const minted = mintPeer();
    localStorage.setItem(PEER_KEY, minted);
    return minted;
  } catch {
    return DEFAULT_PEER;
  }
}

export function setPeer(peer: string): void {
  try {
    localStorage.setItem(PEER_KEY, peer);
  } catch {
    /* ignore — private mode etc. */
  }
}
