// The automatic JSX runtime. esbuild compiles agent JSX to imports of `jsx`/
// `jsxs`/`Fragment` from here, so they must resolve to the host's React. Named
// re-export (not `export *`) because jsx-runtime is CommonJS — see react.ts.
import { Fragment, jsx, jsxs } from "react/jsx-runtime";
export { Fragment, jsx, jsxs };
