import React from "react";
import { createRoot } from "react-dom/client";
import { App } from "./App";
import { Inspect } from "./inspect/Inspect";
import { usePath } from "./inspect/router";
import "./ui/global.css";

const rootEl = document.getElementById("root");
if (!rootEl) {
  throw new Error("missing #root mount point");
}

// One SPA, two surfaces: the agent "face" at `/`, and the operator console
// under `/inspect/*`. A tiny path check picks between them; the inspect section
// owns its own nested routing.
function Root() {
  const { path } = usePath();
  return path.startsWith("/inspect") ? <Inspect /> : <App />;
}

createRoot(rootEl).render(
  <React.StrictMode>
    <Root />
  </React.StrictMode>,
);
