interface ChannelControlsProps {
  /** Whether the mic (audio input) channel is live. */
  audioOn: boolean;
  /** Flip the audio channel on/off. */
  onToggleAudio: () => void;
  /** Surfaced if the last attempt to turn audio on failed. */
  audioError?: string | null;
  /** Whether the camera (vision input) channel is live. */
  videoOn: boolean;
  /** Flip the vision channel on/off. */
  onToggleVideo: () => void;
  /** Surfaced if the last attempt to turn vision on failed. */
  videoError?: string | null;
  /** Whether the text input channel is on. */
  textOn: boolean;
  /** Flip the text input channel on/off. */
  onToggleText: () => void;
  /** Whether the agent's voice (audio output) is on. */
  voiceOn: boolean;
  /** Mute/unmute the agent's voice. */
  onToggleVoice: () => void;
}

/**
 * The channel controls — a quiet cluster in the corner. The input channels (mic,
 * camera, text) and the output channel (voice) are all independent: each can be
 * on or off at any time, and they don't conflict. Kept deliberately minimal to
 * preserve the calm room (no chrome by default), but always present so a user
 * who can't (or won't) use a given channel still has a clear way in or out.
 */
export function ChannelControls({
  audioOn,
  onToggleAudio,
  audioError,
  videoOn,
  onToggleVideo,
  videoError,
  textOn,
  onToggleText,
  voiceOn,
  onToggleVoice,
}: ChannelControlsProps) {
  return (
    <div className="hi-channels" role="group" aria-label="input channels">
      <button
        type="button"
        className={`hi-channel${audioOn ? " is-on" : ""}`}
        onClick={onToggleAudio}
        title={audioError ?? (audioOn ? "mic on — tap to mute" : "mic off — tap to listen")}
        aria-pressed={audioOn}
        aria-label={audioOn ? "turn microphone off" : "turn microphone on"}
      >
        <MicGlyph muted={!audioOn} />
      </button>

      <button
        type="button"
        className={`hi-channel${videoOn ? " is-on" : ""}`}
        onClick={onToggleVideo}
        title={videoError ?? (videoOn ? "camera on — tap to turn off" : "camera off — tap to turn on")}
        aria-pressed={videoOn}
        aria-label={videoOn ? "turn camera off" : "turn camera on"}
      >
        <CamGlyph off={!videoOn} />
      </button>

      <button
        type="button"
        className={`hi-channel${textOn ? " is-on" : ""}`}
        onClick={onToggleText}
        title={textOn ? "text on — tap to hide" : "text off — tap to type"}
        aria-pressed={textOn}
        aria-label={textOn ? "hide the text input" : "show the text input"}
      >
        <KeyboardGlyph />
      </button>

      <span className="hi-channel-sep" aria-hidden="true" />

      <button
        type="button"
        className={`hi-channel${voiceOn ? " is-on" : ""}`}
        onClick={onToggleVoice}
        title={voiceOn ? "voice on — tap to mute" : "voice muted — tap to unmute"}
        aria-pressed={voiceOn}
        aria-label={voiceOn ? "mute the agent's voice" : "unmute the agent's voice"}
      >
        <SpeakerGlyph muted={!voiceOn} />
      </button>
    </div>
  );
}

function MicGlyph({ muted }: { muted: boolean }) {
  return (
    <svg viewBox="0 0 24 24" width="18" height="18" fill="none" aria-hidden="true">
      <rect x="9" y="3" width="6" height="11" rx="3" stroke="currentColor" strokeWidth="1.6" />
      <path
        d="M6 11a6 6 0 0 0 12 0M12 17v3"
        stroke="currentColor"
        strokeWidth="1.6"
        strokeLinecap="round"
      />
      {muted && (
        <line x1="4" y1="4" x2="20" y2="20" stroke="currentColor" strokeWidth="1.6" strokeLinecap="round" />
      )}
    </svg>
  );
}

function CamGlyph({ off }: { off: boolean }) {
  return (
    <svg viewBox="0 0 24 24" width="18" height="18" fill="none" aria-hidden="true">
      <rect x="3" y="6" width="13" height="12" rx="2.5" stroke="currentColor" strokeWidth="1.6" />
      <path d="M16 10l5-3v10l-5-3" stroke="currentColor" strokeWidth="1.6" strokeLinejoin="round" />
      {off && (
        <line x1="4" y1="4" x2="20" y2="20" stroke="currentColor" strokeWidth="1.6" strokeLinecap="round" />
      )}
    </svg>
  );
}

function SpeakerGlyph({ muted }: { muted: boolean }) {
  return (
    <svg viewBox="0 0 24 24" width="18" height="18" fill="none" aria-hidden="true">
      <path
        d="M4 9v6h3l5 4V5L7 9H4z"
        stroke="currentColor"
        strokeWidth="1.6"
        strokeLinejoin="round"
      />
      {muted ? (
        <path d="M16 9l5 6M21 9l-5 6" stroke="currentColor" strokeWidth="1.6" strokeLinecap="round" />
      ) : (
        <path
          d="M16 9a4 4 0 0 1 0 6M18.5 6.5a7.5 7.5 0 0 1 0 11"
          stroke="currentColor"
          strokeWidth="1.6"
          strokeLinecap="round"
        />
      )}
    </svg>
  );
}

function KeyboardGlyph() {
  return (
    <svg viewBox="0 0 24 24" width="18" height="18" fill="none" aria-hidden="true">
      <rect x="3" y="6" width="18" height="12" rx="2" stroke="currentColor" strokeWidth="1.6" />
      <path
        d="M7 10h.01M11 10h.01M15 10h.01M8 14h8"
        stroke="currentColor"
        strokeWidth="1.6"
        strokeLinecap="round"
      />
    </svg>
  );
}
