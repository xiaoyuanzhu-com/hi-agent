//! macOS microphone capture vendor (cpal) — the OS-side half of
//! [`crate::capabilities::audio_capture`].
//!
//! Opens the default input device and yields raw **16 kHz mono signed 16-bit
//! little-endian PCM** — the exact format the audio pipeline expects, so the frames
//! feed [`crate::server::audio::ingest_pcm_stream`] the same as the browser mic. The
//! device runs at its own rate (commonly 44.1/48 kHz, often stereo), so each buffer
//! is downmixed to mono and linearly resampled to 16 kHz before it goes out.
//!
//! cpal's `Stream` is `!Send` and must live where it was built, so it stays on a
//! dedicated capture thread that parks until the [`Capture`] handle is dropped; that
//! drop stops the mic and ends the frame channel, letting a downstream ingest
//! finalize. Capturing needs the **Microphone** TCC grant; without it stream setup
//! errors (surfaced from [`start`]) or simply yields silence.

use anyhow::Context;
use bytes::Bytes;
use cpal::traits::{DeviceTrait, HostTrait, StreamTrait};
use cpal::{FromSample, Sample, SampleFormat, SizedSample};
use tokio::sync::mpsc;

/// The agent-facing sample rate every downstream stage assumes.
const TARGET_RATE: f64 = 16_000.0;
/// Flush PCM in ~100 ms chunks (1600 samples × 2 bytes), matching the browser mic's
/// frame cadence so the STT upstream sees a familiar shape.
const CHUNK_BYTES: usize = 1600 * 2;

/// A live capture. Dropping it drops `_stop`, which unblocks the capture thread so it
/// drops the cpal stream — stopping the mic and closing the frame channel.
pub struct Capture {
    _stop: std::sync::mpsc::Sender<()>,
}

/// Open the default input device and stream 16 kHz mono PCM. Returns the [`Capture`]
/// (drop to stop) and the receiver of PCM frames. Errs if there's no input device or
/// the stream can't be built/started (e.g. Microphone not granted).
pub fn start() -> anyhow::Result<(Capture, mpsc::Receiver<Bytes>)> {
    // Bounded so a stalled ingest exerts backpressure (frames are dropped at the
    // realtime callback rather than buffered unboundedly — losing a little live audio
    // beats growing memory under a stall).
    let (frame_tx, frame_rx) = mpsc::channel::<Bytes>(64);
    // Stream-build result, reported back so `start` can fail synchronously.
    let (ready_tx, ready_rx) = std::sync::mpsc::channel::<Result<(), String>>();
    // Dropped with `Capture` to tear the stream down.
    let (stop_tx, stop_rx) = std::sync::mpsc::channel::<()>();

    std::thread::Builder::new()
        .name("mic-capture".to_string())
        .spawn(move || {
            let stream = match build_stream(frame_tx) {
                Ok(s) => s,
                Err(e) => {
                    let _ = ready_tx.send(Err(e.to_string()));
                    return;
                }
            };
            if let Err(e) = stream.play() {
                let _ = ready_tx.send(Err(format!("play stream: {e}")));
                return;
            }
            let _ = ready_tx.send(Ok(()));
            // Keep `stream` alive until the Capture handle is dropped (recv then errs).
            let _ = stop_rx.recv();
            // `stream` drops here → the mic stops and `frame_tx` (held by the
            // callback) drops, closing the frame channel.
        })
        .context("spawning mic-capture thread")?;

    match ready_rx.recv() {
        Ok(Ok(())) => Ok((Capture { _stop: stop_tx }, frame_rx)),
        Ok(Err(e)) => anyhow::bail!("mic capture: {e}"),
        Err(_) => anyhow::bail!("mic-capture thread exited during setup"),
    }
}

/// Build the input stream on the current (capture) thread, wiring its data callback
/// to a [`Resampler`] that downmixes + resamples to 16 kHz mono PCM and ships chunks.
fn build_stream(frame_tx: mpsc::Sender<Bytes>) -> anyhow::Result<cpal::Stream> {
    let host = cpal::default_host();
    let device = host.default_input_device().context("no default input device")?;
    let supported = device.default_input_config().context("no default input config")?;
    let sample_format = supported.sample_format();
    let config: cpal::StreamConfig = supported.into();
    let in_rate = config.sample_rate.0 as f64;
    let channels = config.channels as usize;
    let mut rs = Resampler::new(in_rate, channels);
    let err_fn = |e| tracing::warn!(error = %e, "mic capture stream error");

    let stream = match sample_format {
        SampleFormat::F32 => device.build_input_stream(
            &config,
            move |data: &[f32], _: &_| rs.push(data, &frame_tx),
            err_fn,
            None,
        )?,
        SampleFormat::I16 => device.build_input_stream(
            &config,
            move |data: &[i16], _: &_| rs.push(data, &frame_tx),
            err_fn,
            None,
        )?,
        SampleFormat::U16 => device.build_input_stream(
            &config,
            move |data: &[u16], _: &_| rs.push(data, &frame_tx),
            err_fn,
            None,
        )?,
        other => anyhow::bail!("unsupported input sample format: {other:?}"),
    };
    Ok(stream)
}

/// Stateful downmix + linear resampler from the device's rate/channels to 16 kHz
/// mono i16. State persists across callbacks (the fractional read position and the
/// unconsumed input tail), so resampling is continuous over buffer boundaries.
struct Resampler {
    step: f64,
    channels: usize,
    /// Unconsumed mono input samples (carried between callbacks for interpolation).
    in_buf: Vec<f32>,
    /// Fractional read position within `in_buf`.
    pos: f64,
    /// Accumulated output PCM (i16 LE), flushed in [`CHUNK_BYTES`] runs.
    out_buf: Vec<u8>,
}

impl Resampler {
    fn new(in_rate: f64, channels: usize) -> Self {
        Self {
            step: in_rate / TARGET_RATE,
            channels: channels.max(1),
            in_buf: Vec::new(),
            pos: 0.0,
            out_buf: Vec::new(),
        }
    }

    /// Consume one device buffer: downmix to mono, resample to 16 kHz, and ship any
    /// completed ~100 ms chunks. A full channel just drops the chunk (best-effort).
    fn push<T>(&mut self, data: &[T], tx: &mpsc::Sender<Bytes>)
    where
        T: SizedSample,
        f32: FromSample<T>,
    {
        if self.channels <= 1 {
            self.in_buf.extend(data.iter().map(|&s| f32::from_sample(s)));
        } else {
            for frame in data.chunks_exact(self.channels) {
                let sum: f32 = frame.iter().map(|&s| f32::from_sample(s)).sum();
                self.in_buf.push(sum / self.channels as f32);
            }
        }

        // Linear interpolation at `pos`, advancing by `step`, while a full pair
        // [i, i+1] is available.
        while (self.pos as usize) + 1 < self.in_buf.len() {
            let i = self.pos as usize;
            let frac = (self.pos - i as f64) as f32;
            let v = self.in_buf[i] + (self.in_buf[i + 1] - self.in_buf[i]) * frac;
            let s = (v.clamp(-1.0, 1.0) * 32767.0) as i16;
            self.out_buf.extend_from_slice(&s.to_le_bytes());
            self.pos += self.step;
        }

        // Drop consumed input, keeping the fractional remainder aligned.
        let consumed = self.pos as usize;
        if consumed > 0 {
            self.in_buf.drain(..consumed.min(self.in_buf.len()));
            self.pos -= consumed as f64;
        }

        while self.out_buf.len() >= CHUNK_BYTES {
            let rest = self.out_buf.split_off(CHUNK_BYTES);
            let chunk = std::mem::replace(&mut self.out_buf, rest);
            // Best-effort: drop on a full/closed channel rather than block the
            // realtime audio thread.
            let _ = tx.try_send(Bytes::from(chunk));
        }
    }
}
