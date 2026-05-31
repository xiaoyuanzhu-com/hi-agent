import { useAgentSession } from "./hooks/useAgentSession";
import { Atmosphere } from "./ui/Atmosphere";
import { Presence } from "./ui/Presence";
import { SpeechText } from "./ui/SpeechText";
import { SurfaceHost } from "./ui/SurfaceHost";
import { HistoryDrawer } from "./ui/HistoryDrawer";
import { WakeGate } from "./ui/WakeGate";
import { KeyboardFallback } from "./ui/KeyboardFallback";

/**
 * The whole surface: a calm, breathing room.
 *
 *   Atmosphere (background) · Presence (dot-matrix, the agent) · SpeechText
 *   (the agent's words, fading in as whole sentences) · SurfaceHost (rich
 *   agent-authored content as a card/full overlay) · HistoryDrawer (recall).
 *
 * Before wake, a single tap-to-begin gate; after wake, a hidden keyboard
 * fallback. No input box or buttons by default — it listens, speaks, and shows.
 */
export function App() {
  const s = useAgentSession();

  return (
    <div className="hi-root">
      <Atmosphere />
      <Presence bus={s.bus} state={s.state} reactive={s.reactive} activity={s.activity} demote={s.demote} />

      <div className="hi-stage">
        <SpeechText items={s.sentences} />
      </div>

      <SurfaceHost surface={s.activeSurface} onDismiss={s.dismissSurface} />

      {s.woken && s.surfaceHistory.length > 0 && (
        <HistoryDrawer surfaces={s.surfaceHistory} onOpen={s.openSurface} />
      )}

      {!s.woken ? (
        <WakeGate onWake={s.wake} error={s.wakeError} busy={s.waking} />
      ) : (
        <KeyboardFallback onSend={s.sendText} />
      )}
    </div>
  );
}
