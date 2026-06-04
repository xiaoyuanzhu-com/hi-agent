import { createContext, useContext, type ReactNode } from "react";
import { useAgentSession, type AgentSession } from "../hooks/useAgentSession";
import { getScene } from "../lib/scene";

// The session core, exposed to any view (host chrome or agent-authored) through
// one React context. A view never re-owns devices or sockets; it reads the live
// session here. Crucially this provider is mounted ABOVE the swappable view slot,
// so swapping a view never remounts the session — the mic socket and the /audio
// reader outlive any view change.
//
// Stage 0 bridge: the provider still sources its value from `useAgentSession`.
// Stage 7 inlines that hook's body here and the provider becomes the owner.
const SessionContext = createContext<AgentSession | null>(null);

export function SessionProvider({ children }: { children: ReactNode }) {
  const session = useAgentSession();
  return <SessionContext.Provider value={session}>{children}</SessionContext.Provider>;
}

function useSession(): AgentSession {
  const ctx = useContext(SessionContext);
  if (ctx === null) {
    throw new Error("@hi/core hooks must be used within <SessionProvider>");
  }
  return ctx;
}

/** The current exchange's visible lines (user prompt + rolling agent reply). */
export function useSpeech() {
  return useSession().sentences;
}

/** The agent's presence: animation/voice state plus the live audio + cadence. */
export function usePresence() {
  const s = useSession();
  return { state: s.state, reactive: s.reactive, activity: s.activity, bus: s.bus };
}

/** Session entry/wake state for the host gate (not for agent views). */
export function useWake() {
  const s = useSession();
  return {
    woken: s.woken,
    waking: s.waking,
    wakeError: s.wakeError,
    wake: s.wake,
    startTextOnly: s.startTextOnly,
  };
}

/** Every in/out channel's live on/off state and its toggle. */
export function useChannels() {
  const s = useSession();
  return {
    audioInput: s.audioInput,
    audioError: s.audioError,
    videoInput: s.videoInput,
    videoError: s.videoError,
    audioOutput: s.audioOutput,
    textInput: s.textInput,
    toggleAudio: s.toggleAudio,
    toggleVideo: s.toggleVideo,
    toggleAudioOutput: s.toggleAudioOutput,
    setTextChannel: s.setTextChannel,
  };
}

/** Send a typed line on the text input channel. */
export function useSendText() {
  return useSession().sendText;
}

/** The scene this client belongs to (the isolation key). */
export function useScene() {
  return getScene();
}
