// Peer identity. The backend routes outbound thoughts/audio by peer (X-HI-To),
// so the browser must have a stable identity. Per the design it is silent and
// not user-facing: a persisted default, no editor UI.

const PEER_KEY = "hi-agent.peer";
const DEFAULT_PEER = "web@local";

export function getPeer(): string {
  try {
    return localStorage.getItem(PEER_KEY) || DEFAULT_PEER;
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
