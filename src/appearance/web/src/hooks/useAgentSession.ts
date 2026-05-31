import { useCallback, useEffect, useMemo, useRef, useState } from "react";
import { subscribeThought, postThought } from "../channels/thought";
import { subscribeAudio } from "../channels/audio";
import { SttStream } from "../channels/stt";
import { subscribeSurface, type SurfaceEnvelope } from "../channels/surface";
import { AudioBus } from "../lib/audioBus";
import { ActivityMeter } from "../lib/activityMeter";
import { MicCapture } from "../lib/micCapture";
import { VoicePlayer } from "../lib/voicePlayer";
import { SentenceBuffer } from "../lib/sentences";
import { getPeer } from "../lib/peer";
import type { PresenceState } from "../ui/Presence";
import type { SpeechItem } from "../ui/SpeechText";

// How many recent sentences stay on screen (calm, 1–2 at a time).
const SENTENCE_WINDOW = 2;
// Stable id for the in-progress user line so React updates it in place.
const LIVE_USER_ID = -1;

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
  wake: () => void;
  sendText: (text: string) => void;
  dismissSurface: () => void;
  openSurface: (surface: SurfaceEnvelope) => void;
}

/**
 * The coordinator — deliberately a *dumb face*. After the wake gesture it owns
 * the mic (AudioBus + MicCapture/VAD → streaming STT) and subscribes to the
 * output channels, rendering whatever arrives: /audio clips play on arrival,
 * /thought chunks fade in as whole sentences.
 *
 * Crucially it does NOT decide turns. Turn-taking — when the agent speaks, what
 * counts as the human yielding the floor, which drafts to suppress — lives in
 * the mind (the reactor). The face contributes exactly one output-side behavior:
 * a local **barge-in reflex**, muting the speaker the instant its own mic goes
 * hot (don't talk over the human, don't play into a hot mic). Everything else is
 * pass-through. The reactor learns the live floor state from the STT socket's
 * lifetime, so the client needs no extra control channel.
 */
export function useAgentSession(): AgentSession {
  const peer = useMemo(() => getPeer(), []);

  const [woken, setWoken] = useState(false);
  const [waking, setWaking] = useState(false);
  const [wakeError, setWakeError] = useState<string | null>(null);
  const [bus, setBus] = useState<AudioBus | null>(null);
  const [sentences, setSentences] = useState<SpeechItem[]>([]);
  // The user's live transcript while they speak: a rolling preliminary until
  // the polished final lands and is folded into `sentences`.
  const [userLive, setUserLive] = useState<string | null>(null);

  const [userSpeaking, setUserSpeaking] = useState(false);
  const [agentStreaming, setAgentStreaming] = useState(false);
  const [awaiting, setAwaiting] = useState(false);
  const [ttsPlaying, setTtsPlaying] = useState(false);
  const [offline, setOffline] = useState(false);
  const [activeSurface, setActiveSurface] = useState<SurfaceEnvelope | null>(null);
  const [surfaceHistory, setSurfaceHistory] = useState<SurfaceEnvelope[]>([]);

  const busRef = useRef<AudioBus | null>(null);
  const micRef = useRef<MicCapture | null>(null);
  const micStreamRef = useRef<MediaStream | null>(null);
  const voiceRef = useRef<VoicePlayer | null>(null);
  const sttRef = useRef<SttStream | null>(null);
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
          for await (const chunk of subscribeThought({ peer, signal: ctrl.signal })) {
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
  }, [woken, peer]);

  // ---- GET /audio subscription loop (Phase 2: TTS playback) --------------
  // Pure render: play each clip as it arrives. The mind only puts speech on the
  // wire once it has committed to it, so there's nothing to gate or discard
  // here. Barge-in is handled locally in onSpeechStart (voice.stop()).
  useEffect(() => {
    if (!woken) return;
    const ctrl = new AbortController();
    let cancelled = false;
    void (async () => {
      while (!cancelled) {
        try {
          for await (const blob of subscribeAudio({ peer, signal: ctrl.signal })) {
            if (cancelled) break;
            voiceRef.current?.enqueue(blob);
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
  }, [woken, peer]);

  // ---- GET /surface subscription loop (Phase 3: content overlays) --------
  useEffect(() => {
    if (!woken) return;
    const ctrl = new AbortController();
    let cancelled = false;
    void (async () => {
      while (!cancelled) {
        try {
          for await (const env of subscribeSurface({ peer, signal: ctrl.signal })) {
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
  }, [woken, peer]);

  // ---- wake: acquire mic, build the audio graph --------------------------
  const wake = useCallback(() => {
    if (woken || waking) return;
    setWaking(true);
    setWakeError(null);
    void (async () => {
      try {
        const stream = await navigator.mediaDevices.getUserMedia({
          audio: { channelCount: 1, echoCancellation: true, noiseSuppression: true },
        });
        const audioBus = new AudioBus();
        await audioBus.resume();
        // Autoplay policy: on an auto-wake with no user gesture this page load,
        // the context can stay suspended — which mutes TTS *and* stalls the
        // mic's ScriptProcessor (no listening). If so, resume on the first
        // incidental interaction so audio unlocks without a dedicated tap.
        if (audioBus.ctx.state !== "running") {
          const events = ["pointerdown", "keydown", "touchstart"];
          const resumeOnGesture = () => {
            void audioBus.resume();
            for (const ev of events) window.removeEventListener(ev, resumeOnGesture);
          };
          for (const ev of events) window.addEventListener(ev, resumeOnGesture);
        }
        const micNode = audioBus.ctx.createMediaStreamSource(stream);
        audioBus.attachMic(micNode);
        const voice = new VoicePlayer(
          audioBus,
          () => setTtsPlaying(true),
          () => setTtsPlaying(false),
        );
        // Fold a polished final transcript into the sentence timeline as a
        // user line, then let the agent's reply stream in over /thought.
        const finalizeUser = (text: string) => {
          const trimmed = text.trim();
          setUserLive(null);
          if (!trimmed) return;
          sentenceIdRef.current += 1;
          const item: SpeechItem = {
            id: sentenceIdRef.current,
            text: trimmed,
            speaker: "user",
          };
          setSentences((prev) => {
            const next = [...prev, item];
            return next.length > SENTENCE_WINDOW ? next.slice(next.length - SENTENCE_WINDOW) : next;
          });
          setAwaiting(true); // agent is now thinking until /thought chunks arrive
        };

        const mic = new MicCapture(audioBus.ctx, micNode, {
          onSpeechStart: () => {
            setUserSpeaking(true);
            // Barge-in reflex: the moment our mic goes hot, mute the speaker so
            // we never play over the human. This is the face's *only* output
            // decision; whether/what to say next is the mind's call. Opening the
            // STT socket below also signals the reactor that the floor is held.
            voice.stop();
            sttRef.current?.close(); // drop any prior utterance's socket
            sttRef.current = new SttStream(peer, {
              onPartial: (text) => setUserLive(text),
              onFinal: (text) => {
                finalizeUser(text);
                sttRef.current?.close();
                sttRef.current = null;
              },
              onClose: () => {
                setUserLive(null);
                sttRef.current = null;
              },
            });
          },
          onSpeechEnd: () => {
            setUserSpeaking(false);
            // Finalize this utterance. The socket stays open until the final
            // lands, then closes — which is what tells the reactor the floor has
            // been released. The mind decides if/when to reply from there.
            sttRef.current?.end();
          },
          onChunk: (pcm16) => sttRef.current?.sendPcm(pcm16),
        });
        micStreamRef.current = stream;
        busRef.current = audioBus;
        micRef.current = mic;
        voiceRef.current = voice;
        setBus(audioBus);
        setWoken(true);
        setWaking(false);
      } catch (err) {
        const msg = (err instanceof Error ? err.message : String(err)).toLowerCase();
        setWakeError(
          msg.includes("denied") || msg.includes("permission") || msg.includes("notallowed")
            ? "microphone permission needed — tap to retry"
            : "couldn't reach the microphone — tap to retry",
        );
        setWaking(false);
      }
    })();
  }, [woken, waking, peer]);

  // Auto-wake on repeat visits: if the mic was already granted on a prior
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
      sttRef.current?.close();
      voiceRef.current?.stop();
      micStreamRef.current?.getTracks().forEach((t) => t.stop());
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
      postThought({ from: peer, body: trimmed }).catch(() => setAwaiting(false));
    },
    [peer],
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

  // Render the finalized timeline plus the user's in-progress line (if any),
  // windowed to keep the display calm.
  const displaySentences = useMemo<SpeechItem[]>(() => {
    const base =
      userLive !== null
        ? [...sentences, { id: LIVE_USER_ID, text: userLive, speaker: "user" as const, pending: true }]
        : sentences;
    return base.length > SENTENCE_WINDOW ? base.slice(base.length - SENTENCE_WINDOW) : base;
  }, [sentences, userLive]);

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
    wake,
    sendText,
    dismissSurface,
    openSurface,
  };
}
