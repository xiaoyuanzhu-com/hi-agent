import {
  createContext,
  useContext,
  useEffect,
  useMemo,
  useRef,
  useState,
  type ReactNode,
} from "react";
import { subscribeView } from "../channels/out/view";
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
 * session's channel loops — survives any view swap. Holds the active views keyed
 * by id: show/replace set the module under an id (reusing an id is the continuity
 * lever), dismiss removes it, an optional ttl auto-removes it.
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

    const clearTtl = (id: string) => {
      const t = timers.get(id);
      if (t !== undefined) {
        window.clearTimeout(t);
        timers.delete(id);
      }
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
          for await (const env of subscribeView({ scene, signal: ctrl.signal })) {
            if (cancelled) break;
            if (env.op === "dismiss") {
              clearTtl(env.id);
              remove(env.id);
              continue;
            }
            if (!env.module_url) continue;
            const url = env.module_url;
            setViews((prev) => {
              const next = new Map(prev);
              next.set(env.id, url);
              return next;
            });
            clearTtl(env.id);
            if (env.ttl_ms && env.ttl_ms > 0) {
              const timer = window.setTimeout(() => {
                timers.delete(env.id);
                remove(env.id);
              }, env.ttl_ms);
              timers.set(env.id, timer);
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
      timers.forEach((t) => window.clearTimeout(t));
      timers.clear();
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
