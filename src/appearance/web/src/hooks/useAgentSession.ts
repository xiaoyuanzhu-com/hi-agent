import { useCallback, useEffect, useMemo, useRef, useState } from "react";
import { subscribeThought, postThought } from "../channels/thought";
import { subscribeAudioTurns } from "../channels/audio";
import { postVision } from "../channels/vision";
import { subscribeSurface, type SurfaceEnvelope } from "../channels/surface";
import { AudioBus } from "../lib/audioBus";
import { ActivityMeter } from "../lib/activityMeter";
import { AudioStreamer } from "../lib/audioStreamer";
import { VisionCapture } from "../lib/visionCapture";
import { VoicePlayer } from "../lib/voicePlayer";
import { SentenceBuffer } from "../lib/sentences";
import { getScene } from "../lib/scene";
import type { PresenceState } from "../ui/Presence";
import type { SpeechItem } from "../ui/SpeechText";

// How many recent sentences stay on screen (calm, 1–2 at a time).
const SENTENCE_WINDOW = 2;

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
  /** 0..1 — how much the presence should dim for the content overlay. */
  demote: number;
  bus: AudioBus | null;
  /** Live cognition cadence (streamed-chunk pulses) the field reacts to. */
  activity: ActivityMeter;
  sentences: SpeechItem[];
  activeSurface: SurfaceEnvelope | null;
  surfaceHistory: SurfaceEnvelope[];
  woken: boolean;
  waking: boolean;
  wakeError: string | null;
  /** Whether the mic (audio input) channel is currently live. */
  audioInput: boolean;
  /** Surfaced if turning the audio channel on failed (denied / no device). */
  audioError: string | null;
  /** Whether the camera (vision input) channel is currently live. */
  videoInput: boolean;
  /** Surfaced if turning the vision channel on failed (denied / no device). */
  videoError: string | null;
  /** Whether the agent's voice (audio output) channel is on. */
  audioOutput: boolean;
  /** Whether the text input channel is on (the input line is shown). */
  textInput: boolean;
  /** Begin the session with the audio channel on (the default tap). */
  wake: () => void;
  /** Begin the session text-only — no mic prompt; audio can be toggled on later. */
  startTextOnly: () => void;
  /** Flip the audio-input channel on/off independently of the others. */
  toggleAudio: () => void;
  /** Flip the vision-input channel on/off independently of the others. */
  toggleVideo: () => void;
  /** Flip the agent's voice (audio output) on/off; text output is unaffected. */
  toggleAudioOutput: () => void;
  /** Turn the text input channel on/off (shows/hides the input line). */
  setTextChannel: (on: boolean) => void;
  sendText: (text: string) => void;
  dismissSurface: () => void;
  openSurface: (surface: SurfaceEnvelope) => void;
}

/**
 * The coordinator — deliberately a *dumb face*. After the wake gesture it owns
 * the input channels (mic → /api/audio, one VAD-segmented WAV per utterance;
 * camera → /api/vision, a frame every couple seconds) and subscribes to the
 * output channels, rendering whatever arrives: /api/audio clips play on arrival,
 * /api/thought chunks fade in as whole sentences.
 *
 * Crucially it does NOT decide turns. Turn-taking — when the agent speaks, which
 * drafts to suppress — lives in the mind (the reactor), which commits after the
 * inbound signal stream goes quiet. The face contributes exactly one output-side
 * behavior: a local **barge-in reflex**, muting the speaker the instant its own
 * mic goes hot (don't talk over the human, don't play into a hot mic).
 * Everything else is pass-through: the input channels just stream the signal.
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
  const [waking, setWaking] = useState(false);
  const [wakeError, setWakeError] = useState<string | null>(null);
  const [bus, setBus] = useState<AudioBus | null>(null);
  const [sentences, setSentences] = useState<SpeechItem[]>([]);

  const [audioInput, setAudioInput] = useState(false);
  const [audioError, setAudioError] = useState<string | null>(null);
  const [videoInput, setVideoInput] = useState(false);
  const [videoError, setVideoError] = useState<string | null>(null);
  const [audioOutput, setAudioOutput] = useState(prefsRef.current.audioOutput);
  const [textInput, setTextInput] = useState(prefsRef.current.textInput);
  const [userSpeaking, setUserSpeaking] = useState(false);
  const [agentStreaming, setAgentStreaming] = useState(false);
  const [awaiting, setAwaiting] = useState(false);
  const [ttsPlaying, setTtsPlaying] = useState(false);
  const [offline, setOffline] = useState(false);
  const [activeSurface, setActiveSurface] = useState<SurfaceEnvelope | null>(null);
  const [surfaceHistory, setSurfaceHistory] = useState<SurfaceEnvelope[]>([]);

  const busRef = useRef<AudioBus | null>(null);
  const micRef = useRef<AudioStreamer | null>(null);
  const micStreamRef = useRef<MediaStream | null>(null);
  const voiceRef = useRef<VoicePlayer | null>(null);
  const visionRef = useRef<VisionCapture | null>(null);
  const visionStreamRef = useRef<MediaStream | null>(null);
  const sentenceIdRef = useRef(0);
  const surfaceTtlRef = useRef<number | null>(null);
  // Live cognition cadence: bumped per streamed chunk, decays between them, so
  // the Presence pulses with the agent's real output rate (not a canned loop).
  const activityRef = useRef(new ActivityMeter());

  // ---- GET /thought subscription loop (after wake) -----------------------
  useEffect(() => {
    if (!woken) return;
    const ctrl = new AbortController();
    let cancelled = false;
    const buffer = new SentenceBuffer();

    const pushSentences = (list: string[]) => {
      if (list.length === 0) return;
      setSentences((prev) => {
        let next = prev;
        for (const text of list) {
          sentenceIdRef.current += 1;
          next = [...next, { id: sentenceIdRef.current, text, speaker: "agent" }];
        }
        return next.length > SENTENCE_WINDOW
          ? next.slice(next.length - SENTENCE_WINDOW)
          : next;
      });
    };

    void (async () => {
      while (!cancelled) {
        try {
          let gotChunk = false;
          // Render the agent's words as they arrive. The mind only streams a
          // reply once it has committed to speaking (the human yielded the
          // floor), so there are no superseded drafts to untangle here.
          for await (const chunk of subscribeThought({ scene, signal: ctrl.signal })) {
            if (cancelled) break;
            setOffline(false);
            if (!gotChunk) {
              gotChunk = true;
              setAwaiting(false);
              setAgentStreaming(true);
            }
            // Pulse the field with this chunk; larger bursts lift it more.
            activityRef.current.bump(Math.min(1, chunk.text.length / 40));
            pushSentences(buffer.push(chunk.text));
          }
          pushSentences(buffer.flush()); // body closed → utterance complete
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
    };
  }, [woken, scene]);

  // ---- GET /audio subscription loop (Phase 2: TTS playback) --------------
  // Pure render: each response is one turn's continuous audio. Stream its body
  // straight into the player as it arrives — no clip queue. The mind only puts
  // speech on the wire once it has committed to it, so there's nothing to gate
  // here. Barge-in is handled locally in onSpeechStart (voice.stop()), which
  // invalidates the turn so any chunks still in flight are dropped.
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

  // ---- GET /surface subscription loop (Phase 3: content overlays) --------
  useEffect(() => {
    if (!woken) return;
    const ctrl = new AbortController();
    let cancelled = false;
    void (async () => {
      while (!cancelled) {
        try {
          for await (const env of subscribeSurface({ scene, signal: ctrl.signal })) {
            if (cancelled) break;
            if (env.op === "dismiss") {
              setActiveSurface((cur) => (cur && cur.id === env.id ? null : cur));
              continue;
            }
            setActiveSurface(env);
            setSurfaceHistory((prev) => [...prev, env]);
            if (surfaceTtlRef.current) window.clearTimeout(surfaceTtlRef.current);
            if (env.ttl_ms && env.ttl_ms > 0) {
              surfaceTtlRef.current = window.setTimeout(() => {
                setActiveSurface((cur) => (cur && cur.id === env.id ? null : cur));
              }, env.ttl_ms);
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

  // ---- audio-input channel: acquire/release the mic (and vision) ---------
  // Independent of the session itself — text and audio are coequal input
  // channels, each freely toggled on or off. Enabling needs the session's
  // AudioBus to already exist (built in startSession).
  const enableAudio = useCallback(async () => {
    const audioBus = busRef.current;
    if (!audioBus || micRef.current) return; // no session yet, or already live
    try {
      const stream = await navigator.mediaDevices.getUserMedia({
        audio: { channelCount: 1, echoCancellation: true, noiseSuppression: true },
      });
      const micNode = audioBus.ctx.createMediaStreamSource(stream);
      audioBus.attachMic(micNode);

      // Passthrough: stream every mic frame to the backend; the upstream STT
      // segments and transcribes. No client-side VAD. Barge-in and UI state are
      // driven by recognized text, not raw mic energy — so we never falsely
      // mute the agent.
      const streamer = new AudioStreamer(audioBus.ctx, micNode, {
        scene,
        onTranscript: ({ text, isFinal }) => {
          const heard = text.trim().length > 0;
          if (heard) {
            // First real recognized speech ducks the speaker — the face's only
            // output decision; whether/what to say next is the mind's call.
            voiceRef.current?.stop();
            setUserSpeaking(true);
          }
          if (isFinal && heard) {
            // Utterance finalized and dispatched server-side; show "thinking".
            setUserSpeaking(false);
            setAwaiting(true);
          }
        },
      });
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
    }
  }, [scene]);

  const disableAudio = useCallback(() => {
    micRef.current?.stop();
    micRef.current = null;
    micStreamRef.current?.getTracks().forEach((t) => t.stop());
    micStreamRef.current = null;
    setUserSpeaking(false);
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
  // without audio, and toggled on its own. Frames stream a couple seconds apart
  // and a dropped frame is fine, so posting is fire-and-forget.
  const enableVision = useCallback(async () => {
    if (visionRef.current) return; // already live
    try {
      const videoStream = await navigator.mediaDevices.getUserMedia({ video: true });
      visionStreamRef.current = videoStream;
      visionRef.current = new VisionCapture(videoStream, {
        onFrame: (frameBlob, frameMime) => {
          postVision({ scene, blob: frameBlob, mime: frameMime }).catch(() => {});
        },
      });
      setVideoError(null);
      setVideoInput(true);
    } catch (err) {
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
    visionStreamRef.current?.getTracks().forEach((t) => t.stop());
    visionStreamRef.current = null;
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
  // words flowing as text on /thought.
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

  // ---- start the session: build the output graph (no mic required) -------
  // A single user gesture is the one unavoidable interaction — browsers need it
  // to unlock audio *playback*. The mic is a separate, optional channel layered
  // on top via enableAudio(), so the session (and the text channel) work even
  // when audio can't be used at all.
  const startSession = useCallback(
    (opts?: { textOnly?: boolean }) => {
      if (woken || waking) return;
      setWaking(true);
      setWakeError(null);
      void (async () => {
        try {
          const audioBus = new AudioBus();
          await audioBus.resume();
          // Autoplay policy: on an auto-start with no gesture this page load the
          // context can stay suspended — which mutes TTS. Resume on the first
          // incidental interaction so audio unlocks without a dedicated tap.
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
          // Reapply the saved channel state. Voice output is the only one that
          // can be set before any device work; the input channels acquire below.
          const prefs = prefsRef.current;
          voice.setMuted(!prefs.audioOutput);
          setBus(audioBus);
          setWoken(true);
          setWaking(false);

          if (opts?.textOnly) {
            // The "type instead" entry (no usable audio): force text on, don't
            // touch the mic this session. The saved audio intent is left as-is.
            if (!prefs.textInput) setTextChannel(true);
          } else {
            if (prefs.audioInput) void enableAudio();
            if (prefs.videoInput) void enableVision();
          }
        } catch (err) {
          const msg = (err instanceof Error ? err.message : String(err)).toLowerCase();
          setWakeError(
            msg.includes("denied") || msg.includes("permission") || msg.includes("notallowed")
              ? "audio playback blocked — tap to retry"
              : "couldn't start the session — tap to retry",
          );
          setWaking(false);
        }
      })();
    },
    [woken, waking, enableAudio, enableVision, setTextChannel],
  );

  const wake = useCallback(() => startSession(), [startSession]);
  const startTextOnly = useCallback(() => startSession({ textOnly: true }), [startSession]);

  // Auto-start on repeat visits: if the mic was already granted on a prior
  // visit, skip the "tap to begin" gate and start listening straight away. The
  // gate still appears on the first-ever visit (permission "prompt") or after a
  // denial, where a user gesture is genuinely required to request the mic.
  useEffect(() => {
    let cancelled = false;
    const perms = navigator.permissions;
    if (!perms?.query) return;
    perms
      .query({ name: "microphone" as PermissionName })
      .then((status) => {
        if (!cancelled && status.state === "granted") wake();
      })
      .catch(() => {
        /* 'microphone' not queryable (e.g. Firefox) — keep the manual gate. */
      });
    return () => {
      cancelled = true;
    };
  }, [wake]);

  // cleanup on unmount
  useEffect(() => {
    return () => {
      micRef.current?.stop();
      visionRef.current?.stop();
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
      // SpeechText shows the agent's words only; the typed line isn't echoed there.
      setAwaiting(true);
      postThought({ scene, body: trimmed }).catch(() => setAwaiting(false));
    },
    [scene],
  );

  const dismissSurface = useCallback(() => {
    if (surfaceTtlRef.current) window.clearTimeout(surfaceTtlRef.current);
    setActiveSurface(null);
  }, []);

  const openSurface = useCallback((surface: SurfaceEnvelope) => {
    if (surfaceTtlRef.current) window.clearTimeout(surfaceTtlRef.current);
    setActiveSurface(surface);
  }, []);

  const state: PresenceState = !woken
    ? "waking"
    : offline
      ? "offline"
      : userSpeaking
        ? "listening"
        : agentStreaming || ttsPlaying
          ? "speaking"
          : awaiting
            ? "thinking"
            : "idle";

  // Dots track live audio while listening (mic) or while the agent's voice plays.
  const reactive = state === "listening" || (state === "speaking" && ttsPlaying);

  // The content overlay dims/demotes the presence — more for full-screen.
  const demote = activeSurface ? (activeSurface.mode === "full" ? 1 : 0.72) : 0;

  // Render the agent's recent words, windowed to keep the display calm. (User
  // speech isn't shown as text for now — that feature is deferred.)
  const displaySentences = useMemo<SpeechItem[]>(() => {
    return sentences.length > SENTENCE_WINDOW
      ? sentences.slice(sentences.length - SENTENCE_WINDOW)
      : sentences;
  }, [sentences]);

  return {
    state,
    reactive,
    demote,
    bus,
    activity: activityRef.current,
    sentences: displaySentences,
    activeSurface,
    surfaceHistory,
    woken,
    waking,
    wakeError,
    audioInput,
    audioError,
    videoInput,
    videoError,
    audioOutput,
    textInput,
    wake,
    startTextOnly,
    toggleAudio,
    toggleVideo,
    toggleAudioOutput,
    setTextChannel,
    sendText,
    dismissSurface,
    openSurface,
  };
}
