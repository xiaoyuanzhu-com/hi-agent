// `@hi/core` — the session surface a view authors against. Host chrome and
// agent-authored views both import these hooks; the import map (Stage 1-2)
// guarantees every importer shares the one provider instance.
export {
  SessionProvider,
  useSpeech,
  usePresence,
  useWake,
  useChannels,
  useSendText,
  useScene,
} from "./session";
export { ViewsProvider, useViews, type ActiveView } from "./views";

