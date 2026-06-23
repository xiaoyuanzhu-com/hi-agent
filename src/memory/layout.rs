//! On-disk paths for the raw memory store.
//!
//! Raw is the lossless source of truth, organized by scene (the isolation unit),
//! then by **channel**, then sharded by UTC day. A channel is that sense's
//! complete record; the day-folder keeps reads bounded and makes per-channel
//! fading/archival a single subtree. Each channel-day carries a surface log
//! named for the channel (`text.jsonl`, `audio.jsonl`, …) plus the bytes its
//! signals reference, laid out on a wall-clock grid.
//!
//! ```text
//! <data_dir>/memory/raw/<scene_enc>/
//!   ├── scene.json
//!   ├── text/<YYYY-MM-DD>/text.jsonl
//!   ├── audio/<YYYY-MM-DD>/{ audio.jsonl, <HH>/<MM>-<SS>.<ext>, output/<HH>/<MM>.<ext> … }
//!   └── vision/<YYYY-MM-DD>/{ vision.jsonl, <HH>/<MM>-<SS>.<ext> … }
//! ```
//!
//! Scene ids are arbitrary strings (`alice@phone`) and may carry path-unsafe
//! characters, so the directory name is a percent-encoding of the id; the true
//! id is recorded in `scene.json`.

use std::path::{Path, PathBuf};

use chrono::{DateTime, Utc};

use crate::types::{Channel, Scene};

/// Where a signal's media bytes sit within its channel-day folder. Input is the
/// default (bare); output lives under `output/`. A one-off capture (a posted
/// clip, a still) gets a second-precision name so it never collides with a
/// streamed minute file; a streamed chunk owns the bare `<HH>/<MM>` minute slot.
#[derive(Debug, Clone, Copy)]
pub enum MediaSlot {
    /// A discrete one-off capture (posted clip / still): `<HH>/<MM>-<SS>.<ext>`.
    InputOneOff,
    /// A minute of an open input stream (mic, camera): `<HH>/<MM>.<ext>`.
    InputStream,
    /// A minute of an output stream (TTS, generated frames): `output/<HH>/<MM>.<ext>`.
    OutputStream,
}

/// `<data_dir>/memory` — the root of the whole memory store (raw + derived).
pub fn memory_dir(data_dir: &Path) -> PathBuf {
    data_dir.join("memory")
}

/// `<memory>/raw` — the root of the lossless store.
pub fn raw_root(data_dir: &Path) -> PathBuf {
    memory_dir(data_dir).join("raw")
}

/// `<memory>/hot.md` — the always-loaded working set (a regenerable projection).
pub fn hot_path(data_dir: &Path) -> PathBuf {
    memory_dir(data_dir).join("hot.md")
}

/// `<memory>/episodes` — derived event bundles.
pub fn episodes_dir(data_dir: &Path) -> PathBuf {
    memory_dir(data_dir).join("episodes")
}

/// `<memory>/facets` — derived current-understanding of subjects.
pub fn facets_dir(data_dir: &Path) -> PathBuf {
    memory_dir(data_dir).join("facets")
}

/// `<memory>/reflexes` — taught quick-action reflexes (one `<id>.json` each). The
/// deepest stage of the memory gradient: a grooved action the fast-path fires
/// without the mind. Written by the `record_reflex` tool, read by the invoke path.
pub fn reflexes_dir(data_dir: &Path) -> PathBuf {
    memory_dir(data_dir).join("reflexes")
}

/// `<raw>/<scene_enc>` — one slice per scene.
pub fn scene_dir(data_dir: &Path, scene: &Scene) -> PathBuf {
    raw_root(data_dir).join(encode_scene(scene))
}

/// `<scene>/<channel>/<YYYY-MM-DD>` — the channel-day folder a signal at `ts`
/// belongs to, holding that day's surface log and the bytes its signals
/// reference. The parent of both the log and the byte grid.
pub fn channel_day_dir(
    data_dir: &Path,
    scene: &Scene,
    channel: Channel,
    ts: DateTime<Utc>,
) -> PathBuf {
    scene_dir(data_dir, scene)
        .join(channel.as_str())
        .join(day_key(ts))
}

/// `<channel>/<date>/<channel>.jsonl` — the day's surface log for one channel,
/// named for the channel so the file is self-describing even detached from its
/// folder.
pub fn channel_log_path(
    data_dir: &Path,
    scene: &Scene,
    channel: Channel,
    ts: DateTime<Utc>,
) -> PathBuf {
    channel_day_dir(data_dir, scene, channel, ts).join(format!("{}.jsonl", channel.as_str()))
}

/// `<scene>/appearance/<YYYY-MM-DD>` — the day-folder for a scene's screen-state
/// history. Appearance is a state channel, not an event stream: it holds
/// timestamped whole-state snapshots (`appearance-<HHMMSSZ>.json`), not a
/// `<channel>.jsonl`, so it is reached through this helper rather than
/// [`channel_day_dir`] (there is no `Channel::Appearance`).
pub fn appearance_day_dir(data_dir: &Path, scene: &Scene, ts: DateTime<Utc>) -> PathBuf {
    scene_dir(data_dir, scene)
        .join("appearance")
        .join(day_key(ts))
}

/// The byte path for a signal's media **relative to its channel-day folder**, by
/// slot (see [`MediaSlot`]). Stored verbatim in the entry's `media.file`, so a
/// reader resolves it as `channel_day_dir(..).join(media.file)`.
pub fn media_rel_path(ts: DateTime<Utc>, slot: MediaSlot, ext: &str) -> String {
    let hh = ts.format("%H");
    let mm = ts.format("%M");
    match slot {
        MediaSlot::InputOneOff => format!("{hh}/{mm}-{}.{ext}", ts.format("%S")),
        MediaSlot::InputStream => format!("{hh}/{mm}.{ext}"),
        MediaSlot::OutputStream => format!("output/{hh}/{mm}.{ext}"),
    }
}

/// The lexically-sortable day key (`YYYY-MM-DD`, UTC) used as a day-folder name.
pub fn day_key(ts: DateTime<Utc>) -> String {
    ts.format("%Y-%m-%d").to_string()
}

/// Path-safe directory name for a scene: percent-encode every byte outside the
/// unreserved set `[A-Za-z0-9._-]`. Deterministic, so a scene always maps to the
/// same folder; the inverse is never needed (the true id lives in `scene.json`).
pub fn encode_scene(scene: &Scene) -> String {
    let mut out = String::with_capacity(scene.0.len());
    for b in scene.0.bytes() {
        match b {
            b'A'..=b'Z' | b'a'..=b'z' | b'0'..=b'9' | b'.' | b'_' | b'-' => out.push(b as char),
            _ => out.push_str(&format!("%{b:02X}")),
        }
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn encodes_path_unsafe_chars() {
        assert_eq!(encode_scene(&Scene("alice@phone".into())), "alice%40phone");
        assert_eq!(encode_scene(&Scene("a/b".into())), "a%2Fb");
        assert_eq!(encode_scene(&Scene("plain-1.0_x".into())), "plain-1.0_x");
    }
}
