//! Forgetting — fading a cold channel-day down to chosen keepsakes + text.
//!
//! Media is not kept forever (see `docs/memory.md` §3). A signal has three depths
//! of vividness: the permanent text surface (`.jsonl`, never touched here), the
//! full captured bytes (recent), and — between them — a sparse set of **keepsakes**
//! the mind judged worth keeping vivid. This module performs the *exact hands* of
//! forgetting; the *judgment* (which day is ripe, which moment to keep) belongs to
//! the reflection session, which calls [`keep_and_fade`] with the spans it chose.
//!
//! The one safety rail is mechanical, not the mind's to bend: a day is only ever
//! faded once it lies **strictly behind the scene's consolidation cursor**
//! (`max(episode.to_id)`), so reflection has always already turned that day's
//! signals into episodes — un-summarized detail can never be lost. Everything else
//! (when, what, how much) is the caller's soft call.
//!
//! Forgetting only ever rewrites or removes *blobs*. A `.jsonl` line is never
//! edited; its `media.file` keeps naming the original path, and a reader resolves
//! best-available (original → a `keep/` keepsake → caption-only).

use std::path::{Path, PathBuf};

use anyhow::{Context, bail};
use chrono::{DateTime, Duration, NaiveDate, TimeZone, Utc};

use crate::types::{Channel, Scene};
use crate::vendors::ffmpeg_frame;

use super::{episodes, journal, layout};

/// A slice of one channel-day to preserve as a keepsake when the rest fades.
/// `start == end` is a single instant (a vision still); a positive width is a
/// clip (a few seconds of sound).
#[derive(Debug, Clone, Copy)]
pub struct KeepSpan {
    pub start: DateTime<Utc>,
    pub end: DateTime<Utc>,
}

/// What one [`keep_and_fade`] did: how many keepsakes were cut and how many bytes
/// of full-fidelity media were dropped.
#[derive(Debug, Default, Clone, Copy)]
pub struct FadeReport {
    pub kept: usize,
    pub bytes_freed: u64,
}

/// One consolidated-but-still-heavy channel-day — the *pressure* the reflection
/// seed shows the mind so it can decide what is ripe to forget.
#[derive(Debug, Clone)]
pub struct FadeDay {
    pub channel: Channel,
    pub date: String,
    pub bytes: u64,
    pub age_days: i64,
    pub episodes: Vec<String>,
}

/// The channels that carry fadeable sensory bytes. `text` has none; `file/` is
/// verbatim handed artifacts (exempt, kept forever); the rest are unused senses.
const FADEABLE: [Channel; 2] = [Channel::Vision, Channel::Audio];

/// Midnight UTC of a `YYYY-MM-DD` day key.
fn day_start(date: &str) -> anyhow::Result<DateTime<Utc>> {
    let d = NaiveDate::parse_from_str(date, "%Y-%m-%d")
        .with_context(|| format!("bad date {date:?}, want YYYY-MM-DD"))?;
    Ok(Utc.from_utc_datetime(&d.and_hms_opt(0, 0, 0).expect("midnight is valid")))
}

/// The day strictly behind which everything in `scene` is consolidated:
/// `day_key(uuidv7_ts(scene_cursor))`. `None` when nothing has been consolidated
/// yet (genesis) — in which case nothing may fade.
async fn consolidated_through_day(
    data_dir: &Path,
    scene: &Scene,
) -> anyhow::Result<Option<String>> {
    let cursor = episodes::scene_cursor(data_dir, scene).await?;
    Ok(cursor
        .as_deref()
        .and_then(journal::uuidv7_ts)
        .map(layout::day_key))
}

/// Fade one `(scene, channel, date)`: cut each keep-span into a clip under
/// `<channel>/<date>/keep/`, then unlink the full-fidelity grid for that day,
/// leaving only `<channel>.jsonl` and `keep/`.
///
/// **Refuses** (returns `Err`) unless the whole day is strictly behind the scene
/// cursor — the safety rail. Keepsakes are written and fsynced *before* any byte
/// is dropped, and a keepsake that fails to cut aborts the whole fade (bytes
/// stay), so a moment the mind asked to keep is never lost to a half-done pass.
/// Idempotent: a day already stripped to `keep/` + `.jsonl` frees nothing and is
/// a clean no-op.
pub async fn keep_and_fade(
    data_dir: &Path,
    scene: &Scene,
    channel: Channel,
    date: &str,
    keep: &[KeepSpan],
) -> anyhow::Result<FadeReport> {
    if !FADEABLE.contains(&channel) {
        bail!("channel {} carries no fadeable media", channel.as_str());
    }
    let day0 = day_start(date)?;

    // The safety rail.
    match consolidated_through_day(data_dir, scene).await? {
        None => bail!("refusing to fade {date}: nothing consolidated in this scene yet"),
        Some(through) if date >= through.as_str() => bail!(
            "refusing to fade {date}: not strictly behind the consolidation cursor ({through})"
        ),
        Some(_) => {}
    }

    let dir = layout::channel_day_dir(data_dir, scene, channel, day0);
    if !tokio::fs::try_exists(&dir).await.unwrap_or(false) {
        return Ok(FadeReport::default());
    }

    // 1. Keepsakes first — write + fsync each before any byte is dropped.
    let grid = input_grid(&dir, day0).await?;
    let keep_dir = dir.join("keep");
    let mut report = FadeReport::default();
    for span in keep {
        let cut = cut_keepsake(&keep_dir, channel, span, &grid)
            .await
            .with_context(|| "keepsake cut failed; leaving full bytes in place")?;
        if cut {
            report.kept += 1;
        }
    }

    // 2. Drop the full-fidelity grid (input minutes, one-offs, output/), keeping
    //    only the day-log and the keepsakes.
    report.bytes_freed = drop_full_bytes(&dir, channel).await?;
    Ok(report)
}

/// Survey a scene's consolidated-but-still-heavy media: every `(channel, day)`
/// strictly behind the cursor that still holds full bytes, heaviest first. This is
/// the pressure the reflection seed surfaces; it makes no decision. Empty at
/// genesis (nothing consolidated) and when no cold day still holds bytes.
pub async fn fade_pressure(
    data_dir: &Path,
    scene: &Scene,
    now: DateTime<Utc>,
) -> anyhow::Result<Vec<FadeDay>> {
    let Some(through) = consolidated_through_day(data_dir, scene).await? else {
        return Ok(Vec::new());
    };
    let mut out = Vec::new();
    for channel in FADEABLE {
        let root = layout::scene_dir(data_dir, scene).join(channel.as_str());
        let mut rd = match tokio::fs::read_dir(&root).await {
            Ok(rd) => rd,
            Err(err) if err.kind() == std::io::ErrorKind::NotFound => continue,
            Err(err) => return Err(err.into()),
        };
        while let Some(ent) = rd.next_entry().await? {
            if !ent.file_type().await?.is_dir() {
                continue;
            }
            let Ok(date) = ent.file_name().into_string() else { continue };
            // Day-folders are YYYY-MM-DD, so a string compare is a date compare.
            if date.as_str() >= through.as_str() {
                continue; // not strictly behind the cursor → not yet fadeable
            }
            let bytes = sum_full_bytes(&ent.path(), channel).await?;
            if bytes == 0 {
                continue; // already faded to text, or text-only
            }
            let age_days = day_start(&date)
                .map(|d| (now - d).num_days())
                .unwrap_or(0);
            let episodes = episodes::names_overlapping_day(data_dir, scene, &date)
                .await
                .unwrap_or_default();
            out.push(FadeDay { channel, date, bytes, age_days, episodes });
        }
    }
    out.sort_by(|a, b| b.bytes.cmp(&a.bytes)); // heaviest first
    Ok(out)
}

/// One input grid blob and the wall-clock instant it begins.
struct GridFile {
    path: PathBuf,
    start: DateTime<Utc>,
}

/// The day's **input** grid blobs (`<HH>/<MM>.<ext>` minutes and `<HH>/<MM>-<SS>.<ext>`
/// one-offs), sorted by start. Skips `output/`, `keep/`, and the day-log — keepsakes
/// preserve the perceived world, not the agent's own output.
async fn input_grid(dir: &Path, day0: DateTime<Utc>) -> anyhow::Result<Vec<GridFile>> {
    let mut grid = Vec::new();
    let mut hrd = tokio::fs::read_dir(dir).await?;
    while let Some(hh_ent) = hrd.next_entry().await? {
        if !hh_ent.file_type().await?.is_dir() {
            continue;
        }
        let Ok(hh_name) = hh_ent.file_name().into_string() else { continue };
        let Ok(hh) = hh_name.parse::<u32>() else { continue }; // skips output/, keep/
        if hh > 23 {
            continue;
        }
        let mut frd = tokio::fs::read_dir(hh_ent.path()).await?;
        while let Some(f) = frd.next_entry().await? {
            if !f.file_type().await?.is_file() {
                continue;
            }
            let Ok(fname) = f.file_name().into_string() else { continue };
            let Some((mm, ss)) = parse_minute_file(&fname) else { continue };
            let start = day0 + Duration::hours(hh as i64) + Duration::minutes(mm as i64)
                + Duration::seconds(ss.unwrap_or(0) as i64);
            grid.push(GridFile { path: f.path(), start });
        }
    }
    grid.sort_by_key(|g| g.start);
    Ok(grid)
}

/// Parse a grid filename stem into `(minute, Some(second))` for a one-off or
/// `(minute, None)` for a streamed minute. `16.mp3` → `(16, None)`, `16-45.mp3`
/// → `(16, Some(45))`. `None` for anything that isn't `MM[-SS].ext`.
fn parse_minute_file(name: &str) -> Option<(u32, Option<u32>)> {
    let stem = name.rsplit_once('.').map(|(s, _)| s).unwrap_or(name);
    match stem.split_once('-') {
        Some((mm, ss)) => Some((mm.parse().ok()?, Some(ss.parse().ok()?))),
        None => Some((stem.parse().ok()?, None)),
    }
}

/// Cut one keepsake from the day's grid into `keep_dir`, returning whether
/// anything was written (false when no captured bytes underlie the span — a gap
/// the mind referenced is not an error). Vision keepsakes are a still frame at the
/// span's start; audio keepsakes are a lossless clip of `[start, end)`.
async fn cut_keepsake(
    keep_dir: &Path,
    channel: Channel,
    span: &KeepSpan,
    grid: &[GridFile],
) -> anyhow::Result<bool> {
    if grid.is_empty() {
        return Ok(false);
    }
    tokio::fs::create_dir_all(keep_dir).await?;
    match channel {
        Channel::Vision => {
            // The blob being written at the span's instant: the latest start <= it,
            // else the earliest (the instant predates capture — clamp to its open).
            let file = grid
                .iter()
                .rev()
                .find(|g| g.start <= span.start)
                .unwrap_or(&grid[0]);
            let offset = (span.start - file.start).num_milliseconds().max(0) as f64 / 1000.0;
            let jpg = ffmpeg_frame::still_at(&file.path, offset).await?;
            let out = keep_dir.join(format!("{}.jpg", hms(span.start)));
            write_synced(&out, &jpg).await?;
            Ok(true)
        }
        Channel::Audio => {
            let dur = (span.end - span.start).num_milliseconds() as f64 / 1000.0;
            if dur <= 0.0 {
                return Ok(false); // a clip needs width
            }
            // Minute blobs overlapping [start, end]; a streamed minute spans ~60s.
            let inputs: Vec<PathBuf> = grid
                .iter()
                .filter(|g| g.start <= span.end && g.start + Duration::seconds(60) >= span.start)
                .map(|g| g.path.clone())
                .collect();
            let Some(first) = grid
                .iter()
                .find(|g| g.start <= span.end && g.start + Duration::seconds(60) >= span.start)
            else {
                return Ok(false);
            };
            let ss = (span.start - first.start).num_milliseconds().max(0) as f64 / 1000.0;
            let out = keep_dir.join(format!("{}-{}.wav", hms(span.start), hms(span.end)));
            ffmpeg_frame::clip_audio(&inputs, ss, dur, &out).await?;
            Ok(true)
        }
        _ => Ok(false),
    }
}

/// `HHMMSS` of an instant — the keepsake filename stem, by which a reader matches
/// a keepsake to a faded signal's `ts`.
fn hms(ts: DateTime<Utc>) -> String {
    ts.format("%H%M%S").to_string()
}

/// Write `bytes` to `path` and fsync, so a keepsake is durable before the full
/// bytes it stands in for are dropped.
async fn write_synced(path: &Path, bytes: &[u8]) -> anyhow::Result<()> {
    use tokio::io::AsyncWriteExt;
    let mut f = tokio::fs::File::create(path).await?;
    f.write_all(bytes).await?;
    f.flush().await?;
    f.sync_data().await?;
    Ok(())
}

/// Remove every full-fidelity entry under a channel-day (input grid, one-offs,
/// `output/`), keeping only `<channel>.jsonl` and `keep/`. Returns bytes freed.
async fn drop_full_bytes(dir: &Path, channel: Channel) -> anyhow::Result<u64> {
    let freed = sum_full_bytes(dir, channel).await?;
    let jsonl = format!("{}.jsonl", channel.as_str());
    let mut rd = tokio::fs::read_dir(dir).await?;
    while let Some(ent) = rd.next_entry().await? {
        let name = ent.file_name().to_string_lossy().into_owned();
        if name == jsonl || name == "keep" {
            continue;
        }
        if ent.file_type().await?.is_dir() {
            tokio::fs::remove_dir_all(ent.path()).await?;
        } else {
            tokio::fs::remove_file(ent.path()).await?;
        }
    }
    Ok(freed)
}

/// Total bytes of a channel-day's full-fidelity media — everything except the
/// day-log and `keep/`. Used both to drop and to weigh fade pressure.
async fn sum_full_bytes(dir: &Path, channel: Channel) -> anyhow::Result<u64> {
    let jsonl = format!("{}.jsonl", channel.as_str());
    let mut total = 0u64;
    let mut stack = Vec::new();
    // Seed with the day's top-level entries except the log and keepsakes.
    let mut rd = match tokio::fs::read_dir(dir).await {
        Ok(rd) => rd,
        Err(err) if err.kind() == std::io::ErrorKind::NotFound => return Ok(0),
        Err(err) => return Err(err.into()),
    };
    while let Some(ent) = rd.next_entry().await? {
        let name = ent.file_name().to_string_lossy().into_owned();
        if name == jsonl || name == "keep" {
            continue;
        }
        if ent.file_type().await?.is_dir() {
            stack.push(ent.path());
        } else {
            total += ent.metadata().await?.len();
        }
    }
    while let Some(d) = stack.pop() {
        let mut rd = tokio::fs::read_dir(&d).await?;
        while let Some(ent) = rd.next_entry().await? {
            if ent.file_type().await?.is_dir() {
                stack.push(ent.path());
            } else {
                total += ent.metadata().await?.len();
            }
        }
    }
    Ok(total)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::memory::journal::Journal;
    use crate::types::JournalEntry;
    use uuid::Uuid;

    /// Append `n` text signals "now" and record one episode covering them, so the
    /// scene cursor sits on today — the precondition for fading any earlier day.
    async fn consolidate_today(dir: &Path, scene: &Scene) {
        let j = Journal::open(dir.to_path_buf()).await.unwrap();
        for _ in 0..3 {
            j.append(JournalEntry::SignalIn {
                id: Uuid::now_v7().to_string(),
                ts: Utc::now(),
                channel: Channel::Text,
                scene: scene.clone(),
                body: "x".into(),
                stream: None,
                media: None,
                origin: None,
            })
            .await
            .unwrap();
            tokio::time::sleep(std::time::Duration::from_millis(2)).await;
        }
        episodes::record_episode(dir, scene, 3, "today", "today", &[]).await.unwrap();
    }

    /// Lay down a fake faded-able day: `<scene>/audio/<date>/{audio.jsonl, 09/16.wav}`.
    async fn seed_audio_day(dir: &Path, scene: &Scene, date: &str, wav_bytes: usize) {
        let day = layout::channel_day_dir(dir, scene, Channel::Audio, day_start(date).unwrap());
        tokio::fs::create_dir_all(day.join("09")).await.unwrap();
        tokio::fs::write(day.join("audio.jsonl"), b"{}\n").await.unwrap();
        tokio::fs::write(day.join("09").join("16.wav"), vec![0u8; wav_bytes]).await.unwrap();
    }

    #[tokio::test]
    async fn refuses_at_genesis() {
        let dir = tempfile::tempdir().unwrap();
        let scene = Scene("s".into());
        seed_audio_day(dir.path(), &scene, "2000-01-01", 1000).await;
        // No episode yet → no cursor → must refuse.
        assert!(keep_and_fade(dir.path(), &scene, Channel::Audio, "2000-01-01", &[]).await.is_err());
    }

    #[tokio::test]
    async fn refuses_unconsolidated_day() {
        let dir = tempfile::tempdir().unwrap();
        let scene = Scene("s".into());
        consolidate_today(dir.path(), &scene).await;
        let today = Utc::now().format("%Y-%m-%d").to_string();
        // Today is the cursor's day → not strictly behind it → refuse.
        assert!(keep_and_fade(dir.path(), &scene, Channel::Audio, &today, &[]).await.is_err());
    }

    #[tokio::test]
    async fn fades_old_day_to_text() {
        let dir = tempfile::tempdir().unwrap();
        let scene = Scene("s".into());
        consolidate_today(dir.path(), &scene).await;
        seed_audio_day(dir.path(), &scene, "2000-01-01", 4096).await;

        let report = keep_and_fade(dir.path(), &scene, Channel::Audio, "2000-01-01", &[])
            .await
            .unwrap();
        assert_eq!(report.bytes_freed, 4096);
        assert_eq!(report.kept, 0);

        let day = layout::channel_day_dir(dir.path(), &scene, Channel::Audio, day_start("2000-01-01").unwrap());
        assert!(tokio::fs::try_exists(day.join("audio.jsonl")).await.unwrap(), "log stays");
        assert!(!tokio::fs::try_exists(day.join("09")).await.unwrap(), "grid gone");

        // Idempotent: a second pass frees nothing and does not error.
        let again = keep_and_fade(dir.path(), &scene, Channel::Audio, "2000-01-01", &[]).await.unwrap();
        assert_eq!(again.bytes_freed, 0);
    }

    #[tokio::test]
    async fn pressure_lists_only_consolidated_heavy_days() {
        let dir = tempfile::tempdir().unwrap();
        let scene = Scene("s".into());
        consolidate_today(dir.path(), &scene).await;
        seed_audio_day(dir.path(), &scene, "2000-01-01", 2048).await;
        let today = Utc::now().format("%Y-%m-%d").to_string();
        seed_audio_day(dir.path(), &scene, &today, 9999).await; // not behind cursor → excluded

        let pressure = fade_pressure(dir.path(), &scene, Utc::now()).await.unwrap();
        assert_eq!(pressure.len(), 1);
        assert_eq!(pressure[0].date, "2000-01-01");
        assert_eq!(pressure[0].bytes, 2048);
    }

    #[test]
    fn parses_grid_names() {
        assert_eq!(parse_minute_file("16.mp3"), Some((16, None)));
        assert_eq!(parse_minute_file("16-45.wav"), Some((16, Some(45))));
        assert_eq!(parse_minute_file("keep"), None);
    }
}
