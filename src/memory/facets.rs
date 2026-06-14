//! Derived current-understanding — `memory/facets/<dim>/<subject>.md`.
//!
//! A facet is the agent's best current understanding of one subject (a person, a
//! place, a project, a cultural topic), **regenerated from episodes** at
//! reflection time with every claim citing the episodes it came from. Like
//! episodes it is a disposable projection over the raw log — never a source of
//! truth.
//!
//! Dimensions are open-ended: `people`/`locations`/`projects`/`culture` are seeds,
//! not an enum. The mind supplies the dimension and subject; this module only does
//! the IO and keeps the on-disk name path-safe ([`slug`]).
//!
//! ## Concurrency
//!
//! Facets are **global**, not per-scene — two scenes can both touch
//! `people/alice.md`. A read-modify-write across scenes is **last-writer-wins, and
//! that is fine**: a facet is a regenerable cache whose truth lives in the
//! episodes, so the next reflection re-derives whatever a racing write dropped.
//! The only mechanical guarantee needed is that a reader never sees a half-written
//! file, so [`update_facet`] writes to a temp sibling and atomically renames.

use std::path::{Path, PathBuf};

use uuid::Uuid;

use super::layout;

/// The current understanding of `dim`/`subject`, or `None` if nothing has been
/// written for it yet. Used by reflection to regenerate (read the old, fold in new
/// episodes, write the whole file back).
pub async fn read_facet(
    data_dir: &Path,
    dim: &str,
    subject: &str,
) -> anyhow::Result<Option<String>> {
    match tokio::fs::read_to_string(facet_path(data_dir, dim, subject)).await {
        Ok(s) => Ok(Some(s)),
        Err(err) if err.kind() == std::io::ErrorKind::NotFound => Ok(None),
        Err(err) => Err(err.into()),
    }
}

/// Write `content` as the whole facet for `dim`/`subject` (regenerate, don't
/// patch). Returns the canonical `<dim>/<subject>` ref (post-[`slug`]) so the
/// caller can confirm where it landed. Atomic: a temp sibling is renamed into
/// place, so a concurrent reader sees either the old file or the new, never a torn
/// one. Errors if the dimension or subject slugs to nothing.
pub async fn update_facet(
    data_dir: &Path,
    dim: &str,
    subject: &str,
    content: &str,
) -> anyhow::Result<String> {
    let dim_s = slug(dim);
    let subj_s = slug(subject);
    if dim_s.is_empty() || subj_s.is_empty() {
        anyhow::bail!("dimension and subject must each contain a usable character");
    }
    let dir = layout::facets_dir(data_dir).join(&dim_s);
    tokio::fs::create_dir_all(&dir).await?;
    let path = dir.join(format!("{subj_s}.md"));
    let tmp = dir.join(format!(".{subj_s}.md.tmp-{}", Uuid::now_v7().simple()));
    tokio::fs::write(&tmp, content).await?;
    tokio::fs::rename(&tmp, &path).await?;
    Ok(format!("{dim_s}/{subj_s}"))
}

/// Every facet that exists, as `<dim>/<subject>` refs, sorted. Seeded into the
/// reflection prompt so the mind reuses an existing subject instead of spawning a
/// near-duplicate under a slightly different name. Empty before any facet exists.
pub async fn facet_subject_index(data_dir: &Path) -> anyhow::Result<Vec<String>> {
    let root = layout::facets_dir(data_dir);
    let mut dims = match tokio::fs::read_dir(&root).await {
        Ok(rd) => rd,
        Err(err) if err.kind() == std::io::ErrorKind::NotFound => return Ok(Vec::new()),
        Err(err) => return Err(err.into()),
    };
    let mut out = Vec::new();
    while let Some(dim_ent) = dims.next_entry().await? {
        if !dim_ent.file_type().await?.is_dir() {
            continue;
        }
        let Ok(dim_name) = dim_ent.file_name().into_string() else {
            continue;
        };
        let mut subs = tokio::fs::read_dir(dim_ent.path()).await?;
        while let Some(s) = subs.next_entry().await? {
            if let Ok(fname) = s.file_name().into_string()
                && let Some(stem) = fname.strip_suffix(".md")
                && !stem.is_empty()
            {
                out.push(format!("{dim_name}/{stem}"));
            }
        }
    }
    out.sort();
    Ok(out)
}

fn facet_path(data_dir: &Path, dim: &str, subject: &str) -> PathBuf {
    layout::facets_dir(data_dir)
        .join(slug(dim))
        .join(format!("{}.md", slug(subject)))
}

/// A path-safe, human-readable slug for one name segment: lowercase, runs of
/// whitespace/separators collapse to a single `-`, and anything that could break
/// out of a single path component (slashes, colons, quotes, control chars) is
/// dropped. Unicode letters/digits are KEPT (a personal DB has Chinese subjects),
/// so this is not ASCII-only. Leading/trailing `.`/`-` are trimmed, so a slug is
/// never a hidden file, a traversal (`.`/`..`), or dash-padded. Idempotent —
/// `slug(slug(x)) == slug(x)` — so a ref round-trips through the subject index
/// unchanged.
fn slug(s: &str) -> String {
    let mut out = String::new();
    let mut prev_dash = false;
    for ch in s.trim().chars() {
        if ch.is_alphanumeric() || ch == '.' || ch == '_' {
            out.extend(ch.to_lowercase());
            prev_dash = false;
        } else if (ch == '-' || ch.is_whitespace()) && !prev_dash && !out.is_empty() {
            out.push('-');
            prev_dash = true;
        }
        // everything else (/, \, :, *, ?, ", <, >, |, control, most punctuation) is dropped
    }
    // Trim leading/trailing `.`/`-` so a slug is never `.`/`..`, a hidden file, or
    // dash-padded. (A `.` mid-name — `v1.0` — is fine.)
    let lo = out.find(|c| c != '.' && c != '-').unwrap_or(out.len());
    out.drain(..lo);
    while out.ends_with('.') || out.ends_with('-') {
        out.pop();
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn read_missing_is_none() {
        let dir = tempfile::tempdir().unwrap();
        assert!(read_facet(dir.path(), "people", "alice").await.unwrap().is_none());
    }

    #[tokio::test]
    async fn write_then_read_roundtrips() {
        let dir = tempfile::tempdir().unwrap();
        let r = update_facet(dir.path(), "people", "Alice", "She likes tea. [ep 2026-06-13-aa]")
            .await
            .unwrap();
        assert_eq!(r, "people/alice");
        let got = read_facet(dir.path(), "people", "Alice").await.unwrap();
        assert_eq!(got.as_deref(), Some("She likes tea. [ep 2026-06-13-aa]"));
    }

    #[tokio::test]
    async fn slug_is_path_safe_unicode_and_idempotent() {
        assert_eq!(slug("Kyoto Trip"), "kyoto-trip");
        assert_eq!(slug("a/b:c"), "abc");
        assert_eq!(slug("../etc/passwd"), "etcpasswd");
        assert_eq!(slug(".."), "");
        assert_eq!(slug("  spaced  out  "), "spaced-out");
        assert_eq!(slug("小明"), "小明");
        let once = slug("Weird:: Name //x");
        assert_eq!(slug(&once), once);
    }

    #[tokio::test]
    async fn empty_subject_is_rejected() {
        let dir = tempfile::tempdir().unwrap();
        assert!(update_facet(dir.path(), "people", "??", "x").await.is_err());
        assert!(update_facet(dir.path(), "", "alice", "x").await.is_err());
    }

    #[tokio::test]
    async fn index_lists_written_subjects_sorted() {
        let dir = tempfile::tempdir().unwrap();
        update_facet(dir.path(), "people", "Bob", "x").await.unwrap();
        update_facet(dir.path(), "people", "Alice", "x").await.unwrap();
        update_facet(dir.path(), "projects", "Kyoto Trip", "x").await.unwrap();
        let idx = facet_subject_index(dir.path()).await.unwrap();
        assert_eq!(idx, vec!["people/alice", "people/bob", "projects/kyoto-trip"]);
    }
}
