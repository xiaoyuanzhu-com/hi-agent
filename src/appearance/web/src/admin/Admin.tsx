import { useEffect } from "react";
import { usePath } from "./router";
import { ScenesView } from "./ScenesView";
import { AcpView } from "./AcpView";
import "./admin.css";

interface Tab {
  key: string;
  label: string;
  path: string;
  render: () => JSX.Element;
}

const TABS: Tab[] = [
  { key: "scenes", label: "Scenes", path: "/admin/scenes", render: () => <ScenesView /> },
  { key: "acp", label: "ACP", path: "/admin/acp", render: () => <AcpView /> },
];
// TABS is a static, non-empty list; the first tab is the default landing.
const FIRST_TAB = TABS[0]!;

/**
 * The admin console — an operator-facing surface distinct from the agent "face"
 * at `/`. Tabs map to nested routes: `/admin/scenes` inspects a scene's live
 * channels, `/admin/acp` inspects ACP sessions. Bare `/admin` redirects to the
 * first tab. Each tab owns deeper nested routes (`…/{id}`) for its detail view.
 */
export function Admin() {
  const { path, navigate } = usePath();

  useEffect(() => {
    if (path === "/admin" || path === "/admin/") navigate(FIRST_TAB.path, { replace: true });
  }, [path, navigate]);

  const active = TABS.find((t) => path.startsWith(t.path)) ?? FIRST_TAB;

  return (
    <div className="admin">
      <header className="admin-bar">
        <h1>hi-agent <span className="muted">admin</span></h1>
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
      <main className="admin-main">{active.render()}</main>
    </div>
  );
}
