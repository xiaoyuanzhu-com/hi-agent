import {
  createContext,
  useCallback,
  useContext,
  useEffect,
  useMemo,
  useRef,
  useState,
  type ReactNode,
} from "react";
import { subscribeViewState, clearViewState } from "../channels/out/view";
import { usePresence, useScene, useWake } from "./session";

// How long a newly appearing view waits for the voice before showing anyway.
// The view is paced to its narration, but the /view and /audio channels have
// very different latencies, so a view tends to land a beat ahead of the speech
// it belongs to. Holding it until the voice is audibly playing closes that gap;
// this fallback ensures a silent or text-only turn (no voice to wait for) still
// shows the view promptly rather than stalling on a beat that never sounds.
const VOICE_GATE_FALLBACK_MS = 1000;

/** One mounted agent view: a stable id and the compiled module URL to import. */
export interface ActiveView {
  id: string;
  moduleUrl: string;
}

/** Where the host should dock the live caption words while a view is on stage.
 * `"self"` = the view renders the words itself (via `useSpeech()`); the host
 * stands down. Declared by the view module as `export const captionAside`. */
export type CaptionAside = "top" | "bottom" | "left" | "right" | "self";

/** Whether the host frames the view's content. `"card"` (the default when a module
 * declares nothing) = the host centers the content in a safe-area clear of the
 * captions / camera / controls and paints a legible surface behind it. `"none"` =
 * full-bleed: the view fills the stage and owns its own background and layout (e.g.
 * a photo or a dark composition). Declared as `export const surface`. */
export type ViewSurface = "card" | "none";

/** What a view's module declared about itself, known only after import. */
export interface ViewMeta {
  captionAside?: CaptionAside;
  surface?: ViewSurface;
}

interface ViewsValue {
  views: ActiveView[];
  /** Module-declared meta keyed by view id; absent until the module loads. */
  meta: ReadonlyMap<string, ViewMeta>;
  /** Called by the view mount once a module is imported (or re-imported). */
  reportMeta: (id: string, meta: ViewMeta) => void;
  /** Close all views — clears the scene's appearance back to the default empty
   * room. Server-side, so every device + a refresh converge on the cleared
   * screen; the empty state arrives via the same long-poll. */
  clear: () => void;
}

const ViewsContext = createContext<ViewsValue>({
  views: [],
  meta: new Map(),
  reportMeta: () => {},
  clear: () => {},
});

/**
 * Runs the /api/out/view long-poll ABOVE the view slot, so the stream — like the
 * session's channel loops — survives any view swap. Mirrors the server's retained
 * per-scene appearance state: each response is the whole set of active views in
 * z-order, so a fresh page, a second device, or a reconnect all converge on the
 * same screen. A view persists until the agent dismisses or replaces it — there
 * is no client-side expiry; the next snapshot is the only lifecycle driver.
 */
export function ViewsProvider({ children }: { children: ReactNode }) {
  const scene = useScene();
  const { woken } = useWake();
  // Whether the agent's voice is audibly playing right now — the gate signal
  // for a newly appearing view (see VOICE_GATE_FALLBACK_MS). Mirrored into a ref
  // (read by the subscription loop without re-subscribing) plus a waiter set the
  // sync effect flushes the instant the voice starts.
  const { reactive } = usePresence();
  const playingRef = useRef(false);
  const voiceWaitersRef = useRef<Set<() => void>>(new Set());
  const [views, setViews] = useState<Map<string, string>>(new Map());
  const [meta, setMeta] = useState<Map<string, ViewMeta>>(new Map());

  useEffect(() => {
    playingRef.current = reactive;
    if (reactive && voiceWaitersRef.current.size > 0) {
      const waiters = [...voiceWaitersRef.current];
      voiceWaitersRef.current.clear();
      waiters.forEach((wake) => wake());
    }
  }, [reactive]);

  const reportMeta = useCallback((id: string, m: ViewMeta) => {
    setMeta((prev) => new Map(prev).set(id, m));
  }, []);

  const clear = useCallback(() => {
    void clearViewState(scene);
  }, [scene]);

  useEffect(() => {
    if (!woken) return;
    const ctrl = new AbortController();
    let cancelled = false;

    // Resolve when the voice starts playing, or after `ms` (a silent/text turn,
    // or muted output — no voice to wait for), or on teardown. Used to hold a
    // newly appearing view until its narration is actually sounding.
    const waitForVoice = (ms: number) =>
      new Promise<void>((resolve) => {
        if (playingRef.current || cancelled) return resolve();
        let settled = false;
        const finish = () => {
          if (settled) return;
          settled = true;
          clearTimeout(timer);
          voiceWaitersRef.current.delete(finish);
          ctrl.signal.removeEventListener("abort", finish);
          resolve();
        };
        const timer = setTimeout(finish, ms);
        voiceWaitersRef.current.add(finish);
        ctrl.signal.addEventListener("abort", finish, { once: true });
      });

    void (async () => {
      // Ids currently applied to the screen. Only this loop applies snapshots,
      // so a local set is authoritative — and lets us tell a view *appearing*
      // (gate it on the voice) from a removal or a swap of an on-screen view
      // (apply at once). Persists across reconnects within this effect.
      const applied = new Set<string>();
      while (!cancelled) {
        try {
          for await (const state of subscribeViewState({ scene, signal: ctrl.signal })) {
            if (cancelled) break;
            // A snapshot that brings up a view id not on screen yet is held
            // until the voice is audibly playing (or the fallback elapses), so
            // it doesn't pop in a beat ahead of the speech it's paced to.
            // Removals and replaces of already-shown views apply immediately.
            const introducesView = state.views.some((v) => !applied.has(v.id));
            if (introducesView && !playingRef.current) {
              await waitForVoice(VOICE_GATE_FALLBACK_MS);
              if (cancelled) break;
            }
            applied.clear();
            for (const v of state.views) applied.add(v.id);
            // Mirror the snapshot wholesale: array order = z-order. ViewSlot
            // keys by id, so unchanged views keep their mounted component.
            setViews(new Map(state.views.map((v) => [v.id, v.module_url])));
            const live = new Set(state.views.map((v) => v.id));
            setMeta((prev) => {
              if (![...prev.keys()].some((id) => !live.has(id))) return prev;
              return new Map([...prev].filter(([id]) => live.has(id)));
            });
          }
        } catch {
          if (cancelled || ctrl.signal.aborted) break;
          await new Promise((r) => setTimeout(r, 1500));
        }
      }
    })();

    return () => {
      cancelled = true;
      ctrl.abort();
    };
  }, [woken, scene]);

  const value = useMemo<ViewsValue>(
    () => ({ views: [...views].map(([id, moduleUrl]) => ({ id, moduleUrl })), meta, reportMeta, clear }),
    [views, meta, reportMeta, clear],
  );
  return <ViewsContext.Provider value={value}>{children}</ViewsContext.Provider>;
}

/** The currently mounted agent views, in insertion (z-) order. */
export function useViews(): ViewsValue {
  return useContext(ViewsContext);
}
