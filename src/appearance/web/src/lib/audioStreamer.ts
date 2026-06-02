// AudioStreamer — continuous mic passthrough over a WebSocket.
//
// The mic stays open and every frame is shipped to the backend as raw 16 kHz
// mono 16-bit PCM. There is NO client-side voice-activity detection: the browser
// streams audio blindly and the upstream STT does all the endpointing. Results
// (partial + final) come back as small JSON text frames and are handed to the
// caller — partials drive barge-in (duck the speaker the instant real speech is
// recognized), finals mark a dispatched utterance.
//
// This replaces the old MicCapture, whose homegrown RMS VAD segmented utterances
// client-side. Moving segmentation to the upstream's ML VAD is both simpler and
// more reliable; the only thing we do here is resample + frame the audio.

import { resample } from "./wav";

export interface TranscriptEvent {
  text: string;
  isFinal: boolean;
}

export interface AudioStreamerOptions {
  /** Scene identity; rides in the WS query string (browsers can't set headers). */
  scene: string;
  onTranscript: (e: TranscriptEvent) => void;
  /** Fired when the socket closes (network drop or stop()). */
  onClose?: () => void;
}

const FRAME = 2048;
const TARGET_SR = 16000;
// 100 ms of 16 kHz mono 16-bit audio per WS frame (matches the upstream chunk).
const SEND_SAMPLES = 1600;

export class AudioStreamer {
  private proc: ScriptProcessorNode;
  private sink: GainNode;
  private ws: WebSocket;
  private readonly sr: number;

  // Resampled int16 samples awaiting a full SEND_SAMPLES frame.
  private pending: Int16Array[] = [];
  private pendingLen = 0;
  // Frames captured before the socket finished opening.
  private backlog: ArrayBuffer[] = [];
  private stopped = false;

  constructor(ctx: AudioContext, source: AudioNode, opts: AudioStreamerOptions) {
    this.sr = ctx.sampleRate;

    const proto = location.protocol === "https:" ? "wss" : "ws";
    const url = `${proto}://${location.host}/api/audio/in?scene=${encodeURIComponent(opts.scene)}`;
    this.ws = new WebSocket(url);
    this.ws.binaryType = "arraybuffer";
    this.ws.onopen = () => {
      for (const buf of this.backlog) this.ws.send(buf);
      this.backlog = [];
    };
    this.ws.onmessage = (ev) => {
      if (typeof ev.data !== "string") return;
      try {
        const m = JSON.parse(ev.data) as { text?: string; final?: boolean };
        opts.onTranscript({ text: m.text ?? "", isFinal: !!m.final });
      } catch {
        /* ignore malformed frame */
      }
    };
    this.ws.onclose = () => opts.onClose?.();

    this.proc = ctx.createScriptProcessor(FRAME, 1, 1);
    this.proc.onaudioprocess = (e) => this.onFrame(e.inputBuffer.getChannelData(0));
    source.connect(this.proc);
    // ScriptProcessor only pulls when connected to the graph; route through a
    // silent gain so nothing is audible.
    this.sink = ctx.createGain();
    this.sink.gain.value = 0;
    this.proc.connect(this.sink);
    this.sink.connect(ctx.destination);
  }

  private onFrame(input: Float32Array): void {
    if (this.stopped) return;

    const frame = new Float32Array(input.length);
    frame.set(input); // engine reuses the input buffer; copy it
    const res = this.sr === TARGET_SR ? frame : resample(frame, this.sr, TARGET_SR);

    const i16 = new Int16Array(res.length);
    for (let i = 0; i < res.length; i++) {
      const s = Math.max(-1, Math.min(1, res[i]!));
      i16[i] = s < 0 ? s * 0x8000 : s * 0x7fff;
    }
    this.pending.push(i16);
    this.pendingLen += i16.length;

    while (this.pendingLen >= SEND_SAMPLES) {
      const chunk = new Int16Array(SEND_SAMPLES);
      let filled = 0;
      while (filled < SEND_SAMPLES) {
        const head = this.pending[0]!;
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
      this.send(chunk.buffer);
    }
  }

  private send(buf: ArrayBuffer): void {
    if (this.ws.readyState === WebSocket.OPEN) this.ws.send(buf);
    else if (this.ws.readyState === WebSocket.CONNECTING) this.backlog.push(buf);
    // closing/closed → drop
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
    try {
      this.ws.close();
    } catch {
      /* ignore */
    }
  }
}
