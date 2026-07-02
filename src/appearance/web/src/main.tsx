import React from "react";
import { createRoot } from "react-dom/client";
import { App } from "./App";
import { Inspect } from "./inspect/Inspect";
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

// One SPA, two surfaces: the agent "face" at `/` and the operator console under
// `/inspect/*`. A tiny path check picks between them; the inspect section owns its
// own nested routing. (AI-credential config lives in the native tray, not the web.)
function Root() {
  const { path } = usePath();
  if (path.startsWith("/inspect")) return <Inspect />;
  return <App />;
}

createRoot(rootEl).render(
  <React.StrictMode>
    <Root />
  </React.StrictMode>,
);
