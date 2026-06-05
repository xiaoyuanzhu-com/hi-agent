//! On-disk paths for the raw memory store.
//!
//! Raw is the lossless source of truth, organized by scene (the isolation unit)
//! and sharded by UTC day so a forever-running scene stays bounded: a day's
//! everything — its `log.jsonl` and the blobs its signals reference — lives in
//! one folder, trivial to archive when cold.
//!
//! ```text
//! <data_dir>/memory/raw/<scene_enc>/
//!   ├── scene.json
//!   └── signals/<YYYY-MM-DD>/{ log.jsonl, <channel>-<id>.<ext> … }
//! ```
//!
//! Scene ids are arbitrary strings (`alice@phone`) and may carry path-unsafe
//! characters, so the directory name is a percent-encoding of the id; the true
//! id is recorded in `scene.json`.

use std::path::{Path, PathBuf};

use chrono::{DateTime, Utc};

use crate::types::Scene;

/// `<data_dir>/memory` — the root of the whole memory store (raw + derived).
pub fn memory_dir(data_dir: &Path) -> PathBuf {
    data_dir.join("memory")
}

/// `<memory>/raw` — the root of the lossless store.
pub fn raw_root(data_dir: &Path) -> PathBuf {
    memory_dir(data_dir).join("raw")
}

/// `<memory>/self.md` — the agent's evolving core identity (hand-authored/sticky).
pub fn self_path(data_dir: &Path) -> PathBuf {
    memory_dir(data_dir).join("self.md")
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

/// `<raw>/<scene_enc>` — one slice per scene.
pub fn scene_dir(data_dir: &Path, scene: &Scene) -> PathBuf {
    raw_root(data_dir).join(encode_scene(scene))
}

/// `<scene>/signals` — the time-sharded signal stream for a scene.
pub fn signals_dir(data_dir: &Path, scene: &Scene) -> PathBuf {
    scene_dir(data_dir, scene).join("signals")
}

/// `<scene>/signals/<YYYY-MM-DD>` — the day-folder a signal at `ts` belongs to,
/// holding that day's log and the blobs its signals reference.
pub fn day_dir(data_dir: &Path, scene: &Scene, ts: DateTime<Utc>) -> PathBuf {
    signals_dir(data_dir, scene).join(day_key(ts))
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
