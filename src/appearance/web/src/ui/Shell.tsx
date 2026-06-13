import { usePresence, useSpeech, useWake, useChannels, useSendText } from "../core";
import { useViews } from "../core/views";
import { Atmosphere } from "./Atmosphere";
import { Presence } from "./Presence";
import { SpeechText } from "./SpeechText";
import { ViewSlot } from "./ViewSlot";
import { WakeGate } from "./WakeGate";
import { KeyboardFallback } from "./KeyboardFallback";
import { ChannelControls } from "./ChannelControls";

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
  const { views, meta } = useViews();

  // The presence recedes while a view is on stage (the agent's content leads).
  const overlaid = views.length > 0;
  const demote = overlaid ? 0.72 : 0;

  // Caption placement follows the topmost view's module-declared aside (last in
  // z-order); undeclared docks bottom. "self" = the view renders the words
  // itself via useSpeech(), so the host's captions stand down.
  const topmost = overlaid ? views[views.length - 1] : undefined;
  const aside = (topmost && meta.get(topmost.id)?.captionAside) ?? "bottom";
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

      {/* While a view holds the stage, the words dock as captions above it
          (only the freshest lines, so the view stays the lead). */}
      {!selfHosted && (
        <div
          className={overlaid ? "hi-stage hi-stage--captions" : "hi-stage"}
          data-aside={overlaid ? aside : undefined}
        >
          <SpeechText items={overlaid ? sentences.slice(-2) : sentences} />
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
