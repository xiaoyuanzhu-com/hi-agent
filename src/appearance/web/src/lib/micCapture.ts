// MicCapture — continuous mic tap that segments speech with VAD and emits one
// WAV blob per finished utterance. This is what replaces push-to-talk: the mic
// stays open, the user just talks, and each utterance is posted to /audio.
//
// A ScriptProcessorNode (deprecated but universally available; matches the old
// recorder) delivers raw Float32 frames. We keep a short pre-roll ring so the
// onset isn't clipped, seed the active buffer with it on speech start, and on
// VAD end encode the buffer to a 16 kHz WAV.

import { Vad, type VadOptions } from "./vad";
import { floatToWavBlob } from "./wav";

export interface MicCaptureOptions {
  vad?: VadOptions;
  preRollMs?: number;
  maxUtteranceMs?: number;
  onUtterance: (wav: { blob: Blob; mime: string }) => void;
  onSpeechStart?: () => void;
  onSpeechEnd?: () => void;
}

const FRAME = 2048;

export class MicCapture {
  private proc: ScriptProcessorNode;
  private sink: GainNode;
  private vad: Vad;
  private readonly sr: number;
  private readonly opts: Required<Omit<MicCaptureOptions, "vad">>;

  private active: Float32Array[] = [];
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
      onUtterance: options.onUtterance,
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
      this.active.push(frame);
      this.activeSamples += frame.length;
      const maxSamples = (this.opts.maxUtteranceMs / 1000) * this.sr;
      if (this.activeSamples >= maxSamples) {
        this.endUtterance(false);
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
      this.active = this.preRoll.slice();
      this.activeSamples = this.preRollSamples;
      this.preRoll = [];
      this.preRollSamples = 0;
      this.opts.onSpeechStart();
    } else if (ev?.type === "end") {
      this.endUtterance(ev.droppedTooShort);
    }
  }

  private endUtterance(dropped: boolean): void {
    const frames = this.active;
    const total = this.activeSamples;
    this.capturing = false;
    this.active = [];
    this.activeSamples = 0;
    this.vad.reset();
    this.opts.onSpeechEnd();
    if (dropped || total === 0) return;
    const pcm = new Float32Array(total);
    let off = 0;
    for (const f of frames) {
      pcm.set(f, off);
      off += f.length;
    }
    this.opts.onUtterance(floatToWavBlob(pcm, this.sr));
  }

  /** Pause/resume capture without tearing down the graph (used during TTS). */
  setSuspended(on: boolean): void {
    this.suspended = on;
    if (on && this.capturing) this.endUtterance(true);
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
