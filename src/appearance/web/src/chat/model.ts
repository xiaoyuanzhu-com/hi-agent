// Chat message model — the small, framework-free types + mappings the menu-bar
// chat popup renders, kept pure so they're unit-testable without DOM or fetch.

export type MsgDir = "in" | "out";
export type MediaKind = "image" | "audio" | "video" | "file";

export interface ChatMedia {
  /** A `/api/media` URL (history) or a local object URL (just-sent attachment). */
  url: string;
  mime: string;
  kind: MediaKind;
  /** Filename for the file chip, when known (optimistic sends). */
  name?: string;
  width?: number;
  height?: number;
  durationMs?: number;
}

export interface ChatMsg {
  /** Stable key: the journal id (history) or a local id (live / optimistic). */
  id: string;
  /** Ms since epoch, for ordering. */
  ts: number;
  dir: MsgDir;
  text: string;
  media?: ChatMedia;
  /** Still streaming (agent reply) or rolling (live transcript) / in-flight upload. */
  pending?: boolean;
}

/** One message as returned by `GET /api/history`. */
export interface RawHistoryMsg {
  id: string;
  ts: string; // RFC 3339
  dir: MsgDir;
  channel: string;
  origin?: string;
  body: string;
  media?: {
    url: string;
    mime: string;
    kind: MediaKind;
    width?: number;
    height?: number;
    duration_ms?: number;
  };
}

export function mediaKindFromMime(mime: string): MediaKind {
  if (mime.startsWith("image/")) return "image";
  if (mime.startsWith("audio/")) return "audio";
  if (mime.startsWith("video/")) return "video";
  return "file";
}

/** Map the history payload into ordered chat messages (oldest first). */
export function historyToMessages(raw: RawHistoryMsg[]): ChatMsg[] {
  return raw.map((m) => ({
    id: m.id,
    ts: Date.parse(m.ts),
    dir: m.dir,
    text: m.body,
    media: m.media
      ? {
          url: m.media.url,
          mime: m.media.mime,
          kind: m.media.kind,
          width: m.media.width,
          height: m.media.height,
          durationMs: m.media.duration_ms,
        }
      : undefined,
  }));
}

let localSeq = 0;
/** A process-unique id for live / optimistic messages (never a journal id). */
export function localId(prefix: string): string {
  localSeq += 1;
  return `${prefix}-${Date.now()}-${localSeq}`;
}
