import { useEffect } from "react";
import { usePath } from "./router";
import { ScenesView } from "./ScenesView";
import { SessionsView } from "./SessionsView";
import "./inspect.css";

interface Tab {
  key: string;
  label: string;
  path: string;
  render: () => JSX.Element;
}

const TABS: Tab[] = [
  { key: "scenes", label: "Scenes", path: "/inspect/scenes", render: () => <ScenesView /> },
  { key: "sessions", label: "Sessions", path: "/inspect/sessions", render: () => <SessionsView /> },
];
// TABS is a static, non-empty list; the first tab is the default landing.
const FIRST_TAB = TABS[0]!;

/**
 * The inspect console — an operator-facing surface distinct from the agent "face"
 * at `/`. Tabs map to nested routes: `/inspect/scenes` inspects a scene's live
 * channels, `/inspect/sessions` inspects ACP sessions. Bare `/inspect` redirects
 * to the first tab. Each tab owns deeper nested routes (`…/{id}`) for its detail view.
 */
export function Inspect() {
  const { path, navigate } = usePath();

  useEffect(() => {
    if (path === "/inspect" || path === "/inspect/") navigate(FIRST_TAB.path, { replace: true });
  }, [path, navigate]);

  const active = TABS.find((t) => path.startsWith(t.path)) ?? FIRST_TAB;

  return (
    <div className="inspect">
      <header className="inspect-bar">
        <h1>Hi Agent <span className="muted">inspect</span></h1>
        <nav className="tabs">
          {TABS.map((t) => (
            <button
              key={t.key}
              className={t === active ? "tab sel" : "tab"}
              onClick={() => navigate(t.path)}
            >
              {t.label}
            </button>
          ))}
        </nav>
        <a className="exit" href="/" title="back to the agent">✕</a>
      </header>
      <main className="inspect-main">{active.render()}</main>
    </div>
  );
}
