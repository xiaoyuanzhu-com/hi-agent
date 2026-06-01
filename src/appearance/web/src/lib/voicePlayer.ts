import type { AudioBus } from "./audioBus";

/**
 * Plays the agent's TTS clips in order through the AudioBus, so the dot-matrix
 * rides the agent's real voice while it speaks. One <audio> element + one
 * MediaElementSource (created once) feeds the shared analyser and the speakers;
 * clips are queued and advanced on `ended`.
 */
export class VoicePlayer {
  private el: HTMLAudioElement;
  private queue: string[] = [];
  private current: string | null = null;
  private playing = false;
  private muted = false;

  constructor(
    bus: AudioBus,
    private onStart: () => void,
    private onEnd: () => void,
  ) {
    this.el = new Audio();
    this.el.preload = "auto";
    const node = bus.ctx.createMediaElementSource(this.el);
    bus.attachPlayback(node); // → analyser + speakers
    this.el.addEventListener("ended", () => this.advance());
    this.el.addEventListener("error", () => this.advance());
  }

  enqueue(blob: Blob): void {
    // Output channel muted: discard the clip rather than buffer silenced voice.
    // The agent's words still render as text via /thought.
    if (this.muted) return;
    this.queue.push(URL.createObjectURL(blob));
    if (!this.playing) this.advance();
  }

  /** Toggle the voice output channel. Muting cuts any in-flight playback. */
  setMuted(on: boolean): void {
    this.muted = on;
    if (on) this.stop();
  }

  private advance(): void {
    if (this.current) {
      URL.revokeObjectURL(this.current);
      this.current = null;
    }
    const next = this.queue.shift();
    if (!next) {
      if (this.playing) {
        this.playing = false;
        this.onEnd();
      }
      return;
    }
    const wasPlaying = this.playing;
    this.playing = true;
    if (!wasPlaying) this.onStart();
    this.current = next;
    this.el.src = next;
    void this.el.play().catch(() => this.advance());
  }

  /** Stop immediately and drop the queue (barge-in). */
  stop(): void {
    this.queue.forEach((u) => URL.revokeObjectURL(u));
    this.queue = [];
    if (this.current) {
      URL.revokeObjectURL(this.current);
      this.current = null;
    }
    try {
      this.el.pause();
    } catch {
      /* ignore */
    }
    this.el.removeAttribute("src");
    if (this.playing) {
      this.playing = false;
      this.onEnd();
    }
  }
}
