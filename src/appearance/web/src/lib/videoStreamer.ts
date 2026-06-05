// VideoStreamer — continuous camera passthrough over a WebSocket.
//
// The camera stays open and `MediaRecorder` encodes it as a WebM stream; every
// chunk is shipped to the backend as a binary frame for the whole time the camera
// is on. This is the visual analog of `AudioStreamer`: UPLOAD-ONLY (nothing comes
// back on the socket) and there is NO client-side sampling — the client streams
// blindly and the backend decides how much to actually look. The first chunk
// carries the WebM init segment, which the backend caches so observers can join
// mid-stream.
//
// This replaces the old VisionCapture, which sampled one JPEG every couple
// seconds client-side — hard-coding perception fidelity the backend should own.

// Candidate recorder formats, best first. Each must be playable via MediaSource
// on the observer side, so we stick to WebM (VP9/VP8) — broadly supported on
// Chromium/Firefox for both MediaRecorder and MSE.
const CANDIDATE_MIMES = ["video/webm;codecs=vp9", "video/webm;codecs=vp8", "video/webm"];

// How often MediaRecorder emits a chunk. Small enough to feel live, large enough
// that each chunk is a usable media segment.
const TIMESLICE_MS = 250;

export interface VideoStreamerOptions {
  /** Scene identity; rides in the WS query string (browsers can't set headers). */
  scene: string;
  /** Fired when the socket closes (network drop or stop()). */
  onClose?: () => void;
}

function pickMime(): string | null {
  if (typeof MediaRecorder === "undefined" || !MediaRecorder.isTypeSupported) return null;
  return CANDIDATE_MIMES.find((m) => MediaRecorder.isTypeSupported(m)) ?? null;
}

export class VideoStreamer {
  private recorder: MediaRecorder;
  private ws: WebSocket;
  private backlog: Blob[] = [];
  private stopped = false;

  /**
   * Start streaming `stream`'s video to the backend. Async to mirror
   * `AudioStreamer.create`; throws if no supported WebM recorder format exists.
   */
  static async create(stream: MediaStream, opts: VideoStreamerOptions): Promise<VideoStreamer> {
    const mime = pickMime();
    if (!mime) throw new Error("no supported WebM MediaRecorder format");
    return new VideoStreamer(stream, mime, opts);
  }

  private constructor(stream: MediaStream, mime: string, opts: VideoStreamerOptions) {
    this.recorder = new MediaRecorder(stream, { mimeType: mime });
    // The recorder may refine the mime (e.g. add the real codec string); send the
    // exact value so the observer opens a matching MediaSource buffer.
    const actualMime = this.recorder.mimeType || mime;

    const proto = location.protocol === "https:" ? "wss" : "ws";
    const url =
      `${proto}://${location.host}/api/in/vision/stream` +
      `?scene=${encodeURIComponent(opts.scene)}&mime=${encodeURIComponent(actualMime)}`;
    this.ws = new WebSocket(url);
    this.ws.binaryType = "arraybuffer";
    this.ws.onopen = () => {
      for (const blob of this.backlog) void this.send(blob);
      this.backlog = [];
    };
    // Upload-only: the server never sends on this socket.
    this.ws.onclose = () => opts.onClose?.();

    this.recorder.ondataavailable = (ev) => {
      if (this.stopped || ev.data.size === 0) return;
      if (this.ws.readyState === WebSocket.OPEN) void this.send(ev.data);
      else if (this.ws.readyState === WebSocket.CONNECTING) this.backlog.push(ev.data);
      // closing/closed → drop
    };
    this.recorder.start(TIMESLICE_MS);
  }

  private async send(blob: Blob): Promise<void> {
    // Chunks must arrive in order, so resolve the ArrayBuffer before sending.
    // ondataavailable already fires in order and these awaits are near-instant.
    const buf = await blob.arrayBuffer();
    if (!this.stopped && this.ws.readyState === WebSocket.OPEN) this.ws.send(buf);
  }

  stop(): void {
    this.stopped = true;
    try {
      if (this.recorder.state !== "inactive") this.recorder.stop();
    } catch {
      /* ignore */
    }
    this.recorder.ondataavailable = null;
    try {
      this.ws.close();
    } catch {
      /* ignore */
    }
  }
}
