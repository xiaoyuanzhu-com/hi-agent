//! Media blob storage — out-of-log bytes for audio, vision frames, etc.
//!
//! A blob lives inside the channel-day folder that holds the channel's log, on a
//! wall-clock grid (see [`super::layout::media_rel_path`]): a one-off capture is
//! `<HH>/<MM>-<SS>.<ext>`, a streamed minute `<HH>/<MM>.<ext>`. The day-log
//! records only the path (relative to the channel-day folder) and metadata; the
//! bytes never enter the JSONL stream (which would blow up readers and bloat
//! snapshots).
//!
//! Old bytes fade (see [`super::decay`]): once a day is consolidated and cold, the
//! forgetting pass drops the full grid, keeping only chosen keepsakes under
//! `keep/`. The `.jsonl` line is never rewritten, so [`resolve`] does the
//! best-available lookup on read — original blob, else nearest keepsake, else the
//! caption alone.

use std::path::{Path, PathBuf};

use chrono::{DateTime, Timelike, Utc};
use tokio::io::AsyncWriteExt;

use crate::types::{Channel, Scene};

use super::layout::{self, MediaSlot};

/// Persist `bytes` inside the scene's channel-day folder for `ts` — the same
/// folder that holds `<channel>.jsonl` — at the grid slot `slot` dictates, and
/// return the path **relative to that folder**. The caller records it in the
/// entry's `media.file`; a reader resolves it as
/// `channel_day_dir(..).join(media.file)`.
pub async fn store_blob(
    data_dir: &Path,
    scene: &Scene,
    channel: Channel,
    ts: DateTime<Utc>,
    slot: MediaSlot,
    ext: &str,
    bytes: &[u8],
) -> anyhow::Result<String> {
    let dir = layout::channel_day_dir(data_dir, scene, channel, ts);
    let rel = layout::media_rel_path(ts, slot, ext);
    let path = dir.join(&rel);
    if let Some(parent) = path.parent() {
        tokio::fs::create_dir_all(parent).await?;
    }

    let mut f = tokio::fs::File::create(&path).await?;
    f.write_all(bytes).await?;
    f.flush().await?;
    f.sync_data().await?;
    Ok(rel)
}

/// Best-available path for a signal's media: the original blob if it still exists,
/// else the nearest keepsake [`super::decay`] left when the day faded, else `None`
/// (the signal survives as its text surface alone). `rel` is the entry's
/// `media.file`; `ts` its timestamp. Readers of historical media should resolve
/// through this rather than joining `media.file` directly, so faded days degrade
/// gracefully instead of 404-ing.
pub async fn resolve(
    data_dir: &Path,
    scene: &Scene,
    channel: Channel,
    ts: DateTime<Utc>,
    rel: &str,
) -> Option<PathBuf> {
    let dir = layout::channel_day_dir(data_dir, scene, channel, ts);
    let original = dir.join(rel);
    if tokio::fs::try_exists(&original).await.unwrap_or(false) {
        return Some(original);
    }
    nearest_keepsake(&dir.join("keep"), ts).await
}

/// The keepsake in `keep_dir` nearest `ts` — one whose span contains it, else the
/// least-distant. Names are `HHMMSS.<ext>` (a vision still) or
/// `HHMMSS-HHMMSS.<ext>` (an audio clip). `None` when there are no keepsakes.
async fn nearest_keepsake(keep_dir: &Path, ts: DateTime<Utc>) -> Option<PathBuf> {
    let target = i64::from(ts.hour() * 3600 + ts.minute() * 60 + ts.second());
    let mut rd = tokio::fs::read_dir(keep_dir).await.ok()?;
    let mut best: Option<(i64, PathBuf)> = None;
    while let Ok(Some(ent)) = rd.next_entry().await {
        let Ok(name) = ent.file_name().into_string() else { continue };
        let Some((start, end)) = parse_keep_span(&name) else { continue };
        let dist = if target < start {
            start - target
        } else if target > end {
            target - end
        } else {
            0
        };
        if best.as_ref().is_none_or(|(d, _)| dist < *d) {
            best = Some((dist, ent.path()));
        }
    }
    best.map(|(_, p)| p)
}

/// Parse a keepsake filename into its `[start, end]` seconds-of-day span. An
/// instant (`091623.jpg`) is a zero-width span; a clip (`091610-091618.wav`) the
/// two endpoints. `None` if the stem isn't `HHMMSS[-HHMMSS]`.
fn parse_keep_span(name: &str) -> Option<(i64, i64)> {
    let stem = name.rsplit_once('.').map(|(s, _)| s).unwrap_or(name);
    match stem.split_once('-') {
        Some((a, b)) => Some((hms_to_secs(a)?, hms_to_secs(b)?)),
        None => {
            let s = hms_to_secs(stem)?;
            Some((s, s))
        }
    }
}

/// `HHMMSS` → seconds of day, or `None` if it isn't six digits.
fn hms_to_secs(s: &str) -> Option<i64> {
    if s.len() != 6 || !s.bytes().all(|b| b.is_ascii_digit()) {
        return None;
    }
    let hh: i64 = s[0..2].parse().ok()?;
    let mm: i64 = s[2..4].parse().ok()?;
    let ss: i64 = s[4..6].parse().ok()?;
    Some(hh * 3600 + mm * 60 + ss)
}

#[cfg(test)]
mod tests {
    use super::*;
    use chrono::TimeZone;

    #[test]
    fn parses_keep_names() {
        assert_eq!(parse_keep_span("091623.jpg"), Some((33383, 33383)));
        assert_eq!(parse_keep_span("091610-091618.wav"), Some((33370, 33378)));
        assert_eq!(parse_keep_span("keep"), None);
    }

    #[tokio::test]
    async fn resolves_original_then_keepsake_then_none() {
        let tmp = tempfile::tempdir().unwrap();
        let dir = tmp.path();
        let scene = Scene("s".into());
        let when = Utc.with_ymd_and_hms(2000, 1, 1, 9, 16, 0).unwrap();
        let rel = store_blob(dir, &scene, Channel::Audio, when, MediaSlot::InputStream, "wav", b"x")
            .await
            .unwrap();

        // Original present → returns it.
        let got = resolve(dir, &scene, Channel::Audio, when, &rel).await.unwrap();
        assert!(got.ends_with("09/16.wav"));

        // Original gone, a keepsake left → falls back to the keepsake.
        let day = layout::channel_day_dir(dir, &scene, Channel::Audio, when);
        tokio::fs::remove_file(day.join(&rel)).await.unwrap();
        tokio::fs::create_dir_all(day.join("keep")).await.unwrap();
        tokio::fs::write(day.join("keep").join("091610-091618.wav"), b"k").await.unwrap();
        let got = resolve(dir, &scene, Channel::Audio, when, &rel).await.unwrap();
        assert!(got.ends_with("keep/091610-091618.wav"));

        // Keepsake gone too → caption-only (None).
        tokio::fs::remove_file(day.join("keep").join("091610-091618.wav")).await.unwrap();
        assert!(resolve(dir, &scene, Channel::Audio, when, &rel).await.is_none());
    }
}
