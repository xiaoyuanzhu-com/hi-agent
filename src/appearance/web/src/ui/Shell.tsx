import { usePresence, useSpeech, useChannels, useSendText } from "../core";
import { useViews } from "../core/views";
import { floorLayout, CAPTIONS_ID, CAMERA_ID, type Participant } from "../core/layout";
import { Atmosphere } from "./Atmosphere";
import { Presence } from "./Presence";
import { SpeechText } from "./SpeechText";
import { ViewSlot } from "./ViewSlot";
import { KeyboardFallback } from "./KeyboardFallback";
import { ChannelControls } from "./ChannelControls";
import { OutOfEnergyHint } from "./OutOfEnergyHint";
import { CameraPreview } from "./CameraPreview";

/**
 * The host chrome — a calm, breathing room — reading the session through
 * `@hi/core` hooks rather than owning it. The session lives in the providers
 * above this component, so the swappable `ViewSlot` below never tears down the
 * mic / audio / channel loops when the agent swaps a view.
 *
 *   Atmosphere · Presence (the agent) · SpeechText (its words) · ViewSlot
 *   (agent-authored views) · the channel controls / input line.
 *
 * Placement is one job: every participant — the agent views, the live captions,
 * and the camera self-view — is laid out by a single `floorLayout` pass. But that
 * unifies *placement*, never *lifecycle*: the captions `<div>` and `<CameraPreview>`
 * are mounted ONCE here, above the swappable `ViewSlot`, and the layout only flips
 * their props/classes. They must never move into `ViewSlot` or a participant
 * `.map()` — re-mounting `<CameraPreview>` re-acquires the camera and blacks out
 * the feed.
 */
export function Shell() {
  const presence = usePresence();
  const sentences = useSpeech();
  const ch = useChannels();
  const sendText = useSendText();
  const { views, meta, clear } = useViews();

  // Everything on screen is a participant. Views carry their declared geometry
  // (wire-authoritative; a module-self-declared fallback fills in for inline
  // `source` views with no wire geometry). The captions are always a participant;
  // the camera joins only while its stream is live.
  const participants: Participant[] = [
    ...views.map((v) => ({
      id: v.id,
      kind: "view" as const,
      geometry: v.geometry ?? meta.get(v.id)?.geometry,
    })),
    { id: CAPTIONS_ID, kind: "captions" as const },
    ...(ch.visionStream ? [{ id: CAMERA_ID, kind: "camera" as const }] : []),
  ];
  const { demote, placements } = floorLayout(participants);

  const captions = placements.get(CAPTIONS_ID);
  const camera = placements.get(CAMERA_ID);
  const captionsDocked = captions?.docked ?? false;

  return (
    <div className="hi-root">
      <Atmosphere />
      <Presence state={presence.state} demote={demote} />

      {/* PINNED participant — mounted once, here, across every layout. The layout
          only flips `pip` (fullscreen backdrop ↔ corner thumbnail); the same
          <video> stays mounted so the feed never re-attaches and blacks out. */}
      <CameraPreview stream={ch.visionStream} pip={camera?.pip ?? false} />

      {/* PINNED participant — the conversation's words. Docks as caption pills
          when something fills the stage behind them (a view or the camera), else
          sits centered as the lead. Hidden only when the topmost view renders the
          words itself. Stays at this mount site across every layout. */}
      {captions && !captions.hidden && (
        <div
          className={captionsDocked ? "hi-stage hi-stage--captions" : "hi-stage"}
          data-region={captionsDocked ? captions.region : undefined}
        >
          <SpeechText items={captionsDocked ? sentences.slice(-3) : sentences} />
        </div>
      )}

      <ViewSlot placements={placements} />

      {/* A quiet card just above the controls while the account is out of energy —
          keep-typing reassurance + a signed-in 升级 link. Self-polling; renders
          nothing when energy is flowing. */}
      <OutOfEnergyHint />

      {/* Channel controls are always present — the session auto-starts, so there
          is no gate. Each control honestly reflects its channel's live state;
          one that couldn't be restored shows off, and a click enables it. */}
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
    </div>
  );
}
