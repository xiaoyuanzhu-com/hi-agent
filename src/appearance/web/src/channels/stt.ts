// Client for the live ASR channel (GET /stt/stream, WebSocket).
//
// One socket per utterance: open on speech onset, stream 16 kHz mono 16-bit LE
// PCM as binary frames, send "end" when VAD detects the end of speech. The
// server proxies the audio to the streaming STT and pushes transcript updates
// back as JSON — rolling preliminaries first, then the polished final.

export interface SttStreamHandlers {
  /** Fast, still-changing preliminary text. */
  onPartial: (text: string) => void;
  /** Polished, finalized text for the utterance. */
  onFinal: (text: string) => void;
  /** Socket closed or errored before a final arrived. */
  onClose?: () => void;
}

interface TranscriptMsg {
  text: string;
  final: boolean;
}

/** A single utterance's streaming recognition. */
export class SttStream {
  private ws: WebSocket;
  private open = false;
  private closed = false;
  private gotFinal = false;
  private backlog: ArrayBuffer[] = [];

  constructor(peer: string, handlers: SttStreamHandlers) {
    const scheme = location.protocol === "https:" ? "wss" : "ws";
    const url = `${scheme}://${location.host}/stt/stream?peer=${encodeURIComponent(peer)}`;
    this.ws = new WebSocket(url);
    this.ws.binaryType = "arraybuffer";

    this.ws.onopen = () => {
      this.open = true;
      for (const buf of this.backlog) this.ws.send(buf);
      this.backlog = [];
    };
    this.ws.onmessage = (e) => {
      if (typeof e.data !== "string") return;
      let msg: TranscriptMsg;
      try {
        msg = JSON.parse(e.data) as TranscriptMsg;
      } catch {
        return;
      }
      if (msg.final) {
        this.gotFinal = true;
        handlers.onFinal(msg.text);
      } else {
        handlers.onPartial(msg.text);
      }
    };
    const finish = () => {
      if (this.closed) return;
      this.closed = true;
      if (!this.gotFinal) handlers.onClose?.();
    };
    this.ws.onerror = finish;
    this.ws.onclose = finish;
  }

  /** Queue/send one PCM chunk. Accepts the Int16Array's backing buffer. */
  sendPcm(pcm16: Int16Array): void {
    if (this.closed) return;
    const buf = pcm16.buffer.slice(
      pcm16.byteOffset,
      pcm16.byteOffset + pcm16.byteLength,
    ) as ArrayBuffer;
    if (this.open) this.ws.send(buf);
    else this.backlog.push(buf);
  }

  /** Signal end-of-utterance so the server finalizes. Keeps the socket open to
   *  receive the final transcript; the server closes it after sending. */
  end(): void {
    if (this.closed) return;
    if (this.open) {
      try {
        this.ws.send("end");
      } catch {
        /* ignore */
      }
    } else {
      // Never opened — nothing was streamed; just tear down.
      this.close();
    }
  }

  /** Abort immediately (barge-in / unmount). */
  close(): void {
    this.closed = true;
    this.backlog = [];
    try {
      this.ws.close();
    } catch {
      /* ignore */
    }
  }
}
