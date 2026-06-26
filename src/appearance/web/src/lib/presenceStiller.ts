// PresenceStiller — the cheap, always-on presence lane beside the full video upload.
//
// Every couple of seconds it grabs ONE downscaled JPEG still from the live camera
// and POSTs it to /api/in/vision/presence, where the backend runs local face
// recognition and raises a real-time "who's here" signal only when presence
// changes. This is NOT the capture stream: `VideoStreamer` uploads the camera at
// full fidelity, continuously, and remains the archive. Keeping the two lanes
// separate is deliberate — the low-res presence sampling must never cap what the
// agent can see at full quality, it only feeds a fast recognition reflex.
//
// The stills are throwaway (the backend never persists them), so this is
// best-effort throughout: a dropped frame or a failed POST is harmless.

import { postPresenceStill } from "../channels/in/vision";

// How often we sample a still. Slow enough to stay cheap (local face inference per
// frame), fast enough that "someone just walked in" lands within a few seconds.
const STILL_EVERY_MS = 2500;

// Longest side of the downscaled still. Big enough for reliable face detection,
// small enough to keep the POST and the backend decode cheap.
const MAX_EDGE = 640;

// JPEG quality for the still — recognition is robust to mild compression.
const JPEG_QUALITY = 0.7;

export interface PresenceStillerOptions {
  /** Scene identity, sent as X-HI-Scene on each still. */
  scene: string;
}

export class PresenceStiller {
  private readonly scene: string;
  private readonly video: HTMLVideoElement;
  private readonly canvas: HTMLCanvasElement;
  private timer: ReturnType<typeof setInterval> | null = null;
  private stopped = false;
  private inFlight = false;

  /**
   * Begin sampling `stream`. A detached <video> decodes the same MediaStream the
   * recorder uses (a stream can feed several sinks) so we can draw frames to a
   * canvas; it is never added to the DOM — the visible self-view is
   * `CameraPreview`'s own element.
   */
  constructor(stream: MediaStream, opts: PresenceStillerOptions) {
    this.scene = opts.scene;
    this.video = document.createElement("video");
    this.video.muted = true;
    this.video.playsInline = true;
    this.video.srcObject = stream;
    void this.video.play().catch(() => {
      /* muted playback should autoplay; if blocked, videoWidth stays 0 and we skip */
    });
    this.canvas = document.createElement("canvas");
    this.timer = setInterval(() => void this.tick(), STILL_EVERY_MS);
  }

  private async tick(): Promise<void> {
    // Skip if torn down, or if the previous still is still uploading — never queue
    // stills, just sample the next interval afresh.
    if (this.stopped || this.inFlight) return;

    const vw = this.video.videoWidth;
    const vh = this.video.videoHeight;
    if (vw === 0 || vh === 0) return; // not decoding a frame yet

    const scale = Math.min(1, MAX_EDGE / Math.max(vw, vh));
    const w = Math.max(1, Math.round(vw * scale));
    const h = Math.max(1, Math.round(vh * scale));
    this.canvas.width = w;
    this.canvas.height = h;
    const ctx = this.canvas.getContext("2d");
    if (!ctx) return;
    ctx.drawImage(this.video, 0, 0, w, h);

    const blob = await new Promise<Blob | null>((resolve) =>
      this.canvas.toBlob((b) => resolve(b), "image/jpeg", JPEG_QUALITY),
    );
    if (!blob || this.stopped) return;

    this.inFlight = true;
    try {
      await postPresenceStill({ scene: this.scene, blob });
    } catch {
      /* best-effort reflex feed — a dropped still is harmless */
    } finally {
      this.inFlight = false;
    }
  }

  stop(): void {
    this.stopped = true;
    if (this.timer !== null) {
      clearInterval(this.timer);
      this.timer = null;
    }
    this.video.srcObject = null;
  }
}
