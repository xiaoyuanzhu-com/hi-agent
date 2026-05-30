// MicCapture — continuous mic tap that segments speech with VAD and streams
// live PCM while the user talks. This replaces the buffer-one-WAV-per-utterance
// model: the mic stays open, VAD marks speech boundaries, and each captured
// frame is resampled to 16 kHz mono 16-bit PCM and handed to `onChunk` in real
// time so the upstream can return rolling transcripts as the user speaks.
//
// A ScriptProcessorNode (deprecated but universally available; matches the old
// recorder) delivers raw Float32 frames. We keep a short pre-roll ring so the
// onset isn't clipped; on speech start the ring is flushed as the first chunks.

import { Vad, type VadOptions } from "./vad";
import { resample } from "./wav";

export interface MicCaptureOptions {
  vad?: VadOptions;
  preRollMs?: number;
  maxUtteranceMs?: number;
  /** Live PCM during a speech segment: 16 kHz mono 16-bit LE samples. */
  onChunk: (pcm16: Int16Array) => void;
  onSpeechStart?: () => void;
  onSpeechEnd?: () => void;
}

const FRAME = 2048;
const TARGET_SR = 16000;

export class MicCapture {
  private proc: ScriptProcessorNode;
  private sink: GainNode;
  private vad: Vad;
  private readonly sr: number;
  private readonly opts: Required<Omit<MicCaptureOptions, "vad">>;

  private activeSamples = 0;
  private preRoll: Float32Array[] = [];
  private preRollSamples = 0;
  private capturing = false;
  private stopped = false;
  private suspended = false;

  constructor(ctx: AudioContext, source: AudioNode, options: MicCaptureOptions) {
    this.sr = ctx.sampleRate;
    this.vad = new Vad(options.vad);
    this.opts = {
      preRollMs: options.preRollMs ?? 250,
      maxUtteranceMs: options.maxUtteranceMs ?? 30000,
      onChunk: options.onChunk,
      onSpeechStart: options.onSpeechStart ?? (() => {}),
      onSpeechEnd: options.onSpeechEnd ?? (() => {}),
    };

    this.proc = ctx.createScriptProcessor(FRAME, 1, 1);
    this.proc.onaudioprocess = (e) => this.onFrame(e.inputBuffer.getChannelData(0));
    source.connect(this.proc);
    // ScriptProcessor only pulls if connected to the graph; route through a
    // silent gain so nothing is audible.
    this.sink = ctx.createGain();
    this.sink.gain.value = 0;
    this.proc.connect(this.sink);
    this.sink.connect(ctx.destination);
  }

  private emit(frame: Float32Array): void {
    const pcm = this.sr === TARGET_SR ? frame : resample(frame, this.sr, TARGET_SR);
    const out = new Int16Array(pcm.length);
    for (let i = 0; i < pcm.length; i++) {
      const s = Math.max(-1, Math.min(1, pcm[i]!));
      out[i] = s < 0 ? s * 0x8000 : s * 0x7fff;
    }
    if (out.length > 0) this.opts.onChunk(out);
  }

  private onFrame(input: Float32Array): void {
    if (this.stopped || this.suspended) return;

    const frame = new Float32Array(input.length);
    frame.set(input); // engine reuses the input buffer; copy it

    let sum = 0;
    for (let i = 0; i < frame.length; i++) sum += frame[i]! * frame[i]!;
    const rms = Math.sqrt(sum / frame.length);
    const dtMs = (frame.length / this.sr) * 1000;

    if (this.capturing) {
      this.emit(frame);
      this.activeSamples += frame.length;
      const maxSamples = (this.opts.maxUtteranceMs / 1000) * this.sr;
      if (this.activeSamples >= maxSamples) {
        this.endUtterance();
        return;
      }
    } else {
      this.preRoll.push(frame);
      this.preRollSamples += frame.length;
      const maxPre = (this.opts.preRollMs / 1000) * this.sr;
      while (this.preRollSamples > maxPre && this.preRoll.length > 0) {
        this.preRollSamples -= this.preRoll.shift()!.length;
      }
    }

    const ev = this.vad.push(rms, dtMs);
    if (ev?.type === "start") {
      this.capturing = true;
      this.activeSamples = this.preRollSamples;
      this.opts.onSpeechStart();
      // Flush the pre-roll so the onset isn't clipped, then drop the ring.
      for (const f of this.preRoll) this.emit(f);
      this.preRoll = [];
      this.preRollSamples = 0;
    } else if (ev?.type === "end") {
      this.endUtterance();
    }
  }

  private endUtterance(): void {
    if (!this.capturing) return;
    this.capturing = false;
    this.activeSamples = 0;
    this.vad.reset();
    this.opts.onSpeechEnd();
  }

  /** Pause/resume capture without tearing down the graph (used during TTS). */
  setSuspended(on: boolean): void {
    this.suspended = on;
    if (on && this.capturing) this.endUtterance();
    if (on) this.vad.reset();
  }

  stop(): void {
    this.stopped = true;
    try {
      this.proc.disconnect();
    } catch {
      /* ignore */
    }
    try {
      this.sink.disconnect();
    } catch {
      /* ignore */
    }
    this.proc.onaudioprocess = null;
  }
}
