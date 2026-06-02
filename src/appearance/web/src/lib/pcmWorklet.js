// PCM stream worklet — runs in AudioWorkletGlobalScope (the audio thread).
//
// This is the audio-thread half of `lib/audioStreamer`. It receives the mic's
// native-rate render quanta (Float32, 128 samples each), resamples them to
// 16 kHz mono, converts to 16-bit PCM, and frames the result into fixed
// SEND_SAMPLES chunks that are posted back to the main thread (over the node's
// MessagePort) for WebSocket shipping.
//
// It is intentionally a plain, dependency-free .js file: AudioWorklet modules
// can't use ES imports, so the linear resampling that used to come from
// `lib/wav` is inlined here. Unlike that per-call helper, the phase is carried
// continuously across quanta, so framing the input into 128-sample chunks
// introduces no resampling drift.

const TARGET_SR = 16000;
// 100 ms of 16 kHz mono 16-bit audio per posted frame (matches the upstream chunk).
const SEND_SAMPLES = 1600;

class PcmStreamProcessor extends AudioWorkletProcessor {
  constructor() {
    super();
    // `sampleRate` is the context's native rate, a global in this scope.
    this.ratio = sampleRate / TARGET_SR; // source samples consumed per output sample
    this.phase = 0; // source position of the next output, relative to the current buffer
    this.last = 0; // final sample of the previous buffer, for interpolation across the seam

    // Resampled int16 awaiting a full SEND_SAMPLES frame.
    this.pending = [];
    this.pendingLen = 0;
  }

  process(inputs) {
    const buf = inputs[0] && inputs[0][0];
    // No connected input this quantum (e.g. a momentary gap) — stay alive.
    if (buf && buf.length) this.ingest(buf);
    return true;
  }

  ingest(buf) {
    const n = buf.length;
    // Sample with the previous buffer's tail standing in for index -1, so the
    // first output of each buffer can interpolate across the seam.
    const at = (i) => (i < 0 ? this.last : i >= n ? buf[n - 1] : buf[i]);

    // Emit while the source position stays inside this buffer.
    const out = [];
    let pos = this.phase;
    while (pos <= n - 1) {
      const lo = Math.floor(pos);
      const frac = pos - lo;
      out.push(at(lo) * (1 - frac) + at(lo + 1) * frac);
      pos += this.ratio;
    }
    this.phase = pos - n; // carry the leftover (possibly negative) into the next buffer
    this.last = buf[n - 1];

    if (out.length) this.frame(out);
  }

  frame(samples) {
    const i16 = new Int16Array(samples.length);
    for (let i = 0; i < samples.length; i++) {
      const s = Math.max(-1, Math.min(1, samples[i]));
      i16[i] = s < 0 ? s * 0x8000 : s * 0x7fff;
    }
    this.pending.push(i16);
    this.pendingLen += i16.length;

    while (this.pendingLen >= SEND_SAMPLES) {
      const chunk = new Int16Array(SEND_SAMPLES);
      let filled = 0;
      while (filled < SEND_SAMPLES) {
        const head = this.pending[0];
        const need = SEND_SAMPLES - filled;
        if (head.length <= need) {
          chunk.set(head, filled);
          filled += head.length;
          this.pending.shift();
        } else {
          chunk.set(head.subarray(0, need), filled);
          this.pending[0] = head.subarray(need);
          filled += need;
        }
      }
      this.pendingLen -= SEND_SAMPLES;
      // Transfer the buffer so it crosses to the main thread without a copy.
      this.port.postMessage(chunk.buffer, [chunk.buffer]);
    }
  }
}

registerProcessor("pcm-stream", PcmStreamProcessor);
