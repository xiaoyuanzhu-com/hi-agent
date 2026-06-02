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
//
// The resampling/framing runs on the audio thread in `pcmWorklet.js` (an
// AudioWorklet processor); this class just wires up the node, ships the finished
// frames it posts back over the WebSocket, and reads transcripts off the socket.

// Vite resolves this to the (hashed, statically served) URL of the worklet
// module, which `addModule` fetches and evaluates in the audio thread.
import workletUrl from "./pcmWorklet.js?url";

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

// Tracks which contexts already have the worklet module so we never call
// addModule twice for the same one (a redundant network round-trip).
const loaded = new WeakSet<BaseAudioContext>();

async function ensureWorklet(ctx: BaseAudioContext): Promise<void> {
  if (loaded.has(ctx)) return;
  await ctx.audioWorklet.addModule(workletUrl);
  loaded.add(ctx);
}

export class AudioStreamer {
  private node: AudioWorkletNode;
  private sink: GainNode;
  private ws: WebSocket;

  // Frames captured before the socket finished opening.
  private backlog: ArrayBuffer[] = [];
  private stopped = false;

  /**
   * Start streaming `source`'s audio to the backend. Async because the worklet
   * module is loaded lazily (`addModule` returns a promise).
   */
  static async create(
    ctx: AudioContext,
    source: AudioNode,
    opts: AudioStreamerOptions,
  ): Promise<AudioStreamer> {
    await ensureWorklet(ctx);
    return new AudioStreamer(ctx, source, opts);
  }

  private constructor(ctx: AudioContext, source: AudioNode, opts: AudioStreamerOptions) {
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

    this.node = new AudioWorkletNode(ctx, "pcm-stream", {
      numberOfInputs: 1,
      numberOfOutputs: 1,
      channelCount: 1,
    });
    // The worklet posts back ready-to-send SEND_SAMPLES PCM frames (transferred).
    this.node.port.onmessage = (ev) => {
      if (!this.stopped) this.send(ev.data as ArrayBuffer);
    };
    source.connect(this.node);
    // The node only renders (and thus pulls input) while it's wired to the
    // destination; route its silent output through a zeroed gain so nothing is
    // audible.
    this.sink = ctx.createGain();
    this.sink.gain.value = 0;
    this.node.connect(this.sink);
    this.sink.connect(ctx.destination);
  }

  private send(buf: ArrayBuffer): void {
    if (this.ws.readyState === WebSocket.OPEN) this.ws.send(buf);
    else if (this.ws.readyState === WebSocket.CONNECTING) this.backlog.push(buf);
    // closing/closed → drop
  }

  stop(): void {
    this.stopped = true;
    this.node.port.onmessage = null;
    try {
      this.node.disconnect();
    } catch {
      /* ignore */
    }
    try {
      this.sink.disconnect();
    } catch {
      /* ignore */
    }
    try {
      this.ws.close();
    } catch {
      /* ignore */
    }
  }
}
