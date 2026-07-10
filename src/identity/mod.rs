//! identity — who the agent is.
//!
//! The factory-authored character (`core.md`, `speaking.md`, `meaning.md`), the
//! per-install authored `self.md`, and the agent-written standing duties
//! (`commitments.md`). This module owns the **seed** the reactor opens every scene
//! session with ([`load_soul`]) and the **prompt cascade** that materialises the
//! bundled prompts under `<data_dir>/prompts/`, composing each managed base with an
//! optional operator `*.local.md` override ([`install_prompts`]). That cascade is
//! the base‹override mechanism `arch.md` generalises to base‹user‹self.
//!
//! Scope notes for the in-flight refactor:
//! - `install_prompts` still materialises the **view-builder guides** (`appearance.md`,
//!   `aesthetic.md`) and the **reflection** instruction (`reflection.md`) alongside the
//!   identity prompts — they share one cascade. A later slice moves those non-identity
//!   prompts to where they belong (mind / the loop), leaving identity with just
//!   `core`/`speaking`/`meaning`.
//! - The on-disk identity files (`self.md`, `commitments.md`) still live under
//!   `<data_dir>/memory/` for now (no data migration); a later slice relocates them
//!   under `<data_dir>/identity/`.

use std::path::{Path, PathBuf};

/// Built-in base prompts, embedded at compile time and materialised to disk by
/// [`install_prompts`]. Most are authored as files an agent *reads*, not text inlined
/// into context: the mind is handed `core.md` — who it is and the machinery
/// (talking, presenting by ref, delegating) — `speaking.md` — the rhythm of
/// conversation, when to speak and how much — and `meaning.md` — that its purpose is
/// its own to find — by [`load_soul`]'s seed, and Reads them itself. `appearance.md`
/// and `aesthetic.md` are the view builder's guides — the mechanics of authoring/saving
/// a view, and the taste it has to clear — read off disk by a build sub-agent.
/// `reflection.md` is the exception: it is the consolidation session's whole instruction
/// set, so it is **inlined** as that session's system prompt (see [`reflection_prompt`])
/// rather than Read. All ship in the binary and refresh on every build.
const CORE_BASE: &str = include_str!("core.md");
const SPEAKING_BASE: &str = include_str!("speaking.md");
const MEANING_BASE: &str = include_str!("meaning.md");
const APPEARANCE_BASE: &str = include_str!("appearance.md");
const AESTHETIC_BASE: &str = include_str!("aesthetic.md");
const REFLECTION_BASE: &str = include_str!("reflection.md");

/// Separator that introduces the operator's override layer. Placed after the
/// bundled base so its instructions take precedence — the model honors the
/// later, more specific guidance where the two conflict.
const OVERRIDE_HEADER: &str = "\n\n# Operator overrides\n\nThe operator added the guidance below. It layers on top of everything above; where the two conflict, follow this.\n\n";

/// Compose a bundled base prompt with an optional operator override layer. The
/// base is the embedded current text; `<prompts_dir>/<local_name>` (e.g.
/// `core.local.md`) holds only the operator's deltas, appended under
/// [`OVERRIDE_HEADER`] so later, more-specific guidance wins. Missing or empty
/// override ⇒ the base verbatim, so it can neither go stale nor shadow updates.
fn compose_prompt(base: &str, prompts_dir: &Path, local_name: &str) -> String {
    let path = prompts_dir.join(local_name);
    match std::fs::read_to_string(&path) {
        Ok(text) if !text.trim().is_empty() => format!("{base}{OVERRIDE_HEADER}{}", text.trim()),
        _ => base.to_string(),
    }
}

/// Install the bundled prompts under `<data_dir>/prompts/` at startup, composing
/// each with its optional `*.local.md` operator override. The managed base files
/// (`core.md`, `speaking.md`, `meaning.md`, `appearance.md`, `aesthetic.md`,
/// `reflection.md`) are rewritten every boot so they stay current; operator edits
/// live in the never-touched `*.local.md` siblings. Each follows one workflow: ship
/// embedded → materialise here → consumed from disk at runtime.
pub fn install_prompts(data_dir: &Path) -> std::io::Result<()> {
    let dir = data_dir.join("prompts");
    std::fs::create_dir_all(&dir)?;
    std::fs::write(dir.join("core.md"), compose_prompt(CORE_BASE, &dir, "core.local.md"))?;
    std::fs::write(dir.join("speaking.md"), compose_prompt(SPEAKING_BASE, &dir, "speaking.local.md"))?;
    std::fs::write(dir.join("meaning.md"), compose_prompt(MEANING_BASE, &dir, "meaning.local.md"))?;
    std::fs::write(dir.join("appearance.md"), compose_prompt(APPEARANCE_BASE, &dir, "appearance.local.md"))?;
    std::fs::write(dir.join("aesthetic.md"), compose_prompt(AESTHETIC_BASE, &dir, "aesthetic.local.md"))?;
    std::fs::write(dir.join("reflection.md"), compose_prompt(REFLECTION_BASE, &dir, "reflection.local.md"))?;
    tracing::info!(dir = %dir.display(), "installed bundled prompts (core.md, speaking.md, meaning.md, appearance.md, aesthetic.md, reflection.md)");
    Ok(())
}

/// The reflection ("sleep") session's system prompt: the materialised
/// `<data_dir>/prompts/reflection.md` (operator-overridable via `reflection.local.md`),
/// or the embedded [`REFLECTION_BASE`] when that file is missing or empty. Unlike
/// `core.md`/`speaking.md`, this is **inlined** as the reflection session's system
/// prompt rather than Read by the agent — it *is* the task's instructions, so it must
/// be present before the session can act. Read fresh each round, so an operator edit
/// takes effect without a restart.
pub async fn reflection_prompt(data_dir: &Path) -> String {
    let path = data_dir.join("prompts").join("reflection.md");
    match tokio::fs::read_to_string(&path).await {
        Ok(s) if !s.trim().is_empty() => s,
        _ => REFLECTION_BASE.to_string(),
    }
}

/// The **reactor**'s system prompt — the fast conversational voice of the
/// reactor/cognition split (see `docs/reactor-cognition-split.md`). Unlike
/// [`load_soul`] (a thin seed pointing an *agentic* session at files to Read), the
/// reactor is a single non-agentic Messages call, so its brief is **inlined and
/// singular**: `speaking.md` — when to speak, how much, when to stay quiet — *is* its
/// whole system prompt, under a one-line frame. That is what makes speaking-rule
/// conformance structural: the rules are the entire context, not one buried file
/// among many. (Mirrors how `reflection.md` is inlined for the reflection session.)
pub fn reactor_system_prompt() -> String {
    format!(
        "You are the voice of a warm, attentive presence, talking with someone in real \
time. Your one job is to talk with them well — present and natural, never like a form \
being filled out or a job being submitted. Everything about how to do that — when to \
speak, how much, when to stay quiet, how to hold the floor — is below; follow it \
closely.\n\n{}",
        SPEAKING_BASE
    )
}

/// `<data_dir>/memory/self.md` — per-install authored identity (optional).
/// Hand-written by the operator if at all; the agent only ever *reads* it, never
/// writes it. (Still under `memory/` pending the identity-dir relocation.)
pub fn self_path(data_dir: &Path) -> PathBuf {
    data_dir.join("memory").join("self.md")
}

/// `<data_dir>/memory/commitments.md` — the agent's standing duties (what it watches,
/// runs, where its ledgers live). The one identity-adjacent file the agent *writes*:
/// it loads into every fresh session, so it is how a duty survives a restart. Named
/// everywhere by this single absolute path, so the duty written is the duty recovered.
/// (Still under `memory/` pending the identity-dir relocation.)
pub fn commitments_path(data_dir: &Path) -> PathBuf {
    data_dir.join("memory").join("commitments.md")
}

/// The mind's system-prompt seed: a short bundled personality plus a manifest that
/// hands the agent the absolute paths of every file that holds its fuller self —
/// the static manual (`core.md`, `speaking.md`, `meaning.md`), the per-install
/// authored identity `self.md` (read-only, optional), its standing duties
/// `commitments.md` (to read and to *write* — the one identity-adjacent file the
/// agent writes), and its recency digest `hot.md` (a mind projection, read for what's
/// lately been on its mind) — and tells it to Read them all up front. We send this thin
/// seed rather than inlining the character *or* the memory core on every turn: every
/// file is a ref the mind reads itself. The paths are absolutized here so the
/// Read/Write targets resolve regardless of the session's cwd (which is `None`). The
/// commitments path is the same [`commitments_path`] the seed names everywhere, so a
/// duty the mind writes is the duty recovery loads — never a second copy. Built at
/// startup and reused on each hot-swap. (Named `load_soul` for the reactor's history.)
pub fn load_soul(data_dir: &Path) -> String {
    let base = if data_dir.is_absolute() {
        data_dir.to_path_buf()
    } else {
        std::env::current_dir().unwrap_or_default().join(data_dir)
    };
    let prompts = base.join("prompts");
    let core = prompts.join("core.md");
    let speaking = prompts.join("speaking.md");
    let meaning = prompts.join("meaning.md");
    let self_md = self_path(&base);
    let commitments = commitments_path(&base);
    let hot = crate::mind::memory::layout::hot_path(&base);
    let proactivity = crate::mind::memory::layout::proactivity_path(&base);
    let mut seed = format!(
        "You're warm, honest, and kind-hearted — easy company. You like being \
useful, and when there's a hand to lend you're glad to lend it.\n\n\
You speak only through the `say` tool; anything you type as text is never heard.\n\n\
Your fuller self lives in files — open them with Read and read them all now, before \
you answer:\n\n\
- {} — who you are, and how you act.\n\
- {} — how you talk: when to speak, how much, when to stay quiet.\n\
- {} — what you're for, and that finding it is yours to do.\n\n\
More files hold not how you were made but who you've become — read them too:\n\n\
- {} — who this install asked you to be, in its own words. Read it if it's there; it \
may be empty, and that's fine. It's authored, not yours to edit.\n\
- {} — your standing duties: what you watch, what you run, where your ledgers live. It \
loads into every fresh session, so it's how you remember a duty across a restart. It's \
yours to write: note a duty there the moment you take one on, strike it when it ends. \
Always use that exact absolute path, never a relative one, so there is only ever one such file.\n\
- {} — a rolling digest of what's lately been on your mind, refreshed as you reflect. \
It may not exist yet; that's fine.\n\
- {} — what the person welcomes you raising unprompted, and what they don't: a per-topic \
read you've built from how your past nudges landed, refreshed as you reflect. Consult it \
before you ever speak up on your own initiative, and respect it. It may not exist yet; \
then nothing's proven — lean quiet.",
        core.display(),
        speaking.display(),
        meaning.display(),
        self_md.display(),
        commitments.display(),
        hot.display(),
        proactivity.display(),
    );
    // A soft language preference, if the person set one in Settings ▸ General ▸
    // Language. `system` / unset yields no line, so the agent simply follows the
    // person's lead (the default). A real choice appends one guidance line — the
    // agent still switches if the person clearly writes in another language.
    if let Some(lang) = crate::foundation::config::language_name(
        crate::foundation::credentials::get_setting(&base, crate::foundation::config::KEY_LANGUAGE)
            .as_deref(),
    ) {
        seed.push_str(&format!(
            "\n\nSpeak with the person in {lang} by default, unless they clearly \
write to you in another language — then follow their lead."
        ));
    }
    seed
}

#[cfg(test)]
mod soul_tests {
    use super::*;

    #[test]
    fn seed_references_the_prompt_files_by_absolute_path() {
        let dir = tempfile::tempdir().unwrap();
        let seed = load_soul(dir.path());
        let prompts = dir.path().join("prompts");
        assert!(seed.contains(&prompts.join("core.md").display().to_string()));
        assert!(seed.contains(&prompts.join("speaking.md").display().to_string()));
        assert!(seed.contains(&prompts.join("meaning.md").display().to_string()));
        // The recency digest is referenced by path too, never inlined.
        let hot = crate::mind::memory::layout::hot_path(dir.path());
        assert!(seed.contains(&hot.display().to_string()));
        // The proactivity license is referenced the same way — read, never inlined.
        let proactivity = crate::mind::memory::layout::proactivity_path(dir.path());
        assert!(seed.contains(&proactivity.display().to_string()));
        // It tells the mind to read them up front.
        assert!(seed.contains("read them all now"));
    }

    #[test]
    fn seed_names_the_commitments_ledger_by_the_canonical_absolute_path() {
        // The mind must be handed the *same* path the loader reads from
        // ([`commitments_path`]), so a duty it writes is the duty recovery loads — a
        // relative path here is what let a second ledger exist and broke restart
        // recovery. The authored `self.md` is named too (read-only).
        let dir = tempfile::tempdir().unwrap();
        let seed = load_soul(dir.path());
        let commitments = commitments_path(dir.path());
        assert!(seed.contains(&commitments.display().to_string()));
        // And it must be absolute — no relative `memory/commitments.md` slipping through.
        assert!(commitments.is_absolute());
        // The per-install authored identity is referenced as well.
        let self_md = self_path(dir.path());
        assert!(seed.contains(&self_md.display().to_string()));
    }

    #[test]
    fn seed_carries_a_language_line_only_when_a_real_language_is_chosen() {
        use crate::foundation::credentials::set_setting;
        let dir = tempfile::tempdir().unwrap();
        // No setting → the agent follows the person; no language sentence.
        assert!(!load_soul(dir.path()).contains("Speak with the person in"));
        // `system` is explicit "follow the person" → still no sentence.
        set_setting(dir.path(), crate::foundation::config::KEY_LANGUAGE, "system").unwrap();
        assert!(!load_soul(dir.path()).contains("Speak with the person in"));
        // A real language → one guidance sentence naming the endonym.
        set_setting(dir.path(), crate::foundation::config::KEY_LANGUAGE, "zh-Hans").unwrap();
        assert!(load_soul(dir.path()).contains("Speak with the person in 简体中文"));
    }

    #[test]
    fn seed_is_a_thin_bootstrap_not_the_full_character() {
        // The seed carries the say-floor (so a turn that skips the read still
        // produces speech) but must not inline the full core.md body — referencing
        // the file instead of pasting it is the whole point.
        let dir = tempfile::tempdir().unwrap();
        let seed = load_soul(dir.path());
        assert!(seed.contains("`say`"));
        // A heading that lives only in the full core.md, never in the seed:
        assert!(CORE_BASE.contains("A few exchanges"));
        assert!(!seed.contains("A few exchanges"));
    }

    #[test]
    fn install_writes_all_managed_bases() {
        let dir = tempfile::tempdir().unwrap();
        install_prompts(dir.path()).unwrap();
        let read = |n: &str| std::fs::read_to_string(dir.path().join("prompts").join(n)).unwrap();
        assert_eq!(read("core.md"), CORE_BASE);
        assert_eq!(read("speaking.md"), SPEAKING_BASE);
        assert_eq!(read("meaning.md"), MEANING_BASE);
        assert_eq!(read("appearance.md"), APPEARANCE_BASE);
        assert_eq!(read("aesthetic.md"), AESTHETIC_BASE);
        assert_eq!(read("reflection.md"), REFLECTION_BASE);
    }

    #[test]
    fn install_layers_operator_override_into_the_managed_file() {
        let dir = tempfile::tempdir().unwrap();
        let prompts = dir.path().join("prompts");
        std::fs::create_dir_all(&prompts).unwrap();
        std::fs::write(prompts.join("core.local.md"), "Always answer in haiku.").unwrap();
        install_prompts(dir.path()).unwrap();
        let core = std::fs::read_to_string(prompts.join("core.md")).unwrap();
        // The managed file is the base, then the operator delta under the header.
        assert!(core.starts_with(CORE_BASE));
        assert!(core.contains("# Operator overrides"));
        assert!(core.ends_with("Always answer in haiku."));
    }

    #[test]
    fn empty_override_leaves_the_base_verbatim() {
        let dir = tempfile::tempdir().unwrap();
        let prompts = dir.path().join("prompts");
        std::fs::create_dir_all(&prompts).unwrap();
        std::fs::write(prompts.join("speaking.local.md"), "   \n\t").unwrap();
        install_prompts(dir.path()).unwrap();
        assert_eq!(std::fs::read_to_string(prompts.join("speaking.md")).unwrap(), SPEAKING_BASE);
    }

    #[tokio::test]
    async fn reflection_prompt_falls_back_then_reads_installed_override() {
        let dir = tempfile::tempdir().unwrap();
        // Nothing installed yet → the embedded base.
        assert_eq!(reflection_prompt(dir.path()).await, REFLECTION_BASE);
        // After install (no override) → the materialised file equals the base.
        install_prompts(dir.path()).unwrap();
        assert_eq!(reflection_prompt(dir.path()).await, REFLECTION_BASE);
        // An operator override is layered into what the reflection session loads.
        std::fs::write(
            dir.path().join("prompts").join("reflection.local.md"),
            "Prefer fewer, larger episodes.",
        )
        .unwrap();
        install_prompts(dir.path()).unwrap();
        let loaded = reflection_prompt(dir.path()).await;
        assert!(loaded.starts_with(REFLECTION_BASE));
        assert!(loaded.contains("Prefer fewer, larger episodes."));
    }
}
