import React from "react";
import { createRoot } from "react-dom/client";
import { App } from "./App";
import { Admin } from "./admin/Admin";
import { usePath } from "./admin/router";
import "./ui/global.css";

const rootEl = document.getElementById("root");
if (!rootEl) {
  throw new Error("missing #root mount point");
}

// One SPA, two surfaces: the agent "face" at `/`, and the operator console
// under `/admin/*`. A tiny path check picks between them; the admin section
// owns its own nested routing.
function Root() {
  const { path } = usePath();
  return path.startsWith("/admin") ? <Admin /> : <App />;
}

createRoot(rootEl).render(
  <React.StrictMode>
    <Root />
  </React.StrictMode>,
);
