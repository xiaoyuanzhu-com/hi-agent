// Native (desktop) ↔ web bridge.
//
// The macOS app hosts this page in a WKWebView that is *reused*, not torn down:
// closing the window sends the app to the background and hides the view, but the
// page — and its whole React tree — keeps running so a reopen is instant (see
// src/foundation/vendors/macos_window.rs). That warmth is the point, but it means
// the page can't lean on load/unmount to know when it's on screen. The native
// shell tells it instead, by dispatching a `hi:lifecycle` CustomEvent whenever the
// app moves between foreground and background. The page listens and pauses/restores
// things a hidden window shouldn't keep doing — first among them holding the
// microphone and camera open.
//
// In a plain browser tab nobody emits these events, so every subscriber here is
// simply inert and the tab's own unmount handles teardown as before.

/** Which way the desktop app just moved relative to the screen. */
export type LifecyclePhase = "foreground" | "background";

/** The DOM event the native shell dispatches for a lifecycle transition. */
const LIFECYCLE_EVENT = "hi:lifecycle";

/**
 * Subscribe to native foreground/background transitions. Returns an unsubscribe
 * function (drop it in a React effect cleanup). No-op in a browser tab, where the
 * event is never dispatched.
 */
export function onNativeLifecycle(
  handler: (phase: LifecyclePhase) => void,
): () => void {
  const listener = (e: Event) => {
    const phase = (e as CustomEvent<{ phase?: LifecyclePhase }>).detail?.phase;
    if (phase === "foreground" || phase === "background") handler(phase);
  };
  window.addEventListener(LIFECYCLE_EVENT, listener);
  return () => window.removeEventListener(LIFECYCLE_EVENT, listener);
}
