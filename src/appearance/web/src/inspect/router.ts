// A tiny history-API router — enough for the inspect section's nested routes
// (/inspect, /inspect/sessions, …) without pulling in react-router. Components
// read the current path with usePath() and move with its navigate().

import { useCallback, useEffect, useState } from "react";

export interface Router {
  path: string;
  navigate: (to: string, opts?: { replace?: boolean }) => void;
}

export function usePath(): Router {
  const [path, setPath] = useState(() => window.location.pathname);

  useEffect(() => {
    const onPop = () => setPath(window.location.pathname);
    window.addEventListener("popstate", onPop);
    return () => window.removeEventListener("popstate", onPop);
  }, []);

  const navigate = useCallback((to: string, opts?: { replace?: boolean }) => {
    if (to === window.location.pathname) return;
    if (opts?.replace) window.history.replaceState({}, "", to);
    else window.history.pushState({}, "", to);
    setPath(to);
  }, []);

  return { path, navigate };
}

/**
 * The selected id under a tab base, or null. `/inspect/scenes/alice%40phone`
 * with base `/inspect/scenes` → `alice@phone`. Ids are URL-encoded in links
 * (scene ids may contain `@`/`:`), so they are decoded here.
 */
export function selectedUnder(path: string, base: string): string | null {
  const prefix = `${base}/`;
  if (!path.startsWith(prefix)) return null;
  const raw = path.slice(prefix.length).replace(/\/$/, "");
  if (!raw) return null;
  try {
    return decodeURIComponent(raw);
  } catch {
    return raw;
  }
}
