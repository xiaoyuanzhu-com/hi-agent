import { useCallback, useEffect, useMemo, useRef, useState } from "react";
import { subscribeThought, postThought } from "../channels/thought";
import { postAudio } from "../channels/audio";
import { AudioBus } from "../lib/audioBus";
import { MicCapture } from "../lib/micCapture";
import { SentenceBuffer } from "../lib/sentences";
import { getPeer } from "../lib/peer";
import type { PresenceState } from "../ui/Presence";
import type { SpeechItem } from "../ui/SpeechText";

// How many recent sentences stay on screen (calm, 1–2 at a time).
const SENTENCE_WINDOW = 2;

export interface AgentSession {
  state: PresenceState;
  reactive: boolean;
  bus: AudioBus | null;
  sentences: SpeechItem[];
  woken: boolean;
  waking: boolean;
  wakeError: string | null;
  wake: () => void;
  sendText: (text: string) => void;
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

  const [userSpeaking, setUserSpeaking] = useState(false);
  const [agentStreaming, setAgentStreaming] = useState(false);
  const [awaiting, setAwaiting] = useState(false);
  const [offline, setOffline] = useState(false);

  const busRef = useRef<AudioBus | null>(null);
  const micRef = useRef<MicCapture | null>(null);
  const micStreamRef = useRef<MediaStream | null>(null);
  const sentenceIdRef = useRef(0);

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
          next = [...next, { id: sentenceIdRef.current, text }];
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
        const micNode = audioBus.ctx.createMediaStreamSource(stream);
        audioBus.attachMic(micNode);
        const mic = new MicCapture(audioBus.ctx, micNode, {
          onSpeechStart: () => setUserSpeaking(true),
          onSpeechEnd: () => setUserSpeaking(false),
          onUtterance: ({ blob, mime }) => {
            setAwaiting(true);
            postAudio({ from: peer, blob, mime }).catch(() => setAwaiting(false));
          },
        });
        micStreamRef.current = stream;
        busRef.current = audioBus;
        micRef.current = mic;
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

  // cleanup on unmount
  useEffect(() => {
    return () => {
      micRef.current?.stop();
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

  const state: PresenceState = !woken
    ? "waking"
    : offline
      ? "offline"
      : userSpeaking
        ? "listening"
        : agentStreaming
          ? "speaking"
          : awaiting
            ? "thinking"
            : "idle";

  // Phase 1: dots track live audio only while listening to the user.
  // Phase 2 will also set this true during TTS playback.
  const reactive = state === "listening";

  return { state, reactive, bus, sentences, woken, waking, wakeError, wake, sendText };
}
