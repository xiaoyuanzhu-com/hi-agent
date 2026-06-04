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
  const { views } = useViews();

  // The presence recedes while a view is on stage (the agent's content leads).
  const demote = views.length > 0 ? 0.72 : 0;

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

      <div className="hi-stage">
        <SpeechText items={sentences} />
      </div>

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
