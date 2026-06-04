// Named import+re-export (not `export *`) because react-dom is CommonJS — see
// the note in react.ts. Agent views seldom need react-dom; createPortal/flushSync
// are the useful escape hatches. The instance is shared with the host.
import ReactDOM, { createPortal, flushSync, version } from "react-dom";
export { createPortal, flushSync, version };
export default ReactDOM;
