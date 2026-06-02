// A tiny history-API router — enough for the admin section's nested routes
// (/admin, /admin/acp, …) without pulling in react-router. Components read the
// current path with usePath() and move with its navigate().

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
