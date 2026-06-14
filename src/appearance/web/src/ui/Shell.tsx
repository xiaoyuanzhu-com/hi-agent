import { usePresence, useSpeech, useWake, useChannels, useSendText } from "../core";
import { useViews } from "../core/views";
import { Atmosphere } from "./Atmosphere";
import { Presence } from "./Presence";
import { SpeechText } from "./SpeechText";
import { ViewSlot } from "./ViewSlot";
import { WakeGate } from "./WakeGate";
import { KeyboardFallback } from "./KeyboardFallback";
import { ChannelControls } from "./ChannelControls";
import { CameraPreview } from "./CameraPreview";

/**
 * The host chrome — a calm, breathing room — reading the session through
 * `@hi/core` hooks rather than owning it. The session lives in the providers
 * above this component, so the swappable `ViewSlot` below never tears down the
 * mic / audio / channel loops when the agent swaps a view.
 *
 *   Atmosphere · Presence (the agent) · SpeechText (its words) · ViewSlot
 *   (agent-authored views) · the wake gate / channel controls / input line.
 */
export function Shell() {
  const presence = usePresence();
  const sentences = useSpeech();
  const { woken, waking, wakeError, wake, startTextOnly } = useWake();
  const ch = useChannels();
  const sendText = useSendText();
  const { views, meta, clear } = useViews();

  // The presence recedes while a view is on stage (the agent's content leads).
  const overlaid = views.length > 0;
  const demote = overlaid ? 0.72 : 0;

  // The live camera fills the stage as a fullscreen backdrop only when no view
  // leads (otherwise it shrinks to a pip).
  const cameraBackdrop = !!ch.visionStream && !overlaid;

  // The words dock as a caption pill whenever something fills the stage behind
  // them — an agent view or the live camera — so they don't sit on top of it.
  // Over a view the side follows the module's declared aside (undeclared docks
  // bottom); over the camera they tuck into the lower-left, clear of a centred
  // face. "self" = the view renders the words itself via useSpeech(), so the
  // host's captions stand down.
  const docked = overlaid || cameraBackdrop;
  const topmost = overlaid ? views[views.length - 1] : undefined;
  const aside = overlaid
    ? (meta.get(topmost!.id)?.captionAside ?? "bottom")
    : "left";
  const selfHosted = overlaid && aside === "self";

  return (
    <div className="hi-root">
      <Atmosphere />
      <Presence
        bus={presence.bus}
        state={presence.state}
        reactive={presence.reactive}
        activity={presence.activity}
        demote={demote}
      />

      {/* The user's self-view while the camera is on — a fullscreen backdrop
          when nothing leads, shrinking to a corner thumbnail once a view does. */}
      <CameraPreview stream={ch.visionStream} pip={overlaid} />

      {/* While a view holds the stage, the words dock as captions above it
          (only the freshest lines, so the view stays the lead). */}
      {!selfHosted && (
        <div
          className={docked ? "hi-stage hi-stage--captions" : "hi-stage"}
          data-aside={docked ? aside : undefined}
        >
          <SpeechText items={docked ? sentences.slice(-3) : sentences} />
        </div>
      )}

      <ViewSlot />

      {!woken ? (
        <WakeGate onWake={wake} onTextOnly={startTextOnly} error={wakeError} busy={waking} />
      ) : (
        <>
          <ChannelControls
            audioOn={ch.audioInput}
            onToggleAudio={ch.toggleAudio}
            audioError={ch.audioError}
            videoOn={ch.videoInput}
            onToggleVideo={ch.toggleVideo}
            videoError={ch.videoError}
            textOn={ch.textInput}
            onToggleText={() => ch.setTextChannel(!ch.textInput)}
            voiceOn={ch.audioOutput}
            onToggleVoice={ch.toggleAudioOutput}
            onCloseViews={clear}
          />
          <KeyboardFallback
            onSend={sendText}
            open={ch.textInput}
            onOpen={() => ch.setTextChannel(true)}
            onClose={() => ch.setTextChannel(false)}
          />
        </>
      )}
    </div>
  );
}
