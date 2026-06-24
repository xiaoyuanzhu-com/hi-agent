//! Still-frame and clip extraction by shelling out to `ffmpeg`.
//!
//! The vision input channel is video (fragmented MP4 with hardware HEVC/H.264, or
//! WebM with VP8/VP9 — negotiated client-side). The face pipeline, like `vision`,
//! wants an *encoded still image*. Decoding HEVC/VP9 in-process would mean linking
//! libav; instead we reuse the same managed-runtime philosophy the rest of the app
//! follows and call the `ffmpeg` binary, which already handles every container and
//! codec the client can produce. One keyframe per minute-file is all the face path
//! needs — recognition is soft evidence, not surveillance.
//!
//! The forgetting pass ([`crate::mind::memory::decay`]) reuses the same binary to cut a
//! keepsake out of cold media before it drops the full bytes: [`still_at`] for a
//! single vision frame, [`clip_audio`] for a few seconds of sound.
//!
//! Best-effort by contract: a missing binary, an undecodable clip, or an empty
//! result is an `Err` the caller logs and skips. A frame comes back as JPEG bytes,
//! ready for [`crate::body::capabilities::face::detect_and_embed`].

use std::path::{Path, PathBuf};
use std::process::Stdio;

use anyhow::{Context, bail};
use bytes::Bytes;
use tokio::io::AsyncWriteExt;
use tokio::process::Command;

/// Env override for the ffmpeg binary; defaults to `ffmpeg` on `PATH`.
const ENV_FFMPEG_BIN: &str = "FFMPEG_BIN";

/// Resolve which `ffmpeg` to run: explicit `FFMPEG_BIN` override → the static
/// ffmpeg bundled in a packaged `.app` ([`super::ffmpeg::bundled_bin`]) → plain
/// `ffmpeg` on `PATH`. The bundle tier is what makes a shipped app work without
/// the user installing ffmpeg; dev/Docker have no bundle and fall through to PATH.
fn ffmpeg_bin() -> String {
    if let Ok(s) = std::env::var(ENV_FFMPEG_BIN) {
        let s = s.trim();
        if !s.is_empty() {
            return s.to_string();
        }
    }
    if let Some(p) = super::ffmpeg::bundled_bin() {
        return p.to_string_lossy().into_owned();
    }
    "ffmpeg".to_string()
}

/// Decode the first frame of `video` (a self-contained clip — init segment
/// prefixed) to JPEG bytes. The bytes are piped in and the frame piped out, so no
/// temp file touches disk. Reads from `pipe:0` sequentially (we only want the
/// opening frame, so no seeking is required, which keeps fragmented-MP4/WebM input
/// happy over a pipe).
pub async fn first_frame(video: Bytes) -> anyhow::Result<Bytes> {
    let mut child = Command::new(ffmpeg_bin())
        .args([
            "-hide_banner",
            "-loglevel", "error",
            "-i", "pipe:0",
            "-frames:v", "1",
            "-f", "image2",
            "-c:v", "mjpeg",
            "pipe:1",
        ])
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .with_context(|| format!("spawning {} failed (is it installed?)", ffmpeg_bin()))?;

    // Write the whole clip to stdin on its own task, then drop the handle to send
    // EOF — done concurrently with reading stdout so a large clip can't deadlock.
    let mut stdin = child.stdin.take().context("ffmpeg stdin missing")?;
    let writer = tokio::spawn(async move {
        let _ = stdin.write_all(&video).await;
        // Drop closes stdin → ffmpeg sees EOF.
    });

    let out = child.wait_with_output().await.context("waiting on ffmpeg failed")?;
    let _ = writer.await;

    if !out.status.success() {
        bail!("ffmpeg frame extraction failed: {}", String::from_utf8_lossy(&out.stderr).trim());
    }
    if out.stdout.is_empty() {
        bail!("ffmpeg produced no frame");
    }
    Ok(Bytes::from(out.stdout))
}

/// Decode one frame at `offset_secs` into the video file `input` to JPEG bytes —
/// the keepsake form for a vision moment the mind chose to keep. Seeks before
/// decoding (`-ss` before `-i`) for speed; ffmpeg clamps a past-the-end offset to
/// the last frame, so a slightly-off timestamp still yields an image.
pub async fn still_at(input: &Path, offset_secs: f64) -> anyhow::Result<Bytes> {
    let out = Command::new(ffmpeg_bin())
        .args(["-hide_banner", "-loglevel", "error", "-ss", &format!("{offset_secs:.3}"), "-i"])
        .arg(input)
        .args(["-frames:v", "1", "-f", "image2", "-c:v", "mjpeg", "pipe:1"])
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .output()
        .await
        .with_context(|| format!("spawning {} failed (is it installed?)", ffmpeg_bin()))?;
    if !out.status.success() {
        bail!("ffmpeg still extraction failed: {}", String::from_utf8_lossy(&out.stderr).trim());
    }
    if out.stdout.is_empty() {
        bail!("ffmpeg produced no still");
    }
    Ok(Bytes::from(out.stdout))
}

/// Cut `[ss, ss+dur)` out of the concatenation of `inputs` (same-format audio —
/// the wav minute files a span crosses, in order) and write it to `out` as
/// lossless PCM wav — the keepsake form for a few seconds of sound. Re-encoding to
/// `pcm_s16le` is not the rejected low-bitrate "fade in place": it is a short,
/// full-quality excerpt of the same samples. A single input is the common case;
/// `concat:` joins several when a span straddles a minute boundary.
pub async fn clip_audio(inputs: &[PathBuf], ss: f64, dur: f64, out: &Path) -> anyhow::Result<()> {
    if inputs.is_empty() {
        bail!("clip_audio: no input files");
    }
    let joined = inputs
        .iter()
        .map(|p| p.to_string_lossy().into_owned())
        .collect::<Vec<_>>()
        .join("|");
    let input_arg = if inputs.len() == 1 { joined } else { format!("concat:{joined}") };
    let status = Command::new(ffmpeg_bin())
        .args(["-hide_banner", "-loglevel", "error", "-i"])
        .arg(&input_arg)
        .args(["-ss", &format!("{ss:.3}"), "-t", &format!("{dur:.3}"), "-c:a", "pcm_s16le"])
        .arg(out)
        .stdout(Stdio::null())
        .stderr(Stdio::piped())
        .output()
        .await
        .with_context(|| format!("spawning {} failed (is it installed?)", ffmpeg_bin()))?;
    if !status.status.success() {
        bail!("ffmpeg clip failed: {}", String::from_utf8_lossy(&status.stderr).trim());
    }
    Ok(())
}
