// Motion (formerly Framer Motion). Only agent views use it; the import map
// serves it at one URL so every view shares the instance — and it resolves its
// own `react` import through the shared react shim, not a second copy.
export * from "motion/react";
