// The chat popup's message coordinator: seed the backlog from `GET /api/history`,
// then keep it live by following the same channels the agent face uses —
// `GET /api/out/text` (agent replies, one streamed utterance at a time) and
// `GET /api/in/text` (the user's typed lines and spoken transcripts, echoed back).
//
// Unlike the ambient face (which windows the last few lines and drops scrollback),
// a chat log *retains* every settled message, so this accumulates into one ordered
// array. Streaming/rolling bubbles live in the same array with `pending: true` and
// settle in place, which keeps arrival order correct without a sort.

import { useCallback, useEffect, useState } from "react";

import { subscribeInText } from "../channels/in/text";
import { subscribeOutText } from "../channels/out/text";
import { type ChatMsg, type RawHistoryMsg, historyToMessages, localId } from "./model";

/** Sentinel id for the single in-flight rolling-transcript bubble. */
const PARTIAL_IN = "in-partial";

export interface ChatController {
  messages: ChatMsg[];
  /** True until the initial history load resolves. */
  loading: boolean;
  /** Append a local message (an optimistic just-sent attachment). */
  pushLocal: (msg: ChatMsg) => void;
}

export function useChatMessages(scene: string): ChatController {
  const [messages, setMessages] = useState<ChatMsg[]>([]);
  const [loading, setLoading] = useState(true);

  const pushLocal = useCallback((msg: ChatMsg) => {
    setMessages((xs) => [...xs, msg]);
  }, []);

  useEffect(() => {
    const ac = new AbortController();
    const { signal } = ac;

    // Seed the backlog first, *then* open the live streams — so a reply that lands
    // mid-load can't be clobbered by the history replace.
    void (async () => {
      try {
        const res = await fetch(`/api/history?scene=${encodeURIComponent(scene)}`, {
          signal,
          cache: "no-store",
        });
        if (res.ok) {
          setMessages(historyToMessages((await res.json()) as RawHistoryMsg[]));
        }
      } catch {
        /* ignore — start from an empty log */
      } finally {
        setLoading(false);
      }
      followAgentReplies(scene, signal, setMessages);
      followUserInputs(scene, signal, setMessages);
    })();

    return () => ac.abort();
  }, [scene]);

  return { messages, loading, pushLocal };
}

type SetMsgs = React.Dispatch<React.SetStateAction<ChatMsg[]>>;

/** Each agent utterance streams into one bubble that settles when the body closes. */
async function followAgentReplies(scene: string, signal: AbortSignal, setMessages: SetMsgs) {
  while (!signal.aborted) {
    const id = localId("out");
    let acc = "";
    let started = false;
    try {
      for await (const chunk of subscribeOutText({ scene, signal })) {
        acc += chunk.text;
        if (!started) {
          started = true;
          setMessages((xs) => [...xs, { id, ts: Date.now(), dir: "out", text: acc, pending: true }]);
        } else {
          setMessages((xs) => xs.map((m) => (m.id === id ? { ...m, text: acc } : m)));
        }
      }
    } catch {
      if (signal.aborted) return;
      await sleep(500); // transient failure — back off and re-subscribe
      continue;
    }
    if (started) {
      setMessages((xs) => xs.map((m) => (m.id === id ? { ...m, pending: false } : m)));
    }
  }
}

/** Typed lines settle to a bubble; a rolling transcript updates one pending bubble. */
async function followUserInputs(scene: string, signal: AbortSignal, setMessages: SetMsgs) {
  while (!signal.aborted) {
    try {
      for await (const ev of subscribeInText({ scene, signal })) {
        if (ev.final) {
          setMessages((xs) => [
            ...xs.filter((m) => m.id !== PARTIAL_IN),
            { id: localId("in"), ts: Date.now(), dir: "in", text: ev.text },
          ]);
        } else {
          setMessages((xs) => [
            ...xs.filter((m) => m.id !== PARTIAL_IN),
            { id: PARTIAL_IN, ts: Date.now(), dir: "in", text: ev.text, pending: true },
          ]);
        }
      }
    } catch {
      if (signal.aborted) return;
      await sleep(500);
    }
  }
}

function sleep(ms: number): Promise<void> {
  return new Promise((r) => setTimeout(r, ms));
}
