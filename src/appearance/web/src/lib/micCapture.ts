// MicCapture — continuous mic tap that segments speech with VAD and emits one
// WAV per utterance. The mic stays open; VAD marks speech boundaries; the
// captured frames (plus a short pre-roll so the onset isn't clipped) are
// accumulated and, on speech end, resampled to 16 kHz mono 16-bit and encoded
// as a WAV blob handed to `onSpeechEnd`. The caller POSTs that to /api/audio.
//
// Segmentation here is purely transport chunking for a request/response
// endpoint — it is NOT a turn decision. The backend brain decides when to
// think; the client just streams the signal one utterance at a time.
//
// A ScriptProcessorNode (deprecated but universally available) delivers raw
// Float32 frames. We keep a short pre-roll ring so the onset isn't clipped; on
// speech start the ring is folded into the utterance buffer.

import { Vad, type VadOptions } from "./vad";
import { concatFloat32, floatToWavBlob } from "./wav";

export interface CapturedAudio {
  blob: Blob;
  mime: string;
}

export interface MicCaptureOptions {
  vad?: VadOptions;
  preRollMs?: number;
  maxUtteranceMs?: number;
  /** Fired when VAD detects speech onset (used by the caller for barge-in). */
  onSpeechStart?: () => void;
  /** Fired at end of utterance with the encoded 16 kHz mono WAV. */
  onSpeechEnd?: (audio: CapturedAudio) => void;
}

const FRAME = 2048;
const TARGET_SR = 16000;

export class MicCapture {
  private proc: ScriptProcessorNode;
  private sink: GainNode;
  private vad: Vad;
  private readonly sr: number;
  private readonly opts: Required<Omit<MicCaptureOptions, "vad">>;

  private capture: Float32Array[] = [];
  private captureSamples = 0;
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

  private onFrame(input: Float32Array): void {
    if (this.stopped || this.suspended) return;

    const frame = new Float32Array(input.length);
    frame.set(input); // engine reuses the input buffer; copy it

    let sum = 0;
    for (let i = 0; i < frame.length; i++) sum += frame[i]! * frame[i]!;
    const rms = Math.sqrt(sum / frame.length);
    const dtMs = (frame.length / this.sr) * 1000;

    if (this.capturing) {
      this.capture.push(frame);
      this.captureSamples += frame.length;
      const maxSamples = (this.opts.maxUtteranceMs / 1000) * this.sr;
      if (this.captureSamples >= maxSamples) {
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
      // Seed the utterance with the pre-roll so the onset isn't clipped.
      this.capture = this.preRoll;
      this.captureSamples = this.preRollSamples;
      this.preRoll = [];
      this.preRollSamples = 0;
      this.opts.onSpeechStart();
    } else if (ev?.type === "end") {
      this.endUtterance();
    }
  }

  private endUtterance(): void {
    if (!this.capturing) return;
    this.capturing = false;
    const frames = this.capture;
    const total = this.captureSamples;
    this.capture = [];
    this.captureSamples = 0;
    this.vad.reset();
    if (total > 0) {
      const pcm = concatFloat32(frames, total);
      const { blob, mime } = floatToWavBlob(pcm, this.sr, TARGET_SR);
      this.opts.onSpeechEnd({ blob, mime });
    }
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
