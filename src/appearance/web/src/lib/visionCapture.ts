// VisionCapture — periodic camera snapshots, the visual analog of the mic tap.
//
// Vision is a continuous input channel: we grab a frame every `intervalMs` and
// hand it to `onFrame` as an encoded image blob. There's no segmentation or
// commit (unlike audio's VAD) — sight just streams; the backend brain decides
// what to do with it. A frame is drawn from a hidden <video> bound to the
// camera stream onto an offscreen canvas, then encoded as JPEG.

export interface VisionCaptureOptions {
  /** How often to capture a frame. */
  intervalMs?: number;
  /** Longest edge of the captured frame in px; the other edge scales to keep
   *  aspect. Keeps payloads modest for a continuous channel. */
  maxEdge?: number;
  /** JPEG quality, 0..1. */
  quality?: number;
  onFrame: (blob: Blob, mime: string) => void;
}

const DEFAULT_INTERVAL_MS = 2500;
const DEFAULT_MAX_EDGE = 640;
const DEFAULT_QUALITY = 0.7;

export class VisionCapture {
  private readonly video: HTMLVideoElement;
  private readonly canvas: HTMLCanvasElement;
  private readonly opts: Required<Omit<VisionCaptureOptions, "onFrame">> &
    Pick<VisionCaptureOptions, "onFrame">;
  private timer: number | null = null;
  private busy = false;
  private stopped = false;

  constructor(stream: MediaStream, options: VisionCaptureOptions) {
    this.opts = {
      intervalMs: options.intervalMs ?? DEFAULT_INTERVAL_MS,
      maxEdge: options.maxEdge ?? DEFAULT_MAX_EDGE,
      quality: options.quality ?? DEFAULT_QUALITY,
      onFrame: options.onFrame,
    };

    this.video = document.createElement("video");
    this.video.muted = true;
    this.video.playsInline = true;
    this.video.srcObject = stream;
    this.canvas = document.createElement("canvas");

    void this.video.play().catch(() => {
      /* autoplay may defer until a gesture; the loop retries each tick */
    });

    this.timer = window.setInterval(() => void this.tick(), this.opts.intervalMs);
  }

  private async tick(): Promise<void> {
    if (this.stopped || this.busy) return;
    const w = this.video.videoWidth;
    const h = this.video.videoHeight;
    if (w === 0 || h === 0) return; // stream not ready yet
    this.busy = true;
    try {
      const scale = Math.min(1, this.opts.maxEdge / Math.max(w, h));
      this.canvas.width = Math.round(w * scale);
      this.canvas.height = Math.round(h * scale);
      const ctx = this.canvas.getContext("2d");
      if (!ctx) return;
      ctx.drawImage(this.video, 0, 0, this.canvas.width, this.canvas.height);
      const blob = await new Promise<Blob | null>((resolve) =>
        this.canvas.toBlob((b) => resolve(b), "image/jpeg", this.opts.quality),
      );
      if (blob && !this.stopped) this.opts.onFrame(blob, "image/jpeg");
    } finally {
      this.busy = false;
    }
  }

  stop(): void {
    this.stopped = true;
    if (this.timer !== null) {
      window.clearInterval(this.timer);
      this.timer = null;
    }
    this.video.srcObject = null;
  }
}
