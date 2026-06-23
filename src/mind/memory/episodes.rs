//! Derived event bundles — `memory/episodes/<date>-<slug>/episode.md`.
//!
//! An episode is a coherent event within a scene, a **derived projection** over
//! the raw log: regenerable, never the source of truth. Reflection (the "sleep"
//! pass; see [`crate::body::reactor::heartbeat`]) segments the scene's unconsolidated
//! frontier into episodes — each a gist under frontmatter recording the scene,
//! the signal-id range it covers, and the subjects it touched.
//!
//! ## The cursor
//!
//! There is no separate watermark file: the "what has been consolidated" cursor
//! for a scene is `max(to_id)` over its episodes ([`scene_cursor`]). Deleting
//! `episodes/` therefore resets consolidation to genesis and a re-run rebuilds
//! everything (regenerate-don't-patch). Episodes are **sequential cuts** of the
//! post-cursor stream: [`record_episode`] takes a `count` of leading
//! unconsolidated signals, resolves the range from raw, and advances the cursor
//! by exactly that many — so the mind never handles a raw signal id.

use std::path::Path;

use uuid::Uuid;

use super::{Memory, journal, layout};
use crate::types::Scene;

/// How many of a scene's unconsolidated signals one reflection round reads from
/// the frontier (oldest first). Both the reflection orchestration's seeding and
/// the `record_episode` tool resolve against this same cap, so the `count` the
/// mind chooses always lands within the tail it was shown. A large backlog drains
/// forward over several reflections rather than flooding one.
pub const REFLECTION_TAIL_LIMIT: usize = 150;

/// The consolidation cursor for `scene`: `max(to_id)` over its episodes, or `None`
/// if the scene has no id-bearing episode yet (genesis). Legacy
/// `kind: heartbeat-briefing` episodes carry no `to_id` and so don't contribute —
/// the id-cursor starts fresh past them. uuidv7 ids are lexically time-sortable,
/// so a string max is a recency max.
pub async fn scene_cursor(data_dir: &Path, scene: &Scene) -> anyhow::Result<Option<String>> {
    let dir = layout::episodes_dir(data_dir);
    let mut rd = match tokio::fs::read_dir(&dir).await {
        Ok(rd) => rd,
        Err(err) if err.kind() == std::io::ErrorKind::NotFound => return Ok(None),
        Err(err) => return Err(err.into()),
    };
    let mut max: Option<String> = None;
    while let Some(ent) = rd.next_entry().await? {
        if !ent.file_type().await?.is_dir() {
            continue;
        }
        let content = match tokio::fs::read_to_string(ent.path().join("episode.md")).await {
            Ok(c) => c,
            Err(_) => continue,
        };
        if frontmatter_field(&content, "scene").as_deref() != Some(scene.0.as_str()) {
            continue;
        }
        if let Some(to_id) = frontmatter_field(&content, "to_id")
            && max.as_deref().is_none_or(|m| to_id.as_str() > m)
        {
            max = Some(to_id);
        }
    }
    Ok(max)
}

/// Record one episode as the first `count` signals of `scene`'s current
/// unconsolidated frontier (the signals after [`scene_cursor`], read via
/// [`journal::after_cursor`]). Resolves the `[from_id, to_id]` range from raw,
/// writes the bundle, and returns its ref (the directory name) so a facet can
/// cite it. The cursor then advances by exactly `count`, so a following call's
/// `count` is relative to the new frontier — the mind never names a raw id.
///
/// Errors (surfaced to the reflection session as a tool error) if the frontier is
/// empty or `count` is outside `1..=frontier_len`, so a miscount never writes a
/// degenerate episode.
pub async fn record_episode(
    data_dir: &Path,
    scene: &Scene,
    count: usize,
    title: &str,
    gist: &str,
    subjects: &[String],
) -> anyhow::Result<String> {
    if count == 0 {
        anyhow::bail!("count must be >= 1");
    }
    let cursor = scene_cursor(data_dir, scene).await?;
    let tail =
        journal::after_cursor(data_dir, scene, cursor.as_deref(), REFLECTION_TAIL_LIMIT).await?;
    if tail.is_empty() {
        anyhow::bail!("no unconsolidated signals to record");
    }
    if count > tail.len() {
        anyhow::bail!(
            "count {count} exceeds the {} unconsolidated signals on the frontier",
            tail.len()
        );
    }

    let covered = &tail[..count];
    let from_id = journal::entry_id(&covered[0]).to_string();
    let to_id = journal::entry_id(&covered[count - 1]).to_string();
    let from_ts = journal::entry_ts(&covered[0]);
    let to_ts = journal::entry_ts(&covered[count - 1]);

    // The dir name is `<date>-<slug>`, a human-readable handle the mind can scan.
    // The slug is the model's `title`, slugified; an empty/symbol-only title falls
    // back to the gist's opening words, and a still-empty result to a uuid tail so
    // a name always exists. Within one reflection round several episodes can share
    // a date and even a slug, and `create_dir_all` would silently reuse a colliding
    // dir (overwriting the prior episode), so we create the leaf exclusively and
    // append `-2`, `-3`, … until one is fresh.
    let base = {
        let s = slugify(title);
        let s = if s.is_empty() { slugify(gist) } else { s };
        if s.is_empty() {
            let short = Uuid::now_v7().simple().to_string();
            short[short.len() - 8..].to_string()
        } else {
            s
        }
    };
    let parent = layout::episodes_dir(data_dir);
    tokio::fs::create_dir_all(&parent).await?;
    let date = to_ts.format("%Y-%m-%d").to_string();
    let mut name = format!("{date}-{base}");
    let mut dir = parent.join(&name);
    let mut n = 2;
    loop {
        match tokio::fs::create_dir(&dir).await {
            Ok(()) => break,
            Err(err) if err.kind() == std::io::ErrorKind::AlreadyExists => {
                name = format!("{date}-{base}-{n}");
                dir = parent.join(&name);
                n += 1;
            }
            Err(err) => return Err(err.into()),
        }
    }

    // Frontmatter values are emitted as JSON (a subset of YAML), so a scene id,
    // signal id, or subject with a colon/quote/newline can never break the block.
    let body = format!(
        "---\nscene: {}\ntitle: {}\nfrom_id: {}\nto_id: {}\nfrom_ts: {}\nto_ts: {}\nsubjects: {}\nkind: reflection\n---\n\n{}\n",
        jstr(&scene.0),
        jstr(title.trim()),
        jstr(&from_id),
        jstr(&to_id),
        jstr(&from_ts.to_rfc3339()),
        jstr(&to_ts.to_rfc3339()),
        jarr(subjects),
        gist.trim(),
    );
    tokio::fs::write(dir.join("episode.md"), body).await?;
    Ok(name)
}

/// The gists (episode bodies, frontmatter stripped) of the most recent `limit`
/// episodes, oldest first. With `scene` set, only that scene's episodes count —
/// reflection uses this for continue-vs-new judgment; `hot.md` passes `None` for
/// the global working set. Empty if there are no matching episodes yet.
pub async fn recent_gists(
    memory: &Memory,
    scene: Option<&Scene>,
    limit: usize,
) -> anyhow::Result<Vec<String>> {
    let dir = layout::episodes_dir(memory.data_dir());
    let mut rd = match tokio::fs::read_dir(&dir).await {
        Ok(rd) => rd,
        Err(err) if err.kind() == std::io::ErrorKind::NotFound => return Ok(Vec::new()),
        Err(err) => return Err(err.into()),
    };
    let mut names: Vec<String> = Vec::new();
    while let Some(ent) = rd.next_entry().await? {
        if ent.file_type().await?.is_dir()
            && let Ok(name) = ent.file_name().into_string()
        {
            names.push(name);
        }
    }
    names.sort();

    // Walk newest-first so a scene filter keeps the most recent matches, then
    // restore oldest-first for the caller.
    let mut gists: Vec<String> = Vec::new();
    for name in names.iter().rev() {
        if gists.len() >= limit {
            break;
        }
        let content = match tokio::fs::read_to_string(dir.join(name).join("episode.md")).await {
            Ok(c) => c,
            Err(_) => continue,
        };
        if let Some(s) = scene
            && frontmatter_field(&content, "scene").as_deref() != Some(s.0.as_str())
        {
            continue;
        }
        gists.push(strip_frontmatter(&content).trim().to_owned());
    }
    gists.reverse();
    Ok(gists)
}

/// Episode dir-names for `scene` whose covered day-range intersects `day`
/// (`YYYY-MM-DD`), newest first — a hint the forgetting pass shows the mind: which
/// events a cold day held, so it can judge what's worth keeping. Best-effort; an
/// episode with no parseable `from_ts`/`to_ts` is skipped.
pub async fn names_overlapping_day(
    data_dir: &Path,
    scene: &Scene,
    day: &str,
) -> anyhow::Result<Vec<String>> {
    let dir = layout::episodes_dir(data_dir);
    let mut rd = match tokio::fs::read_dir(&dir).await {
        Ok(rd) => rd,
        Err(err) if err.kind() == std::io::ErrorKind::NotFound => return Ok(Vec::new()),
        Err(err) => return Err(err.into()),
    };
    let mut names = Vec::new();
    while let Some(ent) = rd.next_entry().await? {
        if !ent.file_type().await?.is_dir() {
            continue;
        }
        let content = match tokio::fs::read_to_string(ent.path().join("episode.md")).await {
            Ok(c) => c,
            Err(_) => continue,
        };
        if frontmatter_field(&content, "scene").as_deref() != Some(scene.0.as_str()) {
            continue;
        }
        let (Some(from), Some(to)) =
            (frontmatter_field(&content, "from_ts"), frontmatter_field(&content, "to_ts"))
        else {
            continue;
        };
        // The RFC3339 values lead with the date, so a 10-char prefix compare is a
        // date compare: include when from_date <= day <= to_date.
        let from_d = from.get(..10).unwrap_or("");
        let to_d = to.get(..10).unwrap_or("");
        if from_d <= day && day <= to_d
            && let Ok(name) = ent.file_name().into_string()
        {
            names.push(name);
        }
    }
    names.sort();
    names.reverse();
    Ok(names)
}

/// The start-date (`from_ts`, `YYYY-MM-DD`) of every episode in `scene`, sorted
/// ascending — the forgetting pass uses it to weigh a cold day's burial depth
/// (how many events began after it) with a single scan. Episodes without a
/// parseable `from_ts` are skipped.
pub async fn scene_from_dates(data_dir: &Path, scene: &Scene) -> anyhow::Result<Vec<String>> {
    let dir = layout::episodes_dir(data_dir);
    let mut rd = match tokio::fs::read_dir(&dir).await {
        Ok(rd) => rd,
        Err(err) if err.kind() == std::io::ErrorKind::NotFound => return Ok(Vec::new()),
        Err(err) => return Err(err.into()),
    };
    let mut dates = Vec::new();
    while let Some(ent) = rd.next_entry().await? {
        if !ent.file_type().await?.is_dir() {
            continue;
        }
        let content = match tokio::fs::read_to_string(ent.path().join("episode.md")).await {
            Ok(c) => c,
            Err(_) => continue,
        };
        if frontmatter_field(&content, "scene").as_deref() != Some(scene.0.as_str()) {
            continue;
        }
        if let Some(from) = frontmatter_field(&content, "from_ts")
            && let Some(d) = from.get(..10)
        {
            dates.push(d.to_owned());
        }
    }
    dates.sort();
    Ok(dates)
}

/// One frontmatter scalar by key, JSON-decoding a quoted value (so a colon inside
/// it survives) and returning a bare value as-is. `None` if there's no
/// frontmatter block or the key is absent. Splits on the first `:` so an RFC3339
/// value's own colons stay in the value.
fn frontmatter_field(content: &str, key: &str) -> Option<String> {
    let fm = content.strip_prefix("---\n")?;
    let block = &fm[..fm.find("\n---\n")?];
    for line in block.lines() {
        let (k, v) = line.split_once(':')?;
        if k.trim() != key {
            continue;
        }
        let v = v.trim();
        if v.starts_with('"')
            && let Ok(s) = serde_json::from_str::<String>(v)
        {
            return Some(s);
        }
        return Some(v.to_string());
    }
    None
}

/// Strip a leading `---\n…\n---\n` YAML frontmatter block, returning the body.
fn strip_frontmatter(content: &str) -> &str {
    let Some(rest) = content.strip_prefix("---\n") else {
        return content;
    };
    match rest.find("\n---\n") {
        Some(i) => &rest[i + "\n---\n".len()..],
        None => content,
    }
}

/// A filesystem-safe slug for an episode dir: lowercase ASCII alphanumerics, every
/// other run collapsed to a single `-`, trimmed of leading/trailing `-`, and capped
/// to a few words so the handle stays short. Non-ASCII (e.g. CJK) carries no ASCII
/// letters, so such a title slugs to empty and the caller falls back.
fn slugify(s: &str) -> String {
    let mut out = String::new();
    let mut words = 0;
    for ch in s.chars() {
        if ch.is_ascii_alphanumeric() {
            out.push(ch.to_ascii_lowercase());
        } else if !out.ends_with('-') && !out.is_empty() {
            out.push('-');
            words += 1;
            if words >= 6 {
                break;
            }
        }
    }
    let trimmed = out.trim_matches('-');
    trimmed.chars().take(60).collect()
}

/// A string as a JSON (⊂ YAML) scalar; falls back to an empty string literal.
fn jstr(s: &str) -> String {
    serde_json::to_string(s).unwrap_or_else(|_| "\"\"".into())
}

/// A string list as a JSON (⊂ YAML) flow sequence; falls back to `[]`.
fn jarr(v: &[String]) -> String {
    serde_json::to_string(v).unwrap_or_else(|_| "[]".into())
}

#[cfg(test)]
mod reflection_tests {
    use super::*;
    use crate::mind::memory::journal::Journal;
    use crate::types::{Channel, JournalEntry};
    use chrono::Utc;

    /// Append `n` text signals with strictly increasing uuidv7 ids (a 2ms gap puts
    /// each in its own millisecond, so `now_v7` stays monotonic). Returns the ids.
    async fn append(j: &Journal, scene: &Scene, n: usize) -> Vec<String> {
        let mut ids = Vec::new();
        for _ in 0..n {
            let id = Uuid::now_v7().to_string();
            j.append(JournalEntry::SignalIn {
                id: id.clone(),
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
            ids.push(id);
            tokio::time::sleep(std::time::Duration::from_millis(2)).await;
        }
        ids
    }

    #[tokio::test]
    async fn cursor_none_before_any_episode() {
        let dir = tempfile::tempdir().unwrap();
        assert_eq!(scene_cursor(dir.path(), &Scene("s".into())).await.unwrap(), None);
    }

    #[tokio::test]
    async fn record_advances_cursor_by_count() {
        let dir = tempfile::tempdir().unwrap();
        let j = Journal::open(dir.path().to_path_buf()).await.unwrap();
        let scene = Scene("s".into());
        let ids = append(&j, &scene, 5).await;

        let name =
            record_episode(dir.path(), &scene, 2, "Lunch with Alice", "first event", &["people/alice".into()])
                .await
                .unwrap();
        assert!(name.contains('-'));
        assert!(name.ends_with("-lunch-with-alice"));
        assert_eq!(
            scene_cursor(dir.path(), &scene).await.unwrap().as_deref(),
            Some(ids[1].as_str())
        );

        record_episode(dir.path(), &scene, 2, "Second thing", "second event", &[]).await.unwrap();
        assert_eq!(
            scene_cursor(dir.path(), &scene).await.unwrap().as_deref(),
            Some(ids[3].as_str())
        );

        let cursor = scene_cursor(dir.path(), &scene).await.unwrap();
        let tail = journal::after_cursor(dir.path(), &scene, cursor.as_deref(), REFLECTION_TAIL_LIMIT)
            .await
            .unwrap();
        assert_eq!(tail.len(), 1);

        // Two distinct episode dirs — distinct titles slug to distinct names. (Same
        // title in one round would still get a unique name via the `-2`/`-3` suffix.)
        let mut rd = tokio::fs::read_dir(layout::episodes_dir(dir.path())).await.unwrap();
        let mut dirs = 0;
        while rd.next_entry().await.unwrap().is_some() {
            dirs += 1;
        }
        assert_eq!(dirs, 2);
    }

    #[tokio::test]
    async fn record_rejects_out_of_range_count() {
        let dir = tempfile::tempdir().unwrap();
        let j = Journal::open(dir.path().to_path_buf()).await.unwrap();
        let scene = Scene("s".into());
        append(&j, &scene, 2).await;
        assert!(record_episode(dir.path(), &scene, 5, "Too many", "too many", &[]).await.is_err());
        assert!(record_episode(dir.path(), &scene, 0, "Zero", "zero", &[]).await.is_err());
    }

    #[tokio::test]
    async fn cursor_is_scene_scoped() {
        let dir = tempfile::tempdir().unwrap();
        let j = Journal::open(dir.path().to_path_buf()).await.unwrap();
        let a = Scene("a".into());
        let b = Scene("b".into());
        let a_ids = append(&j, &a, 2).await;
        append(&j, &b, 2).await;
        record_episode(dir.path(), &a, 2, "A event", "a event", &[]).await.unwrap();
        assert_eq!(
            scene_cursor(dir.path(), &a).await.unwrap().as_deref(),
            Some(a_ids[1].as_str())
        );
        assert_eq!(scene_cursor(dir.path(), &b).await.unwrap(), None);
    }
}
