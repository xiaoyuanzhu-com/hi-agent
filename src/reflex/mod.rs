//! Reflex fast-path — taught quick-actions the user fires instantly, with no
//! model in the loop.
//!
//! A reflex is a single grooved move: "the field labelled `身份证号` on this page
//! gets my ID `…`". The mind (the cortex) *authors* one via the `record_reflex`
//! tool when the user teaches it; the fast path (the cerebellum) *runs* it on an
//! explicit invoke — recognize the field, click it, type the value — without ever
//! waking the LLM.
//!
//! Recognition reuses the [`accessibility`](crate::capabilities::accessibility)
//! tree as the field-level signal and [`desktop_context`](crate::capabilities::desktop_context)
//! as the coarse app/window gate; the effect reuses [`input`](crate::capabilities::input).
//! There is no new perception or actuation here — a reflex is just a stored
//! [`Trigger`] + value composed over capabilities that already exist.
//!
//! ## Abstain on doubt
//!
//! The fast path is deliberately conservative: it fires only when exactly one
//! taught reflex matches the current window *and* exactly one on-screen field
//! matches that reflex. Zero matches, two reflexes, or two candidate fields all
//! resolve to [`Recognition::Abstain`] — the user just does it by hand, as today.
//! A false negative is harmless; a false positive types an ID into the wrong box,
//! so the matcher is tuned to never fire when unsure. The matching logic is a pure
//! function over [`Element`]s so it stays unit-testable off-macOS.

use std::path::Path;

use serde::{Deserialize, Serialize};
use uuid::Uuid;

use crate::capabilities::accessibility::Element;
use crate::memory::layout;

/// The AX role a reflex targets when its trigger doesn't pin one — a text field is
/// the overwhelmingly common fill target.
const DEFAULT_ROLE: &str = "AXTextField";

/// One taught quick-action: how to recognize the situation, and what to type.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Reflex {
    /// Stable id and on-disk filename stem — the slug of `name`.
    pub id: String,
    /// Human handle for the reflex (e.g. "fill my ID").
    pub name: String,
    /// How to recognize where this reflex applies.
    pub trigger: Trigger,
    /// Exactly what to type once the field is focused. Stored verbatim (plaintext,
    /// like the rest of the life-DB) — never echoed in responses or logs.
    pub value: String,
}

/// How a reflex recognizes its moment: a coarse window gate (`app` / `title_contains`,
/// matched against `desktop_context`) and the field to target (`role` + `label_contains`,
/// matched against the accessibility tree). All string matching is case-insensitive
/// substring, except `role`, which is matched whole (case-insensitively).
#[derive(Debug, Clone, PartialEq, Default, Serialize, Deserialize)]
pub struct Trigger {
    /// Frontmost app to require (substring, e.g. "Safari"). `None` = any app.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub app: Option<String>,
    /// Frontmost window-title substring to require. `None` = any title.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub title_contains: Option<String>,
    /// AX role of the target control. `None` = [`DEFAULT_ROLE`] (`AXTextField`).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub role: Option<String>,
    /// Substring of the target field's on-screen label (e.g. "ID number", "身份证").
    pub label_contains: String,
}

/// The outcome of [`recognize`]: either one unambiguous (reflex, target) to fire,
/// or an abstention carrying a human-readable reason (surfaced by the invoke route).
#[derive(Debug, Clone, PartialEq)]
pub enum Recognition {
    Fire { reflex: Reflex, target: Element },
    Abstain(String),
}

/// The on-disk id (and filename stem) for a reflex of this name — a path-safe slug.
/// Empty if `name` has no usable character.
pub fn id_for(name: &str) -> String {
    crate::memory::facets::slug(name)
}

/// Whether the coarse window gate passes for the current desktop context.
fn context_matches(t: &Trigger, frontmost_app: Option<&str>, window_title: Option<&str>) -> bool {
    let contains = |needle: &Option<String>, hay: Option<&str>| match needle {
        None => true,
        Some(n) => hay
            .map(|h| h.to_lowercase().contains(&n.to_lowercase()))
            .unwrap_or(false),
    };
    contains(&t.app, frontmost_app) && contains(&t.title_contains, window_title)
}

/// The elements a trigger targets: role matches (default `AXTextField`) and the
/// label contains the trigger's substring (case-insensitive).
fn matching_elements<'a>(t: &Trigger, elements: &'a [Element]) -> Vec<&'a Element> {
    let want_role = t.role.as_deref().unwrap_or(DEFAULT_ROLE);
    let needle = t.label_contains.to_lowercase();
    elements
        .iter()
        .filter(|e| {
            e.role.eq_ignore_ascii_case(want_role)
                && e
                    .label
                    .as_deref()
                    .map(|l| l.to_lowercase().contains(&needle))
                    .unwrap_or(false)
        })
        .collect()
}

/// Decide what to fire for the current moment, or abstain. Conservative on both
/// axes: exactly one taught reflex must match the window, and exactly one field
/// must match that reflex. Anything else abstains with a reason.
pub fn recognize(
    reflexes: &[Reflex],
    frontmost_app: Option<&str>,
    window_title: Option<&str>,
    elements: &[Element],
) -> Recognition {
    let candidates: Vec<&Reflex> = reflexes
        .iter()
        .filter(|r| context_matches(&r.trigger, frontmost_app, window_title))
        .collect();
    let reflex = match candidates.as_slice() {
        [] => return Recognition::Abstain("no taught reflex matches the current app/window".into()),
        [one] => *one,
        many => {
            return Recognition::Abstain(format!(
                "{} taught reflexes match this window — ambiguous, not firing",
                many.len()
            ));
        }
    };
    match matching_elements(&reflex.trigger, elements).as_slice() {
        [] => Recognition::Abstain(format!(
            "reflex '{}' matches this window but no field labelled '{}' is on screen",
            reflex.name, reflex.trigger.label_contains
        )),
        [one] => Recognition::Fire {
            reflex: reflex.clone(),
            target: (*one).clone(),
        },
        many => Recognition::Abstain(format!(
            "{} fields match '{}' — ambiguous, not firing",
            many.len(),
            reflex.trigger.label_contains
        )),
    }
}

/// Fire a recognized reflex: click the target field's centre to focus it, then type
/// the value. The element's bounds are 0..1 fractions of the main display (the same
/// space `act` uses), so the centre maps straight to a display point. macOS-only at
/// runtime via [`input`](crate::capabilities::input); errors cleanly elsewhere.
pub async fn fire(reflex: &Reflex, target: &Element) -> anyhow::Result<()> {
    use crate::capabilities::input::{self, Action, Point};
    let (w, h) = input::main_display_point_size()?;
    let center = Point {
        x: (target.bounds.x + target.bounds.w / 2.0) * w,
        y: (target.bounds.y + target.bounds.h / 2.0) * h,
    };
    input::perform(Action::Click(center)).await?;
    input::perform(Action::Type(reflex.value.clone())).await?;
    Ok(())
}

/// Persist a reflex as `<memory>/reflexes/<id>.json`, atomically (temp sibling +
/// rename) so a concurrent reader never sees a torn file. Re-teaching the same
/// `name` overwrites in place (same id). Returns the id.
pub async fn save(data_dir: &Path, reflex: &Reflex) -> anyhow::Result<String> {
    if reflex.id.is_empty() {
        anyhow::bail!("reflex id (slug of name) must contain a usable character");
    }
    let dir = layout::reflexes_dir(data_dir);
    tokio::fs::create_dir_all(&dir).await?;
    let path = dir.join(format!("{}.json", reflex.id));
    let tmp = dir.join(format!(".{}.json.tmp-{}", reflex.id, Uuid::now_v7().simple()));
    tokio::fs::write(&tmp, serde_json::to_vec_pretty(reflex)?).await?;
    tokio::fs::rename(&tmp, &path).await?;
    Ok(reflex.id.clone())
}

/// Load every taught reflex. Missing dir → empty. A single unreadable/!malformed
/// file is skipped (logged), never failing the whole load — one bad record must
/// not disable every other reflex.
pub async fn load_all(data_dir: &Path) -> anyhow::Result<Vec<Reflex>> {
    let dir = layout::reflexes_dir(data_dir);
    let mut rd = match tokio::fs::read_dir(&dir).await {
        Ok(rd) => rd,
        Err(err) if err.kind() == std::io::ErrorKind::NotFound => return Ok(Vec::new()),
        Err(err) => return Err(err.into()),
    };
    let mut out = Vec::new();
    while let Some(ent) = rd.next_entry().await? {
        let name = ent.file_name();
        let name = name.to_string_lossy();
        // Skip temp/hidden writes and non-json files.
        if name.starts_with('.') || !name.ends_with(".json") {
            continue;
        }
        match tokio::fs::read(ent.path()).await {
            Ok(bytes) => match serde_json::from_slice::<Reflex>(&bytes) {
                Ok(r) => out.push(r),
                Err(err) => {
                    tracing::warn!(file = %name, error = %err, "skipping malformed reflex record")
                }
            },
            Err(err) => tracing::warn!(file = %name, error = %err, "skipping unreadable reflex record"),
        }
    }
    Ok(out)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::capabilities::accessibility::Rect;

    fn el(id: usize, role: &str, label: Option<&str>) -> Element {
        Element {
            id,
            role: role.to_string(),
            label: label.map(str::to_string),
            value: None,
            bounds: Rect { x: 0.5, y: 0.5, w: 0.1, h: 0.05 },
        }
    }

    fn id_reflex() -> Reflex {
        Reflex {
            id: id_for("fill my ID"),
            name: "fill my ID".into(),
            trigger: Trigger {
                app: Some("Safari".into()),
                title_contains: None,
                role: None,
                label_contains: "身份证".into(),
            },
            value: "11010119900307xxxx".into(),
        }
    }

    #[test]
    fn id_for_is_slug() {
        assert_eq!(id_for("Fill My ID"), "fill-my-id");
        assert!(id_for("///").is_empty());
    }

    #[test]
    fn fires_on_single_reflex_and_single_field() {
        let reflexes = vec![id_reflex()];
        let elements = vec![el(0, "AXButton", Some("Submit")), el(1, "AXTextField", Some("身份证号"))];
        match recognize(&reflexes, Some("Safari"), Some("Sign up"), &elements) {
            Recognition::Fire { reflex, target } => {
                assert_eq!(reflex.name, "fill my ID");
                assert_eq!(target.id, 1);
            }
            other => panic!("expected fire, got {other:?}"),
        }
    }

    #[test]
    fn abstains_when_app_gate_fails() {
        let reflexes = vec![id_reflex()];
        let elements = vec![el(1, "AXTextField", Some("身份证号"))];
        // Right field, wrong app → abstain.
        assert!(matches!(
            recognize(&reflexes, Some("Notes"), None, &elements),
            Recognition::Abstain(_)
        ));
    }

    #[test]
    fn abstains_when_field_absent() {
        let reflexes = vec![id_reflex()];
        let elements = vec![el(0, "AXTextField", Some("email"))];
        assert!(matches!(
            recognize(&reflexes, Some("Safari"), None, &elements),
            Recognition::Abstain(_)
        ));
    }

    #[test]
    fn abstains_on_ambiguous_fields() {
        let reflexes = vec![id_reflex()];
        let elements = vec![
            el(0, "AXTextField", Some("身份证号")),
            el(1, "AXTextField", Some("配偶身份证号")),
        ];
        assert!(matches!(
            recognize(&reflexes, Some("Safari"), None, &elements),
            Recognition::Abstain(_)
        ));
    }

    #[test]
    fn abstains_on_ambiguous_reflexes() {
        let mut a = id_reflex();
        a.id = id_for("a");
        a.name = "a".into();
        a.trigger.label_contains = "name".into();
        let mut b = id_reflex();
        b.id = id_for("b");
        b.name = "b".into();
        b.trigger.label_contains = "email".into();
        // Both gate only on app=Safari, so both match this window → ambiguous.
        let reflexes = vec![a, b];
        let elements = vec![el(0, "AXTextField", Some("name")), el(1, "AXTextField", Some("email"))];
        assert!(matches!(
            recognize(&reflexes, Some("Safari"), None, &elements),
            Recognition::Abstain(_)
        ));
    }

    #[test]
    fn role_defaults_to_text_field_and_label_is_case_insensitive() {
        let t = Trigger { label_contains: "ID Number".into(), ..Default::default() };
        let elements = vec![
            el(0, "AXStaticText", Some("ID number")), // wrong role
            el(1, "AXTextField", Some("Your ID NUMBER here")), // right role, case-insensitive substring
        ];
        let got = matching_elements(&t, &elements);
        assert_eq!(got.len(), 1);
        assert_eq!(got[0].id, 1);
    }

    #[tokio::test]
    async fn save_then_load_roundtrips() {
        let dir = tempfile::tempdir().unwrap();
        let r = id_reflex();
        save(dir.path(), &r).await.unwrap();
        let loaded = load_all(dir.path()).await.unwrap();
        assert_eq!(loaded, vec![r]);
    }

    #[tokio::test]
    async fn load_missing_dir_is_empty() {
        let dir = tempfile::tempdir().unwrap();
        assert!(load_all(dir.path()).await.unwrap().is_empty());
    }

    #[tokio::test]
    async fn reteaching_same_name_overwrites() {
        let dir = tempfile::tempdir().unwrap();
        let mut r = id_reflex();
        save(dir.path(), &r).await.unwrap();
        r.value = "newvalue".into();
        save(dir.path(), &r).await.unwrap();
        let loaded = load_all(dir.path()).await.unwrap();
        assert_eq!(loaded.len(), 1);
        assert_eq!(loaded[0].value, "newvalue");
    }
}
