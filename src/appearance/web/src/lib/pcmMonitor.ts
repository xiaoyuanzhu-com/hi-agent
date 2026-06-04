// PcmPlayer — plays a stream of raw little-endian 16-bit PCM through Web Audio.
//
// The inbound mic rides `GET /api/in/audio` as raw 16 kHz mono s16le PCM, which
// `<audio>` can't decode, so the inspector schedules it itself: each byte chunk
// is converted to Float32 samples, wrapped in an `AudioBuffer`, and queued
// back-to-back on an `AudioContext` so the frames play gaplessly. The buffers
// carry the source sample rate, so the source node resamples to the context's
// own rate — no need to force the context's rate (Safari rejects that anyway).
//
// Must be created from a user gesture (the inspector's monitor button) so the
// AudioContext is allowed to start.

type AudioCtor = typeof AudioContext;

export class PcmPlayer {
  private ctx: AudioContext;
  // Playback cursor: the context time the next buffer should start at, kept just
  // ahead of `currentTime` so consecutive buffers abut with no gap or overlap.
  private nextTime = 0;
  // A trailing odd byte carried to the next chunk (a sample is two bytes and a
  // chunk boundary can split one).
  private leftover: number | null = null;
  private sources = new Set<AudioBufferSourceNode>();
  private stopped = false;

  constructor(private readonly sampleRate = 16000) {
    const Ctor: AudioCtor =
      window.AudioContext ?? (window as unknown as { webkitAudioContext: AudioCtor }).webkitAudioContext;
    this.ctx = new Ctor();
  }

  /** Append a chunk of raw s16le PCM and schedule it for playback. */
  push(chunk: Uint8Array): void {
    if (this.stopped || chunk.length === 0) return;

    // Stitch any carried byte onto the front of this chunk.
    let bytes: Uint8Array;
    if (this.leftover !== null) {
      bytes = new Uint8Array(chunk.length + 1);
      bytes[0] = this.leftover;
      bytes.set(chunk, 1);
      this.leftover = null;
    } else {
      bytes = chunk;
    }

    const sampleCount = bytes.length >> 1; // floor to whole samples
    if (bytes.length & 1) this.leftover = bytes[bytes.length - 1]!;
    if (sampleCount === 0) return;

    const view = new DataView(bytes.buffer, bytes.byteOffset, sampleCount * 2);
    const samples = new Float32Array(sampleCount);
    for (let i = 0; i < sampleCount; i++) {
      samples[i] = view.getInt16(i * 2, true) / 0x8000;
    }

    const buffer = this.ctx.createBuffer(1, sampleCount, this.sampleRate);
    buffer.copyToChannel(samples, 0);

    const src = this.ctx.createBufferSource();
    src.buffer = buffer;
    src.connect(this.ctx.destination);

    // Keep a little lead so a late chunk doesn't try to start in the past; if we
    // ever fall behind (underrun), resync to "now" + lead.
    const lead = 0.08;
    const now = this.ctx.currentTime;
    if (this.nextTime < now + 0.01) this.nextTime = now + lead;
    src.start(this.nextTime);
    this.nextTime += buffer.duration;

    this.sources.add(src);
    src.onended = () => this.sources.delete(src);
  }

  /** Stop playback immediately and release the context. */
  stop(): void {
    if (this.stopped) return;
    this.stopped = true;
    for (const src of this.sources) {
      try {
        src.stop();
      } catch {
        /* already stopped */
      }
    }
    this.sources.clear();
    void this.ctx.close();
  }
}
