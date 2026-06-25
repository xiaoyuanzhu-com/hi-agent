// One message bubble. `in` (the user / their voice) sits right and accented; `out`
// (the agent) sits left and neutral. Any media renders by kind above the text.

import { type ChatMsg } from "./model";

export function Bubble({ msg }: { msg: ChatMsg }) {
  const side = msg.dir === "in" ? "chat-row--in" : "chat-row--out";
  const tone = msg.dir === "in" ? "chat-bubble--in" : "chat-bubble--out";
  const pending = msg.pending ? " chat-bubble--pending" : "";
  return (
    <div className={`chat-row ${side}`}>
      <div className={`chat-bubble ${tone}${pending}`}>
        {msg.media ? <Media msg={msg} /> : null}
        {msg.text ? <div className="chat-text">{msg.text}</div> : null}
      </div>
    </div>
  );
}

function Media({ msg }: { msg: ChatMsg }) {
  const m = msg.media;
  if (!m) return null;
  switch (m.kind) {
    case "image":
      return <img className="chat-media-img" src={m.url} alt={m.name ?? "image"} loading="lazy" />;
    case "audio":
      return <audio className="chat-media-audio" src={m.url} controls preload="none" />;
    case "video":
      return <video className="chat-media-video" src={m.url} controls preload="metadata" />;
    default:
      return (
        <a className="chat-file" href={m.url} target="_blank" rel="noreferrer">
          <span className="chat-file-icon" aria-hidden="true">
            📎
          </span>
          <span className="chat-file-name">{m.name ?? "附件"}</span>
        </a>
      );
  }
}
