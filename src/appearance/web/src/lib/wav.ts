// WAV encoding helpers, shared by the /audio channel and the live mic capture.
//
// Browsers capture Float32 PCM through the Web Audio graph; STT backends want a
// 16 kHz mono 16-bit WAV. These helpers do the resample + encode. Extracted from
// the old Composer recorder so both the one-shot path and the continuous VAD
// capture share one implementation.

export function concatFloat32(buffers: Float32Array[], total: number): Float32Array {
  const out = new Float32Array(total);
  let offset = 0;
  for (const buf of buffers) {
    out.set(buf, offset);
    offset += buf.length;
  }
  return out;
}

export function resample(pcm: Float32Array, fromRate: number, toRate: number): Float32Array {
  if (fromRate === toRate) return pcm;
  const ratio = fromRate / toRate;
  const newLen = Math.floor(pcm.length / ratio);
  const out = new Float32Array(newLen);
  for (let i = 0; i < newLen; i++) {
    const srcIdx = i * ratio;
    const lo = Math.floor(srcIdx);
    const hi = Math.min(lo + 1, pcm.length - 1);
    const frac = srcIdx - lo;
    out[i] = pcm[lo]! * (1 - frac) + pcm[hi]! * frac;
  }
  return out;
}

export function encodeWav(pcm: Float32Array, sampleRate: number): ArrayBuffer {
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
  view.setUint32(pos, 16, true); pos += 4; // fmt chunk size
  view.setUint16(pos, 1, true); pos += 2;  // PCM
  view.setUint16(pos, 1, true); pos += 2;  // mono
  view.setUint32(pos, sampleRate, true); pos += 4;
  view.setUint32(pos, sampleRate * 2, true); pos += 4; // byte rate (mono x 16-bit)
  view.setUint16(pos, 2, true); pos += 2;  // block align
  view.setUint16(pos, 16, true); pos += 2; // bits per sample
  writeStr("data");
  view.setUint32(pos, numSamples * 2, true); pos += 4;

  for (let i = 0; i < numSamples; i++) {
    const s = Math.max(-1, Math.min(1, pcm[i]!));
    view.setInt16(pos, s < 0 ? s * 0x8000 : s * 0x7fff, true);
    pos += 2;
  }
  return buffer;
}

const TARGET_SAMPLE_RATE = 16000;

/** Resample (if needed) to 16 kHz and encode a mono 16-bit WAV blob. */
export function floatToWavBlob(
  pcm: Float32Array,
  fromRate: number,
  toRate: number = TARGET_SAMPLE_RATE,
): { blob: Blob; mime: string } {
  const resampled = fromRate === toRate ? pcm : resample(pcm, fromRate, toRate);
  const wav = encodeWav(resampled, toRate);
  return { blob: new Blob([wav], { type: "audio/wav" }), mime: "audio/wav" };
}
