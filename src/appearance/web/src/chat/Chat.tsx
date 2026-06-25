// The menu-bar chat popup surface (`/chat`), rendered by the host SPA and loaded by
// the tray's WKWebView popover. It joins the desktop conversation: the popover opens
// `…/chat?scene=desktop`, so the same scene as the voice/gesture path, and the list
// reflects typed lines, agent replies, and spoken transcripts alike.

import { useMemo } from "react";

import { getScene } from "../lib/scene";
import { Composer } from "./Composer";
import { MessageList } from "./MessageList";
import { useChatMessages } from "./useChatMessages";
import "./chat.css";

export function Chat() {
  // The popover passes the scene in the URL (`?scene=desktop`); fall back to this
  // browser's persisted scene when opened directly.
  const scene = useMemo(() => {
    const q = new URLSearchParams(window.location.search).get("scene");
    return q && q.length > 0 ? q : getScene();
  }, []);

  const { messages, loading, pushLocal } = useChatMessages(scene);

  return (
    <div className="chat-root">
      <header className="chat-header">
        <span className="chat-mark" aria-label="hi">
          <span className="chat-mark-h">h</span>
          <span className="chat-mark-i">i</span>
        </span>
      </header>
      <MessageList messages={messages} loading={loading} />
      <Composer scene={scene} pushLocal={pushLocal} />
    </div>
  );
}
