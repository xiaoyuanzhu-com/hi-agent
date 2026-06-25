// The scrollable message list. Sticks to the bottom as new messages arrive, the
// way a messaging thread does, unless the user has scrolled up to read history.

import { useEffect, useLayoutEffect, useRef } from "react";

import { Bubble } from "./Bubble";
import { type ChatMsg } from "./model";

export function MessageList({ messages, loading }: { messages: ChatMsg[]; loading: boolean }) {
  const ref = useRef<HTMLDivElement>(null);
  const stick = useRef(true);

  // Track whether we're pinned to the bottom, so a reply doesn't yank the view
  // away while the user is reading older messages.
  function onScroll() {
    const el = ref.current;
    if (!el) return;
    const slack = el.scrollHeight - el.scrollTop - el.clientHeight;
    stick.current = slack < 40;
  }

  useLayoutEffect(() => {
    const el = ref.current;
    if (el && stick.current) el.scrollTop = el.scrollHeight;
  }, [messages]);

  useEffect(() => {
    const el = ref.current;
    if (el) el.scrollTop = el.scrollHeight;
  }, []);

  return (
    <div className="chat-list" ref={ref} onScroll={onScroll}>
      {loading && messages.length === 0 ? (
        <div className="chat-empty">…</div>
      ) : messages.length === 0 ? (
        <div className="chat-empty">说点什么吧</div>
      ) : (
        messages.map((m) => <Bubble key={m.id} msg={m} />)
      )}
    </div>
  );
}
