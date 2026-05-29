// Voice-activity detection — a pure endpointing state machine.
//
// Fed one frame's RMS amplitude at a time, it decides when an utterance starts
// and ends. Hysteresis (start threshold > end threshold) avoids chattering on
// the boundary; an utterance ends after `endSilenceMs` of trailing silence.
// Kept free of any Web Audio dependency so it is unit-testable with synthetic
// amplitude sequences; `micCapture` wires it to a real mic.

export type VadEvent =
  | { type: "start" }
  | { type: "end"; voicedMs: number; droppedTooShort: boolean }
  | null;

export interface VadOptions {
  /** RMS (0..1) that triggers speech start. */
  startThreshold?: number;
  /** RMS below which a frame counts as silence. Should be < startThreshold. */
  endThreshold?: number;
  /** Trailing silence (ms) that ends an utterance. */
  endSilenceMs?: number;
  /** Utterances with less voiced time than this are flagged too-short. */
  minVoicedMs?: number;
}

export class Vad {
  private speaking = false;
  private silenceMs = 0;
  private voicedMs = 0;
  private readonly o: Required<VadOptions>;

  constructor(opts: VadOptions = {}) {
    this.o = {
      startThreshold: opts.startThreshold ?? 0.045,
      endThreshold: opts.endThreshold ?? 0.025,
      endSilenceMs: opts.endSilenceMs ?? 700,
      minVoicedMs: opts.minVoicedMs ?? 300,
    };
  }

  /** Feed one frame's RMS (0..1) and the ms elapsed since the previous frame. */
  push(rms: number, dtMs: number): VadEvent {
    if (!this.speaking) {
      if (rms >= this.o.startThreshold) {
        this.speaking = true;
        this.voicedMs = dtMs;
        this.silenceMs = 0;
        return { type: "start" };
      }
      return null;
    }

    if (rms >= this.o.endThreshold) {
      this.voicedMs += dtMs;
      this.silenceMs = 0;
    } else {
      this.silenceMs += dtMs;
    }

    if (this.silenceMs >= this.o.endSilenceMs) {
      const voiced = this.voicedMs;
      this.speaking = false;
      this.voicedMs = 0;
      this.silenceMs = 0;
      return {
        type: "end",
        voicedMs: voiced,
        droppedTooShort: voiced < this.o.minVoicedMs,
      };
    }
    return null;
  }

  get active(): boolean {
    return this.speaking;
  }

  reset(): void {
    this.speaking = false;
    this.silenceMs = 0;
    this.voicedMs = 0;
  }
}
