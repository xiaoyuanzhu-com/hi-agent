import { useCallback, useEffect, useMemo, useRef, useState } from "react";
import { subscribeThought, postThought } from "../channels/thought";
import { subscribeAudio } from "../channels/audio";
import { SttStream } from "../channels/stt";
import { subscribeSurface, type SurfaceEnvelope } from "../channels/surface";
import { AudioBus } from "../lib/audioBus";
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
// Courtesy pause after the user stops before the agent's held reply is voiced —
// a small floor-yield margin so a quick follow-up breath isn't talked over. The
// VAD's endSilenceMs already gates *detecting* the stop; this is on top.
const FLOOR_SETTLE_MS = 350;

export interface AgentSession {
  state: PresenceState;
  reactive: boolean;
  /** 0..1 — how much the presence should dim for the content overlay. */
  demote: number;
  bus: AudioBus | null;
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
 * The coordinator. After the wake gesture it owns the mic (AudioBus +
 * MicCapture/VAD → POST /audio), the GET /thought subscription (chunks →
 * whole-sentence fade), and the derived presence state machine.
 *
 * Barge-in is free in Phase 1: a new POST /audio cancels the in-flight routing
 * turn server-side, which closes the streaming /thought body and re-subscribes.
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

  // ---- output floor control (turn-tagged TTS) ----------------------------
  // The agent's speaking is its own channel: we voice only the latest cognition
  // turn and hold it until the user yields the floor. `activeTurnRef` is the
  // highest turn id seen; clips from older (superseded) turns are discarded.
  // `pendingAudioRef` buffers the live turn's clips while the floor is closed so
  // the kept reply's opening is never dropped — it's released, in order, once
  // the floor opens.
  const activeTurnRef = useRef(-1);
  const pendingAudioRef = useRef<Blob[]>([]);
  const floorOpenRef = useRef(true);
  const settleTimerRef = useRef<number | null>(null);

  const releasePending = useCallback(() => {
    if (!floorOpenRef.current) return;
    const voice = voiceRef.current;
    if (!voice) return;
    const clips = pendingAudioRef.current;
    if (clips.length === 0) return;
    pendingAudioRef.current = [];
    for (const blob of clips) voice.enqueue(blob);
  }, []);

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
          for await (const chunk of subscribeThought({ peer, signal: ctrl.signal })) {
            if (cancelled) break;
            setOffline(false);
            if (!gotChunk) {
              gotChunk = true;
              setAwaiting(false);
              setAgentStreaming(true);
            }
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
  useEffect(() => {
    if (!woken) return;
    const ctrl = new AbortController();
    let cancelled = false;
    void (async () => {
      while (!cancelled) {
        try {
          for await (const { blob, turn } of subscribeAudio({ peer, signal: ctrl.signal })) {
            if (cancelled) break;
            if (turn < activeTurnRef.current) continue; // superseded draft — discard
            if (turn > activeTurnRef.current) {
              // A newer cognition turn supersedes the old: drop any buffered or
              // playing clips from the previous turn so only the latest reply speaks.
              activeTurnRef.current = turn;
              pendingAudioRef.current = [];
              voiceRef.current?.stop();
            }
            pendingAudioRef.current.push(blob);
            releasePending(); // voices now if the floor is open, else holds
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
            // User takes the floor: hold output. Keep any buffered reply (it's
            // the latest known) — it's only discarded once a newer turn lands.
            floorOpenRef.current = false;
            if (settleTimerRef.current !== null) {
              window.clearTimeout(settleTimerRef.current);
              settleTimerRef.current = null;
            }
            voice.stop(); // barge-in: stop speaking the moment the user does
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
            // Yield the floor after a short settle, then voice whatever reply
            // has buffered for the live turn.
            if (settleTimerRef.current !== null) window.clearTimeout(settleTimerRef.current);
            settleTimerRef.current = window.setTimeout(() => {
              settleTimerRef.current = null;
              floorOpenRef.current = true;
              releasePending();
            }, FLOOR_SETTLE_MS);
            sttRef.current?.end(); // finalize; socket stays open for the final
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
      if (settleTimerRef.current !== null) window.clearTimeout(settleTimerRef.current);
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
