// AudioBus — one AudioContext + AnalyserNode that the Presence reads each frame.
//
// In Phase 1 the only source is the microphone, so the dot-matrix reflects the
// user's voice. In Phase 2 the playing TTS element is attached too, and the
// session switches which source feeds the analyser so the dots ride the agent's
// voice while it speaks. `read()` returns a level + log-spaced frequency bands,
// the same mapping the chosen `demos/dot-matrix.html` reference uses.

export interface Reading {
  /** RMS amplitude, 0..1. */
  level: number;
  /** Log-spaced frequency-band magnitudes, 0..1, length `bandCount`. */
  bands: Float32Array;
}

const NB = 56;
const MIN_HZ = 80;
const MAX_HZ = 6500;
const FFT = 1024;
const GAIN = 1.6;

type AudioCtor = typeof AudioContext;

export class AudioBus {
  readonly ctx: AudioContext;
  private analyser: AnalyserNode;
  // Explicit <ArrayBuffer> so the AnalyserNode getters accept these (TS 5.7+
  // made the typed arrays generic over their backing buffer).
  private freqBytes: Uint8Array<ArrayBuffer>;
  private timeBytes: Uint8Array<ArrayBuffer>;
  private bands = new Float32Array(NB);

  constructor() {
    const Ctor: AudioCtor =
      window.AudioContext ?? (window as unknown as { webkitAudioContext: AudioCtor }).webkitAudioContext;
    this.ctx = new Ctor();
    this.analyser = this.ctx.createAnalyser();
    this.analyser.fftSize = FFT;
    this.analyser.smoothingTimeConstant = 0.55;
    this.freqBytes = new Uint8Array(this.analyser.frequencyBinCount);
    this.timeBytes = new Uint8Array(this.analyser.fftSize);
  }

  async resume(): Promise<void> {
    if (this.ctx.state === "suspended") await this.ctx.resume();
  }

  /** Connect a mic source node into the analyser (never the speakers). */
  attachMic(node: AudioNode): void {
    node.connect(this.analyser);
  }

  /** Phase 2: connect a playback node into both the analyser and the speakers. */
  attachPlayback(node: AudioNode): void {
    node.connect(this.analyser);
    node.connect(this.ctx.destination);
  }

  get bandCount(): number {
    return NB;
  }

  /** Sample the analyser once. Visual smoothing is the caller's concern. */
  read(): Reading {
    this.analyser.getByteFrequencyData(this.freqBytes);
    this.analyser.getByteTimeDomainData(this.timeBytes);

    let sum = 0;
    for (let i = 0; i < this.timeBytes.length; i++) {
      const v = (this.timeBytes[i]! - 128) / 128;
      sum += v * v;
    }
    const level = Math.min(1, Math.sqrt(sum / this.timeBytes.length) * GAIN * 2.4);

    const binHz = this.ctx.sampleRate / FFT;
    for (let b = 0; b < NB; b++) {
      const fLo = MIN_HZ * Math.pow(MAX_HZ / MIN_HZ, b / NB);
      const fHi = MIN_HZ * Math.pow(MAX_HZ / MIN_HZ, (b + 1) / NB);
      const lo = Math.max(0, Math.floor(fLo / binHz));
      const hi = Math.max(lo, Math.min(this.freqBytes.length - 1, Math.ceil(fHi / binHz)));
      let m = 0;
      for (let k = lo; k <= hi; k++) m += this.freqBytes[k]!;
      let val = m / (hi - lo + 1) / 255;
      val *= 1 + (b / NB) * 1.4; // lift the consonant-range highs
      this.bands[b] = Math.min(1, val * GAIN);
    }
    return { level, bands: this.bands };
  }

  close(): void {
    try {
      this.analyser.disconnect();
    } catch {
      /* ignore */
    }
    void this.ctx.close();
  }
}
