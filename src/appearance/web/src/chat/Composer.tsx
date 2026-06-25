// The input row: a growing text field plus an attach (＋) button. Typed lines post
// to `/api/in/text` and render from the echo (one source of truth, like the face).
// Attachments post to `/api/in/file`; since handed files ride the `file` channel —
// which the text observe stream doesn't echo — they render optimistically here.

import { useRef, useState } from "react";

import { postInText } from "../channels/in/text";
import { type ChatMsg, localId, mediaKindFromMime } from "./model";

export function Composer({
  scene,
  pushLocal,
}: {
  scene: string;
  pushLocal: (m: ChatMsg) => void;
}) {
  const [text, setText] = useState("");
  const [drag, setDrag] = useState(false);
  const fileRef = useRef<HTMLInputElement>(null);
  const inputRef = useRef<HTMLTextAreaElement>(null);

  async function sendText() {
    const body = text.trim();
    if (!body) return;
    setText("");
    try {
      await postInText({ scene, body });
    } catch (e) {
      console.error("chat: send failed", e);
    }
    inputRef.current?.focus();
  }

  async function sendFiles(files: File[]) {
    for (const file of files) {
      const id = localId("file");
      const mime = file.type || "application/octet-stream";
      pushLocal({
        id,
        ts: Date.now(),
        dir: "in",
        text: "",
        media: { url: URL.createObjectURL(file), mime, kind: mediaKindFromMime(mime), name: file.name },
        pending: true,
      });
      try {
        const fd = new FormData();
        fd.append("file", file, file.name);
        await fetch("/api/in/file", { method: "POST", headers: { "X-HI-Scene": scene }, body: fd });
      } catch (e) {
        console.error("chat: upload failed", e);
      }
    }
  }

  function onKeyDown(e: React.KeyboardEvent<HTMLTextAreaElement>) {
    if (e.key === "Enter" && !e.shiftKey) {
      e.preventDefault();
      void sendText();
    }
  }

  return (
    <div
      className={`chat-composer${drag ? " chat-composer--drag" : ""}`}
      onDragOver={(e) => {
        e.preventDefault();
        setDrag(true);
      }}
      onDragLeave={() => setDrag(false)}
      onDrop={(e) => {
        e.preventDefault();
        setDrag(false);
        const fs = e.dataTransfer?.files;
        if (fs?.length) void sendFiles([...fs]);
      }}
    >
      <button className="chat-attach" title="发送文件" onClick={() => fileRef.current?.click()}>
        ＋
      </button>
      <input
        ref={fileRef}
        type="file"
        multiple
        hidden
        onChange={(e) => {
          if (e.target.files?.length) void sendFiles([...e.target.files]);
          e.target.value = "";
        }}
      />
      <textarea
        ref={inputRef}
        className="chat-input"
        rows={1}
        placeholder="发消息…"
        value={text}
        onChange={(e) => setText(e.target.value)}
        onKeyDown={onKeyDown}
      />
      <button
        className="chat-send"
        title="发送"
        disabled={text.trim().length === 0}
        onClick={() => void sendText()}
      >
        ↑
      </button>
    </div>
  );
}
