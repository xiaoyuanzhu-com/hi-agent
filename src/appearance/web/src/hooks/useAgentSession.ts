import { useCallback, useEffect, useMemo, useRef, useState } from "react";
import { subscribeOutText } from "../channels/out/text";
import { subscribeAudioTurns } from "../channels/out/audio";
import { postInText, subscribeInText } from "../channels/in/text";
import { AudioBus } from "../lib/audioBus";
import { ActivityMeter } from "../lib/activityMeter";
import { AudioStreamer } from "../lib/audioStreamer";
import { VideoStreamer } from "../lib/videoStreamer";
import { PresenceStiller } from "../lib/presenceStiller";
import { VoicePlayer } from "../lib/voicePlayer";
import { SentenceBuffer } from "../lib/sentences";
import { getScene } from "../lib/scene";
import type { PresenceState } from "../ui/Presence";
import type { SpeechItem } from "../ui/SpeechText";

// How many of the agent's reply lines stay on screen at once. The reply rolls
// as a calm caption (newest last), but it's windowed *on top of* the pinned
// user line — never instead of it (see `visibleExchange`), so an answer of any
// length can't scroll the prompt that prompted it off-screen. Three lines (not
// two) give a viewer time to read each line before it rolls out of view.
const AGENT_REPLY_WINDOW = 3;

// Stable id for the single rolling-interim line (the user's speech as it's
// being recognized). One slot per scene by design: partials are cumulative, so
// each replaces the last, and keying by a constant id lets React patch the
// same <p> instead of remounting per partial.
const INTERIM_ID = -1;

// A rolling interim with no follow-up (STT stream died mid-utterance, no final
// ever lands) is cleared after this long so a ghost italic line can't linger.
const INTERIM_STALE_MS = 3000;

// Agent reply sentences are revealed paced to roughly speaking rate, not dumped
// the instant they arrive, so the transcript tracks the voice instead of racing
// ahead of it. Estimate-grade on purpose (we don't read real playback position);
// a barge-in clears whatever is still queued. See `pumpAgent` / `duck`. The rate
// sits just under the backend's ~200ms/char Mandarin speech model (interrupts.rs)
// so text leads the voice by a hair rather than lagging it; tune live by ear.
const REVEAL_MS_PER_CHAR = 170;
const MIN_REVEAL_MS = 450;

// Index of the last user line in a timeline, or -1 if the agent has spoken but
// the user hasn't yet (e.g. an opening greeting). The user line anchors the
// "current exchange": everything after it is the agent's reply to it.
function lastUserIndex(items: SpeechItem[]): number {
  for (let i = items.length - 1; i >= 0; i--) {
    if (items[i]?.speaker === "user") return i;
  }
  return -1;
}

// State bound: drop turns before the user's current one. Earlier exchanges are
// never shown, so they aren't retained — but the current user line and the full
// reply accumulating after it are always kept. A new user line (the next turn)
// is what clears the previous exchange.
function dropPriorTurns(items: SpeechItem[]): SpeechItem[] {
  const u = lastUserIndex(items);
  return u <= 0 ? items : items.slice(u);
}

// Display window: the user's latest line pinned, followed by the most recent
// `AGENT_REPLY_WINDOW` lines of the reply to it. With no user line yet, just the
// rolling reply caption.
function visibleExchange(items: SpeechItem[]): SpeechItem[] {
  const u = lastUserIndex(items);
  if (u === -1) return items.slice(-AGENT_REPLY_WINDOW);
  return [...items.slice(u, u + 1), ...items.slice(u + 1).slice(-AGENT_REPLY_WINDOW)];
}

// ---- Channel preferences (persisted client-side) -------------------------
// The user's chosen on/off state for each channel, remembered across visits in
// localStorage. These are *intents*: a saved "audio on" is reapplied on the
// next visit (the mic is re-acquired after the wake gesture), and survives a
// failed acquisition so it retries rather than silently sticking off.
interface ChannelPrefs {
  audioInput: boolean;
  videoInput: boolean;
  textInput: boolean;
  audioOutput: boolean;
}

const PREFS_KEY = "hi.channels.v1";
const DEFAULT_PREFS: ChannelPrefs = {
  audioInput: true,
  videoInput: false,
  textInput: false,
  audioOutput: true,
};

// True only when the browser can confirm a device permission is already granted,
// so a saved-on channel can be restored *silently* — no prompt, no gesture. A
// "prompt"/"denied" state, or a browser that can't answer the query (older
// Safari / Firefox), both read as "can't restore silently": the channel stays
// off and a click re-requests it.
async function permissionGranted(name: "microphone" | "camera"): Promise<boolean> {
  const perms = navigator.permissions;
  if (!perms?.query) return false;
  try {
    const status = await perms.query({ name: name as PermissionName });
    return status.state === "granted";
  } catch {
    return false;
  }
}

function loadPrefs(): ChannelPrefs {
  try {
    const raw = localStorage.getItem(PREFS_KEY);
    if (!raw) return { ...DEFAULT_PREFS };
    const p = JSON.parse(raw) as Partial<ChannelPrefs>;
    return {
      audioInput: typeof p.audioInput === "boolean" ? p.audioInput : DEFAULT_PREFS.audioInput,
      videoInput: typeof p.videoInput === "boolean" ? p.videoInput : DEFAULT_PREFS.videoInput,
      textInput: typeof p.textInput === "boolean" ? p.textInput : DEFAULT_PREFS.textInput,
      audioOutput: typeof p.audioOutput === "boolean" ? p.audioOutput : DEFAULT_PREFS.audioOutput,
    };
  } catch {
    return { ...DEFAULT_PREFS };
  }
}

export interface AgentSession {
  state: PresenceState;
  reactive: boolean;
  bus: AudioBus | null;
  /** Live cognition cadence (streamed-chunk pulses) the field reacts to. */
  activity: ActivityMeter;
  sentences: SpeechItem[];
  /** Whether the session's output graph is up (auto-started on mount). */
  woken: boolean;
  /** Whether the mic (audio input) channel is currently live. */
  audioInput: boolean;
  /** Surfaced if turning the audio channel on failed (denied / no device). */
  audioError: string | null;
  /** Whether the camera (vision input) channel is currently live. */
  videoInput: boolean;
  /** Surfaced if turning the vision channel on failed (denied / no device). */
  videoError: string | null;
  /** The live camera stream while vision is on (for a self-view), else null. */
  visionStream: MediaStream | null;
  /** Whether the agent's voice (audio output) channel is on. */
  audioOutput: boolean;
  /** Whether the text input channel is on (the input line is shown). */
  textInput: boolean;
  /** Flip the audio-input channel on/off independently of the others. */
  toggleAudio: () => void;
  /** Flip the vision-input channel on/off independently of the others. */
  toggleVideo: () => void;
  /** Flip the agent's voice (audio output) on/off; text output is unaffected. */
  toggleAudioOutput: () => void;
  /** Turn the text input channel on/off (shows/hides the input line). */
  setTextChannel: (on: boolean) => void;
  sendText: (text: string) => void;
}

/**
 * The coordinator — deliberately a *dumb face*. After the wake gesture it owns
 * the input channels (mic → /api/in/audio/stream, continuous PCM; camera →
 * /api/in/vision, a frame every couple seconds) and subscribes to every channel
 * on both boundaries, rendering whatever arrives: /api/out/audio plays on
 * arrival, /api/out/text chunks fade in as whole sentences. The user's words —
 * whether typed or recognized from speech — arrive as settled text lines on
 * /api/in/text (the server transcribes the mic and posts the transcript there),
 * so every client in the scene shows identical UI whether or not it holds the
 * mic. "Audio is audio": the raw mic bytes ride /api/in/audio for anyone who
 * wants to listen, but the conversation the face renders is text.
 *
 * Crucially it does NOT decide turns. Turn-taking — when the agent speaks, which
 * drafts to suppress — lives in the mind (the reactor), which commits after the
 * inbound signal stream goes quiet.
 */
export function useAgentSession(): AgentSession {
  const scene = useMemo(() => getScene(), []);

  // Saved channel intents. Held in a ref (read synchronously by startSession /
  // the toggles) and written through on every explicit user change.
  const prefsRef = useRef<ChannelPrefs>(loadPrefs());
  const persistPrefs = useCallback(() => {
    try {
      localStorage.setItem(PREFS_KEY, JSON.stringify(prefsRef.current));
    } catch {
      /* storage unavailable (private mode / quota) — prefs just won't persist */
    }
  }, []);

  const [woken, setWoken] = useState(false);
  const [bus, setBus] = useState<AudioBus | null>(null);
  const [sentences, setSentences] = useState<SpeechItem[]>([]);

  const [audioInput, setAudioInput] = useState(false);
  const [audioError, setAudioError] = useState<string | null>(null);
  const [videoInput, setVideoInput] = useState(false);
  const [videoError, setVideoError] = useState<string | null>(null);
  // The live camera stream while vision is on, so the UI can render a self-view
  // (the host shows it; null when the camera is off). Held in state alongside
  // the upload-only `visionRef` so a render is triggered when it appears/clears.
  const [visionStream, setVisionStream] = useState<MediaStream | null>(null);
  const [audioOutput, setAudioOutput] = useState(prefsRef.current.audioOutput);
  const [textInput, setTextInput] = useState(prefsRef.current.textInput);
  const [agentStreaming, setAgentStreaming] = useState(false);
  const [awaiting, setAwaiting] = useState(false);
  const [ttsPlaying, setTtsPlaying] = useState(false);
  const [offline, setOffline] = useState(false);
  // The user's speech as it's being recognized (cumulative rolling text), or
  // null when no utterance is in flight. Server-broadcast on /in/text, so every
  // client in the scene shows the same live line.
  const [interim, setInterim] = useState<string | null>(null);

  const busRef = useRef<AudioBus | null>(null);
  const micRef = useRef<AudioStreamer | null>(null);
  const micStreamRef = useRef<MediaStream | null>(null);
  // Reentrancy guard for enableAudio: set synchronously before its first await,
  // so two near-simultaneous calls (e.g. StrictMode's double-invoked effect)
  // can't both open a /api/in/audio/stream socket — a second socket would
  // transcribe + dispatch every utterance a second time, duplicating it.
  const micStartingRef = useRef(false);
  // Bumped by disableAudio/unmount to cancel an in-flight enableAudio: a start
  // that finishes acquiring devices after a teardown tears its own socket down
  // instead of leaking it.
  const micGenRef = useRef(0);
  const voiceRef = useRef<VoicePlayer | null>(null);
  const visionRef = useRef<VideoStreamer | null>(null);
  const presenceRef = useRef<PresenceStiller | null>(null);
  const visionStreamRef = useRef<MediaStream | null>(null);
  const sentenceIdRef = useRef(0);
  // Agent reply sentences awaiting reveal, and the timer pacing them onto screen
  // (see `pumpAgent`). A barge-in empties the queue so unheard lines never show.
  const pendingAgentRef = useRef<string[]>([]);
  const paceTimerRef = useRef<ReturnType<typeof setTimeout> | null>(null);
  // Staleness sweep for the interim line (see INTERIM_STALE_MS).
  const interimTimerRef = useRef<ReturnType<typeof setTimeout> | null>(null);
  // Live cognition cadence: bumped per streamed chunk, decays between them, so
  // the Presence pulses with the agent's real output rate (not a canned loop).
  const activityRef = useRef(new ActivityMeter());

  const clearInterim = useCallback(() => {
    if (interimTimerRef.current !== null) {
      clearTimeout(interimTimerRef.current);
      interimTimerRef.current = null;
    }
    setInterim(null);
  }, []);

  // Each partial replaces the slot wholesale (the text is cumulative), and
  // re-arms the staleness sweep.
  const updateInterim = useCallback(
    (text: string) => {
      setInterim(text);
      if (interimTimerRef.current !== null) clearTimeout(interimTimerRef.current);
      interimTimerRef.current = setTimeout(() => clearInterim(), INTERIM_STALE_MS);
    },
    [clearInterim],
  );

  // Reveal queued agent sentences one at a time, paced to ~speaking rate, so the
  // words track the voice instead of all landing at once. Reschedules itself
  // until the queue drains.
  const pumpAgent = useCallback(() => {
    const text = pendingAgentRef.current.shift();
    if (text === undefined) {
      paceTimerRef.current = null;
      return;
    }
    sentenceIdRef.current += 1;
    const id = sentenceIdRef.current;
    setSentences((prev) => dropPriorTurns([...prev, { id, text, speaker: "agent" }]));
    const ms = Math.max(MIN_REVEAL_MS, text.length * REVEAL_MS_PER_CHAR);
    paceTimerRef.current = setTimeout(pumpAgent, ms);
    // eslint-disable-next-line react-hooks/exhaustive-deps
  }, []);

  // Queue freshly-arrived reply sentences. An idle pacer reveals the first at
  // once, then paces the rest itself.
  const enqueueAgent = useCallback(
    (list: string[]) => {
      if (list.length === 0) return;
      pendingAgentRef.current.push(...list);
      if (paceTimerRef.current === null) pumpAgent();
    },
    [pumpAgent],
  );

  // Drop everything still queued to reveal and stop the pacer — used when the
  // human takes the floor (barge-in) or a new exchange begins.
  const clearAgentQueue = useCallback(() => {
    pendingAgentRef.current = [];
    if (paceTimerRef.current !== null) {
      clearTimeout(paceTimerRef.current);
      paceTimerRef.current = null;
    }
  }, []);

  // Fold a settled user line into the timeline and mark the agent as thinking
  // until its reply streams in. Every user line — typed or transcribed from
  // speech — arrives settled on the /in/text observe loop and funnels here, so
  // user speech and user text render identically.
  const finalizeUser = useCallback((text: string) => {
    const trimmed = text.trim();
    if (!trimmed) return;
    // A new user line supersedes any reply still queued to reveal.
    clearAgentQueue();
    sentenceIdRef.current += 1;
    const item: SpeechItem = { id: sentenceIdRef.current, text: trimmed, speaker: "user" };
    // A new user line opens a new exchange — drop the prior turn so this line
    // (and the reply about to stream) is what stays on screen.
    setSentences((prev) => dropPriorTurns([...prev, item]));
    setAwaiting(true);
  }, [clearAgentQueue]);

  // ---- GET /out/text subscription loop (after wake) ----------------------
  useEffect(() => {
    if (!woken) return;
    const ctrl = new AbortController();
    let cancelled = false;
    const buffer = new SentenceBuffer();

    void (async () => {
      while (!cancelled) {
        try {
          let gotChunk = false;
          // Render the agent's words as they arrive. The mind only streams a
          // reply once it has committed to speaking (the human yielded the
          // floor), so there are no superseded drafts to untangle here.
          for await (const chunk of subscribeOutText({ scene, signal: ctrl.signal })) {
            if (cancelled) break;
            setOffline(false);
            if (!gotChunk) {
              gotChunk = true;
              setAwaiting(false);
              setAgentStreaming(true);
            }
            // Pulse the field with this chunk; larger bursts lift it more.
            activityRef.current.bump(Math.min(1, chunk.text.length / 40));
            enqueueAgent(buffer.push(chunk.text));
          }
          enqueueAgent(buffer.flush()); // body closed → utterance complete
          buffer.reset();
          setAgentStreaming(false);
        } catch {
          if (cancelled || ctrl.signal.aborted) break;
          setAgentStreaming(false);
          setOffline(true);
          await new Promise((r) => setTimeout(r, 1500));
        }
      }
    })();

    return () => {
      cancelled = true;
      ctrl.abort();
      clearAgentQueue();
    };
  }, [woken, scene, enqueueAgent, clearAgentQueue]);

  // ---- GET /out/audio subscription loop (TTS playback) -------------------
  // Pure render: each response is one turn's continuous audio. Stream its body
  // straight into the player as it arrives — no clip queue. The mind only puts
  // speech on the wire once it has committed to it, so there's nothing to gate
  // here.
  useEffect(() => {
    if (!woken) return;
    const ctrl = new AbortController();
    let cancelled = false;
    void (async () => {
      while (!cancelled) {
        try {
          for await (const turn of subscribeAudioTurns({ scene, signal: ctrl.signal })) {
            if (cancelled) break;
            const voice = voiceRef.current;
            if (!voice) continue;
            const token = voice.beginTurn(turn.mime);
            const reader = turn.body.getReader();
            try {
              while (!cancelled) {
                const { value, done } = await reader.read();
                if (done) break;
                if (value) voice.pushChunk(token, value);
              }
            } finally {
              voice.endTurn(token);
              reader.releaseLock();
            }
          }
        } catch {
          if (cancelled || ctrl.signal.aborted) break;
          await new Promise((r) => setTimeout(r, 1500));
        }
      }
    })();
    return () => {
      cancelled = true;
      ctrl.abort();
    };
  }, [woken, scene]);

  // Reflexive duck: recognized speech (a rolling partial on the observe
  // stream) lands while the agent's voice is playing → cut playback right
  // here, like a person stopping mid-word when you start talking. Nothing is
  // sent anywhere: the words buffer to the mind like any other signal, and the
  // backend infers from its own clock what went unheard. One-shot per turn:
  // after stop(), isPlaying() is false, so the partials that follow are no-ops.
  const duck = useCallback(() => {
    const voice = voiceRef.current;
    if (voice?.isPlaying()) voice.stop();
    // Drop any reply lines still queued to reveal — the human took the floor, so
    // the unheard tail shouldn't keep typing itself out after the voice stops.
    clearAgentQueue();
  }, [clearAgentQueue]);

  // ---- GET /in/text observe loop: typed lines (this client or another) ---
  // Every user line lands here: typed input the server echoes back, and speech
  // the server transcribed and posted to the text channel. Both render the same
  // way, so the conversation reads uniformly across every client in the scene.
  // Rolling partials (`final:false`, live STT) render as a live italic line
  // (the interim slot) and double as the duck trigger above.
  useEffect(() => {
    if (!woken) return;
    const ctrl = new AbortController();
    let cancelled = false;
    void (async () => {
      while (!cancelled) {
        try {
          for await (const ev of subscribeInText({ scene, signal: ctrl.signal })) {
            if (cancelled) break;
            setOffline(false);
            if (ev.text.trim().length === 0) continue;
            if (!ev.final) {
              duck();
              updateInterim(ev.text.trim());
              continue;
            }
            clearInterim();
            finalizeUser(ev.text);
          }
        } catch {
          if (cancelled || ctrl.signal.aborted) break;
          clearInterim();
          await new Promise((r) => setTimeout(r, 1500));
        }
      }
    })();
    return () => {
      cancelled = true;
      ctrl.abort();
      clearInterim();
    };
  }, [woken, scene, finalizeUser, duck, updateInterim, clearInterim]);

  // ---- audio-input channel: acquire/release the mic (and vision) ---------
  // Independent of the session itself — text and audio are coequal input
  // channels, each freely toggled on or off. Enabling needs the session's
  // AudioBus to already exist (built in startSession).
  const enableAudio = useCallback(async () => {
    const audioBus = busRef.current;
    // No session yet, already live, or a start is already in flight. The
    // micStartingRef check closes the async gap below: micRef is only set after
    // two awaits, so without it a concurrent second call would slip past and
    // open a duplicate socket.
    if (!audioBus || micRef.current || micStartingRef.current) return;
    micStartingRef.current = true;
    const gen = ++micGenRef.current;
    // True once a teardown (disableAudio/unmount) has superseded this start.
    const superseded = () => micGenRef.current !== gen;
    try {
      const stream = await navigator.mediaDevices.getUserMedia({
        audio: { channelCount: 1, echoCancellation: true, noiseSuppression: true },
      });
      if (superseded()) {
        stream.getTracks().forEach((t) => t.stop());
        return;
      }
      const micNode = audioBus.ctx.createMediaStreamSource(stream);
      audioBus.attachMic(micNode);

      // Passthrough: stream every mic frame to the backend; the upstream STT
      // segments and transcribes. No client-side VAD. The socket is upload-only
      // — the recognized text arrives on the /in/text observe loop above (the
      // server posts the transcript to the text channel), so even this client
      // reads its own words from there.
      const streamer = await AudioStreamer.create(audioBus.ctx, micNode, { scene });
      if (superseded()) {
        // Disabled while we were acquiring — don't leave the socket open.
        streamer.stop();
        stream.getTracks().forEach((t) => t.stop());
        return;
      }
      micStreamRef.current = stream;
      micRef.current = streamer;
      setAudioError(null);
      setAudioInput(true);
    } catch (err) {
      const msg = (err instanceof Error ? err.message : String(err)).toLowerCase();
      setAudioError(
        msg.includes("denied") || msg.includes("permission") || msg.includes("notallowed")
          ? "microphone permission needed"
          : "couldn't reach the microphone",
      );
      setAudioInput(false);
    } finally {
      // Leave the flag untouched if a newer start/teardown already owns it.
      if (!superseded()) micStartingRef.current = false;
    }
  }, [scene]);

  const disableAudio = useCallback(() => {
    // Cancel any enableAudio still acquiring devices, and clear the in-flight
    // flag so a later enable can start.
    micGenRef.current++;
    micStartingRef.current = false;
    micRef.current?.stop();
    micRef.current = null;
    micStreamRef.current?.getTracks().forEach((t) => t.stop());
    micStreamRef.current = null;
    setAudioInput(false);
  }, []);

  const toggleAudio = useCallback(() => {
    const next = !audioInput;
    prefsRef.current.audioInput = next;
    persistPrefs();
    if (next) void enableAudio();
    else disableAudio();
  }, [audioInput, disableAudio, enableAudio, persistPrefs]);

  // ---- vision-input channel: acquire/release the camera ------------------
  // A continuous channel like the mic, but fully independent — usable with or
  // without audio, and toggled on its own. The camera streams continuously as
  // WebM (MediaRecorder → WS); the backend decides how much to look. No
  // client-side sampling.
  const enableVision = useCallback(async () => {
    if (visionRef.current) return; // already live
    try {
      // `ideal` at 4K asks for the camera's best; the browser clamps down to the
      // device's true native max rather than failing when 4K isn't available. But
      // `ideal` only steers size, not orientation: the returned mode follows the
      // camera's native sensor, so a portrait-native camera (a phone front cam,
      // iPhone Continuity Camera) hands back a vertical frame. Bias the request
      // toward the viewport's own orientation — landscape on a desktop screen,
      // portrait on an upright phone — so the feed reads the way the device does.
      const portrait =
        typeof window !== "undefined" &&
        window.matchMedia?.("(orientation: portrait)").matches;
      const long = { ideal: 3840 };
      const short = { ideal: 2160 };
      // The width/height `ideal` only steers size; an `aspectRatio` hint is what
      // tips the browser off a portrait-native high-res mode toward a landscape
      // one (when the camera exposes both). Still a hint, not a guarantee.
      const aspectRatio = { ideal: portrait ? 9 / 16 : 16 / 9 };
      const videoStream = await navigator.mediaDevices.getUserMedia({
        video: portrait
          ? { width: short, height: long, aspectRatio }
          : { width: long, height: short, aspectRatio },
      });
      const got = videoStream.getVideoTracks()[0]?.getSettings();
      console.debug("[vision] captured", got?.width, "x", got?.height, got);
      visionStreamRef.current = videoStream;
      visionRef.current = await VideoStreamer.create(videoStream, { scene });
      // Start the presence lane on the same stream — a cheap low-res still feed for
      // real-time local face recognition, beside the full-fidelity video upload.
      presenceRef.current = new PresenceStiller(videoStream, { scene });
      setVisionStream(videoStream);
      setVideoError(null);
      setVideoInput(true);
    } catch (err) {
      // Stop a half-acquired stream so a denied/failed start leaves no camera on.
      visionStreamRef.current?.getTracks().forEach((t) => t.stop());
      visionStreamRef.current = null;
      setVisionStream(null);
      const msg = (err instanceof Error ? err.message : String(err)).toLowerCase();
      setVideoError(
        msg.includes("denied") || msg.includes("permission") || msg.includes("notallowed")
          ? "camera permission needed"
          : "couldn't reach the camera",
      );
      setVideoInput(false);
    }
  }, [scene]);

  const disableVision = useCallback(() => {
    visionRef.current?.stop();
    visionRef.current = null;
    presenceRef.current?.stop();
    presenceRef.current = null;
    visionStreamRef.current?.getTracks().forEach((t) => t.stop());
    visionStreamRef.current = null;
    setVisionStream(null);
    setVideoInput(false);
  }, []);

  const toggleVideo = useCallback(() => {
    const next = !videoInput;
    prefsRef.current.videoInput = next;
    persistPrefs();
    if (next) void enableVision();
    else disableVision();
  }, [videoInput, disableVision, enableVision, persistPrefs]);

  // ---- voice output channel: mute/unmute the agent's TTS -----------------
  // Independent of everything else — silencing the voice leaves the agent's
  // words flowing as text on /out/text.
  const toggleAudioOutput = useCallback(() => {
    setAudioOutput((on) => {
      const next = !on;
      prefsRef.current.audioOutput = next;
      persistPrefs();
      voiceRef.current?.setMuted(!next);
      return next;
    });
  }, [persistPrefs]);

  // ---- text input channel: show/hide the input line ----------------------
  const setTextChannel = useCallback(
    (on: boolean) => {
      prefsRef.current.textInput = on;
      persistPrefs();
      setTextInput(on);
    },
    [persistPrefs],
  );

  // ---- start the session: build the output graph, restore channels -------
  // Runs once on mount — no wake gate, no dedicated gesture. Building the
  // AudioBus is always allowed; the context may start suspended (autoplay
  // policy), so we resume on the first incidental interaction, which unlocks TTS
  // without a tap. Input channels are then restored *honestly*: a saved-on
  // mic/camera is re-acquired only when its permission is already granted (a
  // silent restore). If it can't be restored silently the channel stays off —
  // the control shows it off, and a click re-requests the device (that click is
  // the gesture/permission prompt the browser wants).
  const startSession = useCallback(() => {
    if (woken) return;
    void (async () => {
      try {
        const audioBus = new AudioBus();
        await audioBus.resume();
        if (audioBus.ctx.state !== "running") {
          const events = ["pointerdown", "keydown", "touchstart"];
          const resumeOnGesture = () => {
            void audioBus.resume();
            for (const ev of events) window.removeEventListener(ev, resumeOnGesture);
          };
          for (const ev of events) window.addEventListener(ev, resumeOnGesture);
        }
        const voice = new VoicePlayer(
          audioBus,
          () => setTtsPlaying(true),
          () => setTtsPlaying(false),
        );
        busRef.current = audioBus;
        voiceRef.current = voice;
        const prefs = prefsRef.current;
        voice.setMuted(!prefs.audioOutput);
        setBus(audioBus);
        setWoken(true);
        // Restore input channels only when they can be restored silently. A
        // saved-on channel whose permission isn't already granted stays off,
        // honestly reflecting that we couldn't restore it — a click enables it.
        if (prefs.audioInput && (await permissionGranted("microphone"))) void enableAudio();
        if (prefs.videoInput && (await permissionGranted("camera"))) void enableVision();
      } catch (err) {
        // The output graph couldn't be built (no Web Audio, etc.). The text
        // channel still works, so mark the session up and leave audio off.
        console.debug("[session] audio graph unavailable", err);
        setWoken(true);
      }
    })();
  }, [woken, enableAudio, enableVision]);

  // Auto-start on mount — the session builds itself and restores channels per
  // the honest policy above. The ref guard keeps StrictMode's double-invoke (and
  // any re-render of startSession) from starting a second graph.
  const startedRef = useRef(false);
  useEffect(() => {
    if (startedRef.current) return;
    startedRef.current = true;
    startSession();
  }, [startSession]);

  // cleanup on unmount
  useEffect(() => {
    return () => {
      // Cancel an in-flight enableAudio so a start that resolves post-unmount
      // tears its own socket down instead of leaking it.
      micGenRef.current++;
      micStartingRef.current = false;
      micRef.current?.stop();
      visionRef.current?.stop();
      presenceRef.current?.stop();
      voiceRef.current?.stop();
      micStreamRef.current?.getTracks().forEach((t) => t.stop());
      visionStreamRef.current?.getTracks().forEach((t) => t.stop());
      busRef.current?.close();
    };
  }, []);

  // ---- keyboard fallback send --------------------------------------------
  const sendText = useCallback(
    (text: string) => {
      const trimmed = text.trim();
      if (!trimmed) return;
      // The server echoes the line back on /in/text, where the observe loop folds
      // it into the timeline as a user line — so we don't add it locally.
      setAwaiting(true);
      postInText({ scene, body: trimmed }).catch(() => setAwaiting(false));
    },
    [scene],
  );

  const state: PresenceState = !woken
    ? "waking"
    : offline
      ? "offline"
      : agentStreaming || ttsPlaying
        ? "speaking"
        : awaiting
          ? "thinking"
          : "idle";

  // Dots track the agent's voice while it plays.
  const reactive = state === "speaking" && ttsPlaying;

  // Render the current exchange — the user's line pinned, the agent's reply
  // rolling beneath it — with the live interim line (speech still being
  // recognized) trailing last, so it also lands in the captions window when a
  // view holds the stage.
  const displaySentences = useMemo<SpeechItem[]>(() => {
    const visible = visibleExchange(sentences);
    if (interim === null) return visible;
    return [...visible, { id: INTERIM_ID, text: interim, speaker: "user", pending: true }];
  }, [sentences, interim]);

  return {
    state,
    reactive,
    bus,
    activity: activityRef.current,
    sentences: displaySentences,
    woken,
    audioInput,
    audioError,
    videoInput,
    videoError,
    visionStream,
    audioOutput,
    textInput,
    toggleAudio,
    toggleVideo,
    toggleAudioOutput,
    setTextChannel,
    sendText,
  };
}
