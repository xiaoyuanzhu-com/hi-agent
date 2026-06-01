import type { AudioBus } from "./audioBus";

/**
 * Plays the agent's voice through the AudioBus so the dot-matrix rides the real
 * voice while it speaks. One <audio> element + one MediaElementSource (created
 * once) feeds the shared analyser and the speakers.
 *
 * A turn's speech is one continuous stream from the backend, so this plays it
 * as one stream: `beginTurn` opens a fresh MediaSource, `pushChunk` appends the
 * bytes as they arrive, `endTurn` closes it. No per-clip queue, no `ended`-driven
 * stitching — the seams between sentences are gone because there are no clips.
 *
 * Each turn carries a generation token; `stop()` (barge-in) bumps the
 * generation and tears the turn down, so late chunks from a superseded turn are
 * ignored. Where MediaSource can't stream the codec (e.g. Safari/MSE quirks) we
 * fall back to buffering the turn and playing it as a single blob — still one
 * utterance per turn, just not incrementally.
 */
type TurnMode = "mse" | "blob" | "off";

export class VoicePlayer {
  private el: HTMLAudioElement;
  private muted = false;
  private playing = false;

  // Current turn state. `gen` is bumped on every beginTurn/stop so stale
  // callbacks (sourceopen, updateend) and late pushChunks can be discarded.
  private gen = 0;
  private mode: TurnMode = "off";
  private ms: MediaSource | null = null;
  private sb: SourceBuffer | null = null;
  private objectUrl: string | null = null;
  private appendQueue: Uint8Array[] = [];
  private inputEnded = false;
  private blobParts: Uint8Array[] = [];
  private blobMime = "";

  constructor(
    bus: AudioBus,
    private onStart: () => void,
    private onEnd: () => void,
  ) {
    this.el = new Audio();
    this.el.preload = "auto";
    const node = bus.ctx.createMediaElementSource(this.el);
    bus.attachPlayback(node); // → analyser + speakers
    this.el.addEventListener("ended", () => this.handleEnded());
    this.el.addEventListener("error", () => this.handleEnded());
  }

  /**
   * Open a new turn. Returns a generation token to pass back to `pushChunk`/
   * `endTurn`; chunks tagged with a superseded token are dropped.
   */
  beginTurn(mime: string): number {
    this.teardownTurn();
    const gen = ++this.gen;
    if (this.muted) {
      this.mode = "off";
      return gen;
    }

    const canStream =
      typeof MediaSource !== "undefined" &&
      typeof MediaSource.isTypeSupported === "function" &&
      MediaSource.isTypeSupported(mime);

    if (!canStream) {
      // Buffer the whole turn, play as one blob on endTurn.
      this.mode = "blob";
      this.blobParts = [];
      this.blobMime = mime;
      return gen;
    }

    this.mode = "mse";
    this.inputEnded = false;
    this.appendQueue = [];
    const ms = new MediaSource();
    this.ms = ms;
    this.objectUrl = URL.createObjectURL(ms);
    ms.addEventListener(
      "sourceopen",
      () => {
        if (gen !== this.gen || this.ms !== ms) return; // superseded before open
        try {
          const sb = ms.addSourceBuffer(mime);
          this.sb = sb;
          sb.addEventListener("updateend", () => this.pump(gen));
          this.pump(gen);
        } catch {
          // addSourceBuffer can still reject the type — degrade to silence for
          // this turn rather than throw; text still renders via /thought.
          this.mode = "off";
        }
      },
      { once: true },
    );
    this.el.src = this.objectUrl;
    return gen;
  }

  /** Append a chunk of the current turn's audio. */
  pushChunk(gen: number, bytes: Uint8Array): void {
    if (gen !== this.gen || this.muted) return;
    if (this.mode === "blob") {
      this.blobParts.push(bytes);
      return;
    }
    if (this.mode === "mse") {
      this.appendQueue.push(bytes);
      this.pump(gen);
    }
  }

  /** Signal the turn's audio is complete; flush and let it finish playing. */
  endTurn(gen: number): void {
    if (gen !== this.gen) return;
    if (this.mode === "blob") {
      if (this.muted || this.blobParts.length === 0) return;
      const blob = new Blob(this.blobParts as unknown as BlobPart[], { type: this.blobMime });
      this.blobParts = [];
      this.objectUrl = URL.createObjectURL(blob);
      this.el.src = this.objectUrl;
      this.startPlayback();
      return;
    }
    if (this.mode === "mse") {
      this.inputEnded = true;
      this.pump(gen);
    }
  }

  /** Toggle the voice output channel. Muting cuts any in-flight playback. */
  setMuted(on: boolean): void {
    this.muted = on;
    if (on) this.stop();
  }

  /** Stop immediately and drop the current turn (barge-in). */
  stop(): void {
    this.gen++; // invalidate the current turn so late chunks are ignored
    this.teardownTurn();
    if (this.playing) {
      this.playing = false;
      this.onEnd();
    }
  }

  /** Drain the append queue into the SourceBuffer one buffer at a time. */
  private pump(gen: number): void {
    if (gen !== this.gen || this.mode !== "mse") return;
    const sb = this.sb;
    if (!sb || sb.updating) return;
    const next = this.appendQueue.shift();
    if (next) {
      this.startPlayback();
      try {
        sb.appendBuffer(next as unknown as BufferSource);
      } catch {
        // Quota/parse error — drop the rest of this turn rather than wedge.
        this.appendQueue = [];
      }
      return;
    }
    if (this.inputEnded && this.ms && this.ms.readyState === "open") {
      try {
        this.ms.endOfStream();
      } catch {
        /* already ended/closed */
      }
    }
  }

  /** Begin playback (idempotent within a turn) and fire onStart once. */
  private startPlayback(): void {
    if (!this.playing) {
      this.playing = true;
      this.onStart();
    }
    void this.el.play().catch(() => {
      /* autoplay race; the next chunk/play retries */
    });
  }

  private handleEnded(): void {
    if (this.playing) {
      this.playing = false;
      this.onEnd();
    }
  }

  /** Drop all state for the current turn without firing onEnd. */
  private teardownTurn(): void {
    try {
      this.el.pause();
    } catch {
      /* ignore */
    }
    if (this.ms && this.ms.readyState === "open") {
      try {
        this.ms.endOfStream();
      } catch {
        /* ignore */
      }
    }
    this.ms = null;
    this.sb = null;
    this.appendQueue = [];
    this.blobParts = [];
    this.inputEnded = false;
    this.mode = "off";
    this.el.removeAttribute("src");
    try {
      this.el.load();
    } catch {
      /* ignore */
    }
    if (this.objectUrl) {
      URL.revokeObjectURL(this.objectUrl);
      this.objectUrl = null;
    }
  }
}
