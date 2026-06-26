import React from "react";
import { createRoot } from "react-dom/client";
import { App } from "./App";
import { Inspect } from "./inspect/Inspect";
import { Settings } from "./settings/Settings";
import { usePath } from "./inspect/router";
import { installAuthGate } from "./lib/authGate";
import "./ui/global.css";

// If the login gate is on, a 401 (session expired) bounces the tab to sign-in.
// No-op when auth is disabled.
installAuthGate();

const rootEl = document.getElementById("root");
if (!rootEl) {
  throw new Error("missing #root mount point");
}

// One SPA, three surfaces: the agent "face" at `/`, the owner Settings page at
// `/settings`, and the operator console under `/inspect/*`. A tiny path check
// picks between them; the inspect section owns its own nested routing.
function Root() {
  const { path } = usePath();
  if (path.startsWith("/inspect")) return <Inspect />;
  if (path.startsWith("/settings")) return <Settings />;
  return <App />;
}

createRoot(rootEl).render(
  <React.StrictMode>
    <Root />
  </React.StrictMode>,
);
