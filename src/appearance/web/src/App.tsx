import { useAgentSession } from "./hooks/useAgentSession";
import { Atmosphere } from "./ui/Atmosphere";
import { Presence } from "./ui/Presence";
import { SpeechText } from "./ui/SpeechText";
import { WakeGate } from "./ui/WakeGate";
import { KeyboardFallback } from "./ui/KeyboardFallback";

/**
 * The whole surface: a calm, breathing room.
 *
 *   Atmosphere (background) · Presence (dot-matrix, the agent) · SpeechText
 *   (the agent's words, fading in as whole sentences). Before wake, a single
 *   tap-to-begin gate. After wake, a hidden keyboard fallback.
 *
 * No input box, no buttons — the session listens and (Phase 2) speaks on its
 * own. Content overlays arrive in Phase 3.
 */
export function App() {
  const s = useAgentSession();

  return (
    <div className="hi-root">
      <Atmosphere />
      <Presence bus={s.bus} state={s.state} reactive={s.reactive} />

      <div className="hi-stage">
        <SpeechText items={s.sentences} />
      </div>

      {!s.woken ? (
        <WakeGate onWake={s.wake} error={s.wakeError} busy={s.waking} />
      ) : (
        <KeyboardFallback onSend={s.sendText} />
      )}
    </div>
  );
}
