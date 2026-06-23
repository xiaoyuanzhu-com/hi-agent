//! macOS input vendor — synthesize mouse and keyboard events via Quartz CGEvent.
//!
//! Each [`Action`] becomes one or more `CGEvent`s posted at the HID tap (the
//! lowest public injection point, so the frontmost app receives them as real
//! input). Posting needs the process to hold the **Accessibility** grant; without
//! it the calls still return `Ok` but the system silently drops the events.
//!
//! The pure mapping (keycodes, modifier bits) lives in
//! [`crate::body::capabilities::input`] so it stays unit-testable off-macOS; this file
//! is the thin FFI that turns an action into posted events and is only compiled
//! on macOS.

use anyhow::{Context, anyhow};
use core_graphics::event::{
    CGEvent, CGEventFlags, CGEventTapLocation, CGEventType, CGMouseButton, EventField,
};
use core_graphics::event_source::{CGEventSource, CGEventSourceStateID};
use core_graphics::geometry::CGPoint;
use core_graphics::display::CGDisplay;

use crate::body::capabilities::input::{Action, Key, Point, modifier_mask};

/// Synthesize one action. The CGEvent calls are blocking, so they run on a
/// blocking thread to keep the async runtime free (matching the other vendors).
pub async fn perform(action: Action) -> anyhow::Result<()> {
    tokio::task::spawn_blocking(move || perform_blocking(action))
        .await
        .context("input perform task panicked")?
}

fn perform_blocking(action: Action) -> anyhow::Result<()> {
    match action {
        Action::MoveTo(p) => post_mouse(CGEventType::MouseMoved, p, CGMouseButton::Left, 1),
        Action::Click(p) => click(p, CGMouseButton::Left, 1),
        // Two down/up pairs; the second carries click-state 2 so apps read one
        // double-click rather than two singles.
        Action::DoubleClick(p) => {
            click(p, CGMouseButton::Left, 1)?;
            click(p, CGMouseButton::Left, 2)
        }
        Action::RightClick(p) => click(p, CGMouseButton::Right, 1),
        Action::Drag { from, to } => drag(from, to),
        Action::Type(text) => type_text(&text),
        Action::Press { key, mods } => press(key, &mods),
    }
}

/// A fresh HID-state event source. Returns an error if the source can't be made,
/// which in practice means the Accessibility grant is missing.
fn source() -> anyhow::Result<CGEventSource> {
    CGEventSource::new(CGEventSourceStateID::HIDSystemState)
        .map_err(|()| anyhow!("could not create CGEventSource (Accessibility permission?)"))
}

fn cgpoint(p: Point) -> CGPoint {
    CGPoint::new(p.x, p.y)
}

/// Post one mouse event. `click_state` rides only when > 1 (for double-clicks);
/// a plain move/click leaves the field at its default.
fn post_mouse(
    ty: CGEventType,
    p: Point,
    button: CGMouseButton,
    click_state: i64,
) -> anyhow::Result<()> {
    let ev = CGEvent::new_mouse_event(source()?, ty, cgpoint(p), button)
        .map_err(|()| anyhow!("could not create mouse event"))?;
    if click_state > 1 {
        ev.set_integer_value_field(EventField::MOUSE_EVENT_CLICK_STATE, click_state);
    }
    ev.post(CGEventTapLocation::HID);
    Ok(())
}

fn click(p: Point, button: CGMouseButton, click_state: i64) -> anyhow::Result<()> {
    let (down, up) = match button {
        CGMouseButton::Right => (CGEventType::RightMouseDown, CGEventType::RightMouseUp),
        _ => (CGEventType::LeftMouseDown, CGEventType::LeftMouseUp),
    };
    post_mouse(down, p, button, click_state)?;
    post_mouse(up, p, button, click_state)
}

fn drag(from: Point, to: Point) -> anyhow::Result<()> {
    post_mouse(CGEventType::LeftMouseDown, from, CGMouseButton::Left, 1)?;
    post_mouse(CGEventType::LeftMouseDragged, to, CGMouseButton::Left, 1)?;
    post_mouse(CGEventType::LeftMouseUp, to, CGMouseButton::Left, 1)
}

/// Type a string as Unicode. A keyboard event with `set_string` overrides the
/// per-key character payload, so the whole string is delivered at once and no
/// keymap is needed (this is the path for non-ASCII like a song title).
fn type_text(text: &str) -> anyhow::Result<()> {
    let down = CGEvent::new_keyboard_event(source()?, 0, true)
        .map_err(|()| anyhow!("could not create keyboard event"))?;
    down.set_string(text);
    down.post(CGEventTapLocation::HID);
    let up = CGEvent::new_keyboard_event(source()?, 0, false)
        .map_err(|()| anyhow!("could not create keyboard event"))?;
    up.set_string(text);
    up.post(CGEventTapLocation::HID);
    Ok(())
}

/// Press a named key while holding modifiers (a chord like ⌘A), then release.
fn press(key: Key, mods: &[crate::body::capabilities::input::Modifier]) -> anyhow::Result<()> {
    let code = key
        .key_code()
        .ok_or_else(|| anyhow!("no keycode for {key:?}; use Type for arbitrary text"))?;
    let flags = CGEventFlags::from_bits_truncate(modifier_mask(mods));

    let down = CGEvent::new_keyboard_event(source()?, code, true)
        .map_err(|()| anyhow!("could not create keyboard event"))?;
    down.set_flags(flags);
    down.post(CGEventTapLocation::HID);

    let up = CGEvent::new_keyboard_event(source()?, code, false)
        .map_err(|()| anyhow!("could not create keyboard event"))?;
    up.set_flags(flags);
    up.post(CGEventTapLocation::HID);
    Ok(())
}

/// The main display's logical bounds size in points (not backing-store pixels).
/// `CGDisplayBounds` lives in the global display point space — the same unit the
/// mouse events use — so a normalized screen fraction maps straight through.
pub fn main_display_point_size() -> anyhow::Result<(f64, f64)> {
    let bounds = CGDisplay::main().bounds();
    anyhow::ensure!(
        bounds.size.width > 0.0 && bounds.size.height > 0.0,
        "main display reported a zero size"
    );
    Ok((bounds.size.width, bounds.size.height))
}
