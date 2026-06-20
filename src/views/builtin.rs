//! Bundled built-in views — seeded into the views tree on startup.
//!
//! Some views are platform "stdlib": basic, universal, the same for everyone (the
//! file-upload entry — a drag-drop zone + a phone QR). We ship their source in the
//! binary and write it into the views tree at boot, so the agent shows them with
//! `show_view` like any other view — and can still adapt them, since they land as
//! ordinary `.jsx` in the (disposable, re-seeded) tree. They live under
//! `_builtin/` so they never collide with the agent's own `<project>/` work.

use std::io;
use std::path::Path;

/// The file-handoff view shown when the user wants to hand the agent a file.
/// Ref: `_builtin/upload` (the agent puts it on screen via `show_view`).
const UPLOAD: &str = include_str!("builtin/upload.jsx");

/// Write the bundled built-in views into `<data_dir>/views/_builtin/`, overwriting
/// each on every boot so a binary update reseeds the latest (mirrors
/// [`crate::reactor::install_prompts`]). The views tree is disposable, so
/// re-seeding is the point, not a hazard.
pub fn install_builtin_views(data_dir: &Path) -> io::Result<()> {
    let dir = data_dir.join("views").join("_builtin");
    std::fs::create_dir_all(&dir)?;
    std::fs::write(dir.join("upload.jsx"), UPLOAD)?;
    Ok(())
}
