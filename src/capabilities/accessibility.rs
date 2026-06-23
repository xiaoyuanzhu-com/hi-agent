//! Accessibility (AX) capability — read the structured UI tree of the frontmost
//! app: the labelled, addressable controls behind what [`super::desktop_context`]
//! sees as flat pixels.
//!
//! This is the optional *accelerator* over the vision baseline. Where the screen
//! capture hands back a screenshot the model must read with its eyes, this hands
//! back the same surface as a list of elements — each with a role, a label, and a
//! screen rectangle — so a caller can target a control by identity instead of
//! guessing coordinates. It is deliberately best-effort and *partial*: only apps
//! that expose an accessibility tree appear here, so the screenshot stays the
//! spine and this augments it where the tree is good. An app (or platform) that
//! exposes nothing yields an empty list and the vision path is unaffected.
//!
//! Like [`super::input`] and [`super::desktop_context`], the "vendor" is the
//! operating system, so selection is compile-time (`cfg(target_os)`) — there is
//! no `init_from_env` and nothing to configure. Reading the tree needs the
//! **Accessibility** grant (the same one `input` needs to post events); without
//! it the frontmost app resolves to nothing and the list comes back empty rather
//! than erroring.
//!
//! Bounds are normalized 0..1 fractions of the main display — the same space
//! `look`/`act` use — so an element rectangle overlays a screenshot the model is
//! looking at, and an element's centre is a ready `act` target. The geometry math
//! ([`Rect::normalized`]) is a pure function kept here so it stays unit-testable
//! off-macOS; the macOS vendor is the thin FFI that walks the tree.
//!
//! **No caller wires this in yet.** A future `look` augmentation (append the
//! element list to the screenshot) and `act` extension (target an element by id)
//! are the callers; wiring them in later is purely additive.

/// One element of the frontmost app's accessibility tree: a labelled control with
/// a place on screen. `id` is a sequential index within a single [`inspect`]
/// snapshot — the handle a caller passes to target this element, the way `look`
/// coordinates are read off the latest capture.
#[derive(Debug, Clone, PartialEq)]
pub struct Element {
    pub id: usize,
    /// The AX role, e.g. `AXButton`, `AXTextField`, `AXStaticText`.
    pub role: String,
    /// The element's label — its title, or failing that its description.
    pub label: Option<String>,
    /// The element's value when it reads as text (a field's contents, a slider's
    /// readout); `None` for non-textual or empty values.
    pub value: Option<String>,
    /// Where the element sits, as 0..1 fractions of the main display.
    pub bounds: Rect,
}

/// A rectangle in normalized display space: `x`/`y` are the top-left as 0..1
/// fractions of the main display's width/height, `w`/`h` the size in the same
/// units. The same space `look`/`act` speak, so it overlays a screenshot directly
/// and `(x + w/2, y + h/2)` is the element's centre as an `act` target.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct Rect {
    pub x: f64,
    pub y: f64,
    pub w: f64,
    pub h: f64,
}

impl Rect {
    /// Normalize a screen rectangle given in global display points against the
    /// display's point size, clamping into 0..1. `display` is `(width, height)` in
    /// points (what `input::main_display_point_size` reports). A zero dimension
    /// can't be normalized, so it maps to 0 rather than producing a NaN.
    pub fn normalized(x: f64, y: f64, w: f64, h: f64, display: (f64, f64)) -> Rect {
        let (dw, dh) = display;
        let frac = |v: f64, d: f64| if d > 0.0 { (v / d).clamp(0.0, 1.0) } else { 0.0 };
        Rect {
            x: frac(x, dw),
            y: frac(y, dh),
            w: frac(w, dw),
            h: frac(h, dh),
        }
    }
}

/// Whether this build can read the accessibility tree on the current platform. A
/// compile-time fact, not a permission check — a macOS build without the
/// Accessibility grant (or with no GUI session) reports `true` but [`inspect`]
/// will find nothing.
pub fn available() -> bool {
    cfg!(target_os = "macos")
}

/// Read the frontmost app's accessibility tree as a flat, depth-first list of
/// labelled, on-screen elements. Best-effort: an app that exposes no tree, a
/// missing Accessibility grant, or no GUI session yields an empty list, not an
/// error. Errs only where [`available`] is `false`.
pub async fn inspect() -> anyhow::Result<Vec<Element>> {
    #[cfg(target_os = "macos")]
    {
        crate::vendors::macos_accessibility::inspect().await
    }
    #[cfg(not(target_os = "macos"))]
    {
        anyhow::bail!("accessibility inspection is not supported on this platform")
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn available_matches_platform() {
        assert_eq!(available(), cfg!(target_os = "macos"));
    }

    #[test]
    fn normalized_maps_points_to_fractions() {
        let r = Rect::normalized(960.0, 540.0, 192.0, 108.0, (1920.0, 1080.0));
        assert_eq!(r, Rect { x: 0.5, y: 0.5, w: 0.1, h: 0.1 });
    }

    #[test]
    fn normalized_clamps_out_of_bounds() {
        // A rect spilling past the edges clamps into 0..1 rather than exceeding it.
        let r = Rect::normalized(-100.0, 1000.0, 4000.0, 4000.0, (1920.0, 1080.0));
        assert_eq!(r.x, 0.0);
        assert!(r.y > 0.9 && r.y <= 1.0);
        assert_eq!(r.w, 1.0);
        assert_eq!(r.h, 1.0);
    }

    #[test]
    fn normalized_zero_display_is_zero_not_nan() {
        let r = Rect::normalized(10.0, 10.0, 10.0, 10.0, (0.0, 0.0));
        assert_eq!(r, Rect { x: 0.0, y: 0.0, w: 0.0, h: 0.0 });
    }
}
