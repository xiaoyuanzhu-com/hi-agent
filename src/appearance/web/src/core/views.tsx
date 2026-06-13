import {
  createContext,
  useContext,
  useEffect,
  useMemo,
  useRef,
  useState,
  type ReactNode,
} from "react";
import { subscribeViewState } from "../channels/out/view";
import { useScene, useWake } from "./session";

/** One mounted agent view: a stable id and the compiled module URL to import. */
export interface ActiveView {
  id: string;
  moduleUrl: string;
}

interface ViewsValue {
  views: ActiveView[];
}

const ViewsContext = createContext<ViewsValue>({ views: [] });

/**
 * Runs the /api/out/view long-poll ABOVE the view slot, so the stream — like the
 * session's channel loops — survives any view swap. Mirrors the server's retained
 * per-scene appearance state: each response is the whole set of active views in
 * z-order, so a fresh page, a second device, or a reconnect all converge on the
 * same screen. TTL timers handle live expiry between snapshots; the server evicts
 * authoritatively and the next snapshot agrees.
 */
export function ViewsProvider({ children }: { children: ReactNode }) {
  const scene = useScene();
  const { woken } = useWake();
  const [views, setViews] = useState<Map<string, string>>(new Map());
  const ttlTimers = useRef<Map<string, number>>(new Map());

  useEffect(() => {
    if (!woken) return;
    const ctrl = new AbortController();
    let cancelled = false;
    const timers = ttlTimers.current;

    const clearTimers = () => {
      timers.forEach((t) => window.clearTimeout(t));
      timers.clear();
    };
    const remove = (id: string) =>
      setViews((prev) => {
        if (!prev.has(id)) return prev;
        const next = new Map(prev);
        next.delete(id);
        return next;
      });

    void (async () => {
      while (!cancelled) {
        try {
          for await (const state of subscribeViewState({ scene, signal: ctrl.signal })) {
            if (cancelled) break;
            // Mirror the snapshot wholesale: array order = z-order. ViewSlot
            // keys by id, so unchanged views keep their mounted component.
            setViews(new Map(state.views.map((v) => [v.id, v.module_url])));
            clearTimers();
            for (const v of state.views) {
              if (v.ttl_ms && v.ttl_ms > 0) {
                const timer = window.setTimeout(() => {
                  timers.delete(v.id);
                  remove(v.id);
                }, v.ttl_ms);
                timers.set(v.id, timer);
              }
            }
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
      clearTimers();
    };
  }, [woken, scene]);

  const value = useMemo<ViewsValue>(
    () => ({ views: [...views].map(([id, moduleUrl]) => ({ id, moduleUrl })) }),
    [views],
  );
  return <ViewsContext.Provider value={value}>{children}</ViewsContext.Provider>;
}

/** The currently mounted agent views, in insertion (z-) order. */
export function useViews(): ViewsValue {
  return useContext(ViewsContext);
}
