// Client for the human-interface /audio channel.
//
// Browser MediaRecorder formats (webm/opus on Chrome, mp4/aac on Safari) are
// inconsistent and not all accepted by every STT backend. We sidestep that by
// capturing raw PCM through the Web Audio graph and encoding a 16 kHz mono
// 16-bit WAV at stop time — a format every STT provider takes.

export interface AudioRecorder {
  /** Stop capture and return the encoded WAV blob plus its mime. */
  stop(): Promise<{ blob: Blob; mime: string; durationMs: number }>;
  /** Tear down without producing a blob (mic permission released). */
  cancel(): void;
}

const TARGET_SAMPLE_RATE = 16000;

export async function startRecording(): Promise<AudioRecorder> {
  const stream = await navigator.mediaDevices.getUserMedia({
    audio: {
      channelCount: 1,
      echoCancellation: true,
      noiseSuppression: true,
    },
  });

  type WindowWithLegacyAudio = typeof window & {
    webkitAudioContext?: typeof AudioContext;
  };
  const w = window as WindowWithLegacyAudio;
  const Ctx: typeof AudioContext = w.AudioContext ?? w.webkitAudioContext!;
  // Some browsers ignore the requested sampleRate; we resample at encode time.
  const ctx = new Ctx({ sampleRate: TARGET_SAMPLE_RATE });
  const source = ctx.createMediaStreamSource(stream);
  const buffers: Float32Array[] = [];
  let totalLen = 0;
  const startedAt = performance.now();

  // ScriptProcessorNode is deprecated but ships in every browser and avoids
  // the AudioWorklet module-loading dance for v0. 4096 sample buffer keeps
  // overhead reasonable.
  const proc = ctx.createScriptProcessor(4096, 1, 1);
  proc.onaudioprocess = (e) => {
    const input = e.inputBuffer.getChannelData(0);
    const chunk = new Float32Array(input.length);
    chunk.set(input);
    buffers.push(chunk);
    totalLen += chunk.length;
  };
  source.connect(proc);
  // Connect to destination so the graph actually pulls; we don't want playback,
  // so route through a near-silent gain node.
  const sink = ctx.createGain();
  sink.gain.value = 0;
  proc.connect(sink);
  sink.connect(ctx.destination);

  let teardown = false;
  const cleanup = () => {
    if (teardown) return;
    teardown = true;
    try {
      proc.disconnect();
    } catch { /* ignore */ }
    try {
      source.disconnect();
    } catch { /* ignore */ }
    try {
      sink.disconnect();
    } catch { /* ignore */ }
    stream.getTracks().forEach((t) => t.stop());
    void ctx.close();
  };

  return {
    stop: async () => {
      if (teardown) throw new Error("recorder already stopped");
      cleanup();
      const pcm = concatFloat32(buffers, totalLen);
      const resampled = ctx.sampleRate === TARGET_SAMPLE_RATE
        ? pcm
        : resample(pcm, ctx.sampleRate, TARGET_SAMPLE_RATE);
      const wav = encodeWav(resampled, TARGET_SAMPLE_RATE);
      return {
        blob: new Blob([wav], { type: "audio/wav" }),
        mime: "audio/wav",
        durationMs: performance.now() - startedAt,
      };
    },
    cancel: cleanup,
  };
}

export async function postAudio(opts: {
  from: string;
  blob: Blob;
  mime: string;
  signal?: AbortSignal;
}): Promise<{ transcript: string; media_path: string }> {
  const res = await fetch("/audio", {
    method: "POST",
    headers: {
      "Content-Type": opts.mime,
      "X-HI-From": opts.from,
    },
    body: opts.blob,
    signal: opts.signal,
  });
  if (!res.ok) {
    const detail = await res.text().catch(() => "");
    throw new Error(
      `/audio POST failed: ${res.status} ${res.statusText}${detail ? ` — ${detail.trim()}` : ""}`,
    );
  }
  return (await res.json()) as { transcript: string; media_path: string };
}

function concatFloat32(buffers: Float32Array[], total: number): Float32Array {
  const out = new Float32Array(total);
  let offset = 0;
  for (const buf of buffers) {
    out.set(buf, offset);
    offset += buf.length;
  }
  return out;
}

function resample(pcm: Float32Array, fromRate: number, toRate: number): Float32Array {
  if (fromRate === toRate) return pcm;
  const ratio = fromRate / toRate;
  const newLen = Math.floor(pcm.length / ratio);
  const out = new Float32Array(newLen);
  for (let i = 0; i < newLen; i++) {
    const srcIdx = i * ratio;
    const lo = Math.floor(srcIdx);
    const hi = Math.min(lo + 1, pcm.length - 1);
    const frac = srcIdx - lo;
    out[i] = pcm[lo] * (1 - frac) + pcm[hi] * frac;
  }
  return out;
}

function encodeWav(pcm: Float32Array, sampleRate: number): ArrayBuffer {
  const numSamples = pcm.length;
  const buffer = new ArrayBuffer(44 + numSamples * 2);
  const view = new DataView(buffer);

  let pos = 0;
  const writeStr = (s: string) => {
    for (let i = 0; i < s.length; i++) view.setUint8(pos++, s.charCodeAt(i));
  };

  writeStr("RIFF");
  view.setUint32(pos, 36 + numSamples * 2, true); pos += 4;
  writeStr("WAVE");
  writeStr("fmt ");
  view.setUint32(pos, 16, true); pos += 4;       // fmt chunk size
  view.setUint16(pos, 1, true); pos += 2;        // PCM
  view.setUint16(pos, 1, true); pos += 2;        // mono
  view.setUint32(pos, sampleRate, true); pos += 4;
  view.setUint32(pos, sampleRate * 2, true); pos += 4; // byte rate (mono × 16-bit)
  view.setUint16(pos, 2, true); pos += 2;        // block align
  view.setUint16(pos, 16, true); pos += 2;       // bits per sample
  writeStr("data");
  view.setUint32(pos, numSamples * 2, true); pos += 4;

  for (let i = 0; i < numSamples; i++) {
    const s = Math.max(-1, Math.min(1, pcm[i]));
    view.setInt16(pos, s < 0 ? s * 0x8000 : s * 0x7fff, true);
    pos += 2;
  }
  return buffer;
}
