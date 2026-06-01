import { useAgentSession } from "./hooks/useAgentSession";
import { Atmosphere } from "./ui/Atmosphere";
import { Presence } from "./ui/Presence";
import { SpeechText } from "./ui/SpeechText";
import { SurfaceHost } from "./ui/SurfaceHost";
import { HistoryDrawer } from "./ui/HistoryDrawer";
import { WakeGate } from "./ui/WakeGate";
import { KeyboardFallback } from "./ui/KeyboardFallback";
import { ChannelControls } from "./ui/ChannelControls";

/**
 * The whole surface: a calm, breathing room.
 *
 *   Atmosphere (background) · Presence (dot-matrix, the agent) · SpeechText
 *   (the agent's words, fading in as whole sentences) · SurfaceHost (rich
 *   agent-authored content as a card/full overlay) · HistoryDrawer (recall).
 *
 * Before entering, a single tap-to-begin gate (with a "type instead" path for
 * when audio can't be used). After entering, the input channels — mic and text —
 * are independent and each toggleable via the corner controls.
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
        <WakeGate onWake={s.wake} onTextOnly={s.startTextOnly} error={s.wakeError} busy={s.waking} />
      ) : (
        <>
          <ChannelControls
            audioOn={s.audioInput}
            onToggleAudio={s.toggleAudio}
            audioError={s.audioError}
            videoOn={s.videoInput}
            onToggleVideo={s.toggleVideo}
            videoError={s.videoError}
            textOn={s.textInput}
            onToggleText={() => s.setTextChannel(!s.textInput)}
            voiceOn={s.audioOutput}
            onToggleVoice={s.toggleAudioOutput}
          />
          <KeyboardFallback
            onSend={s.sendText}
            open={s.textInput}
            onOpen={() => s.setTextChannel(true)}
            onClose={() => s.setTextChannel(false)}
          />
        </>
      )}
    </div>
  );
}
