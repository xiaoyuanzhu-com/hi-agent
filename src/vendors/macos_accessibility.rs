//! macOS accessibility vendor — read the frontmost app's AX tree via the
//! Accessibility API (`AXUIElement`), the structured-UI twin of
//! [`crate::vendors::macos_desktop_context`]'s pixel capture.
//!
//! There is no high-level Rust crate for the Accessibility API, so this is a thin
//! hand-rolled FFI over the C `AXUIElement*` functions (the ApplicationServices
//! framework), using `core-foundation`/`core-graphics` (already deps) for the
//! CFString it hands back and the CGPoint/CGSize geometry. Reading the tree needs
//! the **Accessibility** grant — the same one `macos_input` needs to post events;
//! without it (or with no GUI session) the frontmost app resolves to null and we
//! return an empty list, never an error.
//!
//! The geometry normalization lives in [`crate::body::capabilities::accessibility`] so
//! it stays unit-testable off-macOS; this file only walks the tree and reads
//! attributes. The walk is bounded ([`MAX_ELEMENTS`], [`MAX_DEPTH`]) so a deep or
//! pathological tree can't run away. The attribute readers take a raw AX
//! reference and are sound only for the non-null references AX itself hands back
//! within this module — which is the only way they are ever called.

use std::ffi::c_void;

use anyhow::Context;
use core_foundation::base::TCFType;
use core_foundation::string::{CFString, CFStringRef};
use core_graphics::display::CGDisplay;
use core_graphics::geometry::{CGPoint, CGSize};

use crate::body::capabilities::accessibility::{Element, Rect};

/// An opaque CoreFoundation / Accessibility object pointer — `CFTypeRef`,
/// `AXUIElementRef`, and `AXValueRef` all alias to this.
type CFTypeRef = *const c_void;

/// `AXValueType` selector for extracting a `CGPoint` from an `AXValue`.
const KAXVALUE_CGPOINT_TYPE: u32 = 1;
/// `AXValueType` selector for extracting a `CGSize` from an `AXValue`.
const KAXVALUE_CGSIZE_TYPE: u32 = 2;

/// Bound the walk so a deep or cyclic-looking tree can't blow the stack or hang.
const MAX_ELEMENTS: usize = 1500;
const MAX_DEPTH: usize = 40;

#[link(name = "ApplicationServices", kind = "framework")]
unsafe extern "C" {
    fn AXUIElementCreateSystemWide() -> CFTypeRef;
    fn AXUIElementCopyAttributeValue(
        element: CFTypeRef,
        attribute: CFStringRef,
        value: *mut CFTypeRef,
    ) -> i32;
    fn AXValueGetValue(value: CFTypeRef, the_type: u32, out: *mut c_void) -> u8;
}

#[link(name = "CoreFoundation", kind = "framework")]
unsafe extern "C" {
    fn CFRelease(cf: CFTypeRef);
    fn CFGetTypeID(cf: CFTypeRef) -> usize;
    fn CFStringGetTypeID() -> usize;
    fn CFArrayGetTypeID() -> usize;
    fn CFArrayGetCount(array: CFTypeRef) -> isize;
    fn CFArrayGetValueAtIndex(array: CFTypeRef, idx: isize) -> CFTypeRef;
}

/// RAII guard for an owned (+1) CoreFoundation reference.
struct Owned(CFTypeRef);

impl Drop for Owned {
    fn drop(&mut self) {
        if !self.0.is_null() {
            // SAFETY: `self.0` is the non-null +1 reference we took ownership of.
            unsafe { CFRelease(self.0) };
        }
    }
}

/// Read the frontmost app's AX tree. The FFI is blocking, so it runs on a
/// blocking thread to keep the async runtime free (matching the other vendors).
pub async fn inspect() -> anyhow::Result<Vec<Element>> {
    tokio::task::spawn_blocking(inspect_blocking)
        .await
        .context("accessibility inspect task panicked")?
}

fn inspect_blocking() -> anyhow::Result<Vec<Element>> {
    let display = main_display_size()?;
    let mut out = Vec::new();
    // SAFETY: takes no arguments; returns a valid system-wide element (+1, owned
    // below) or null.
    let system = unsafe { AXUIElementCreateSystemWide() };
    if system.is_null() {
        return Ok(out);
    }
    let system = Owned(system);
    // No frontmost GUI app (headless / SSH session / nothing focused) or no
    // Accessibility grant → nothing to read. Degrade to an empty list.
    if let Some(app) = copy_attr(system.0, "AXFocusedApplication") {
        walk(app.0, display, 0, &mut out);
    }
    Ok(out)
}

/// Depth-first walk: emit each element that has a role and an on-screen frame,
/// then recurse into its children. Frameless structural nodes are skipped (not
/// targetable) but still walked through.
fn walk(element: CFTypeRef, display: (f64, f64), depth: usize, out: &mut Vec<Element>) {
    if element.is_null() || depth > MAX_DEPTH || out.len() >= MAX_ELEMENTS {
        return;
    }

    if let Some(role) = string_attr(element, "AXRole") {
        if let (Some((x, y)), Some((w, h))) =
            (point_attr(element, "AXPosition"), size_attr(element, "AXSize"))
        {
            let label =
                string_attr(element, "AXTitle").or_else(|| string_attr(element, "AXDescription"));
            let value = string_attr(element, "AXValue");
            out.push(Element {
                id: out.len(),
                role,
                label: nonempty(label),
                value: nonempty(value),
                bounds: Rect::normalized(x, y, w, h, display),
            });
        }
    }

    let Some(children) = copy_attr(element, "AXChildren") else {
        return;
    };
    // SAFETY: `children.0` is a non-null CF object we own; we read it as a CFArray
    // of AX references only after confirming its type id, and index within count.
    unsafe {
        if CFGetTypeID(children.0) != CFArrayGetTypeID() {
            return;
        }
        let count = CFArrayGetCount(children.0);
        for i in 0..count {
            if out.len() >= MAX_ELEMENTS {
                break;
            }
            // Borrowed under the array's retain; `children` stays alive across the
            // recursion, so no per-child retain is needed.
            let child = CFArrayGetValueAtIndex(children.0, i);
            walk(child, display, depth + 1, out);
        }
    }
}

/// Copy one attribute value, owning the returned reference. `None` on any AX
/// error (missing attribute, no permission) or a null result.
fn copy_attr(element: CFTypeRef, name: &str) -> Option<Owned> {
    let attr = CFString::new(name);
    let mut value: CFTypeRef = std::ptr::null();
    // SAFETY: `element` is a valid AX reference, `attr` outlives the call, and
    // `value` is a valid out-pointer; the call writes a +1 reference or an error.
    let err =
        unsafe { AXUIElementCopyAttributeValue(element, attr.as_concrete_TypeRef(), &mut value) };
    (err == 0 && !value.is_null()).then(|| Owned(value))
}

/// Read a string-typed attribute (role, title, description, a textual value).
/// `None` if absent or not a `CFString`.
fn string_attr(element: CFTypeRef, name: &str) -> Option<String> {
    let v = copy_attr(element, name)?;
    // SAFETY: `v.0` is a non-null CF object we own; we treat it as a CFString only
    // after confirming its type id. wrap_under_get_rule borrows (no release), so
    // `v` still owns the +1 and releases it on drop.
    unsafe {
        if CFGetTypeID(v.0) != CFStringGetTypeID() {
            return None;
        }
        Some(CFString::wrap_under_get_rule(v.0 as CFStringRef).to_string())
    }
}

/// Read an `AXPosition` (a `CGPoint` wrapped in an `AXValue`) as `(x, y)` in
/// global display points.
fn point_attr(element: CFTypeRef, name: &str) -> Option<(f64, f64)> {
    let v = copy_attr(element, name)?;
    let mut p = CGPoint { x: 0.0, y: 0.0 };
    // SAFETY: `v.0` is a non-null AXValue; AXValueGetValue writes a CGPoint into
    // `p` and returns true only when the value really is a point.
    let ok =
        unsafe { AXValueGetValue(v.0, KAXVALUE_CGPOINT_TYPE, &mut p as *mut CGPoint as *mut c_void) };
    (ok != 0).then_some((p.x, p.y))
}

/// Read an `AXSize` (a `CGSize` wrapped in an `AXValue`) as `(w, h)` in points.
fn size_attr(element: CFTypeRef, name: &str) -> Option<(f64, f64)> {
    let v = copy_attr(element, name)?;
    let mut s = CGSize { width: 0.0, height: 0.0 };
    // SAFETY: `v.0` is a non-null AXValue; AXValueGetValue writes a CGSize into
    // `s` and returns true only when the value really is a size.
    let ok =
        unsafe { AXValueGetValue(v.0, KAXVALUE_CGSIZE_TYPE, &mut s as *mut CGSize as *mut c_void) };
    (ok != 0).then_some((s.width, s.height))
}

fn nonempty(s: Option<String>) -> Option<String> {
    s.filter(|s| !s.trim().is_empty())
}

/// The main display's logical size in points — the space AX positions/sizes live
/// in. Computed here (not borrowed from the input vendor) to keep the two OS
/// vendors independent.
fn main_display_size() -> anyhow::Result<(f64, f64)> {
    let bounds = CGDisplay::main().bounds();
    anyhow::ensure!(
        bounds.size.width > 0.0 && bounds.size.height > 0.0,
        "main display reported a zero size"
    );
    Ok((bounds.size.width, bounds.size.height))
}
