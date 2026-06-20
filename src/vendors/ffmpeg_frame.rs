//! Extract a single still frame from a video clip by shelling out to `ffmpeg`.
//!
//! The vision input channel is video (fragmented MP4 with hardware HEVC/H.264, or
//! WebM with VP8/VP9 — negotiated client-side). The face pipeline, like `vision`,
//! wants an *encoded still image*. Decoding HEVC/VP9 in-process would mean linking
//! libav; instead we reuse the same managed-runtime philosophy the rest of the app
//! follows and call the `ffmpeg` binary, which already handles every container and
//! codec the client can produce. One keyframe per minute-file is all the face path
//! needs — recognition is soft evidence, not surveillance.
//!
//! Best-effort by contract: a missing binary, an undecodable clip, or an empty
//! result is an `Err` the caller logs and skips. The frame comes back as JPEG
//! bytes, ready for [`crate::capabilities::face::detect_and_embed`].

use std::process::Stdio;

use anyhow::{Context, bail};
use bytes::Bytes;
use tokio::io::AsyncWriteExt;
use tokio::process::Command;

/// Env override for the ffmpeg binary; defaults to `ffmpeg` on `PATH`.
const ENV_FFMPEG_BIN: &str = "FFMPEG_BIN";

fn ffmpeg_bin() -> String {
    std::env::var(ENV_FFMPEG_BIN).ok().filter(|s| !s.trim().is_empty()).unwrap_or_else(|| "ffmpeg".to_string())
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
