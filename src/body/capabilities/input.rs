//! Input-actuation capability — synthesize mouse and keyboard input on the
//! machine this process runs on. The effector twin of [`super::desktop_context`]:
//! where that *reads* the screen, this *acts* on it, so a session that has
//! decided where to click (by looking at a screenshot) can actually click.
//!
//! Like `desktop_context`, the "vendor" is the operating system, so selection is
//! compile-time (`cfg(target_os)`) — there is no `init_from_env` and nothing to
//! configure. On a platform without an impl the capability reports unavailable
//! and [`perform`] errors, so a caller degrades cleanly.
//!
//! Coordinates are **global display points** (the unit AppKit and CGEvent use),
//! not backing-store pixels — on a 2× Retina display one point is two pixels, so
//! a target read off a pixel-measured screenshot must be scaled before it is
//! passed here.
//!
//! Scope: mouse + keyboard. System media keys (play/pause a song without finding
//! its button) are a planned addition — they ride a different event family
//! (`NSSystemDefined`) and land with the wiring pass.
//!
//! **No caller wires this in yet.** A future MCP `act` tool (worker/reactor
//! surface) is the caller; wiring it in later is purely additive.

/// A screen location in global display points (origin = top-left of the main
/// display), the unit CGEvent expects.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct Point {
    pub x: f64,
    pub y: f64,
}

/// A keyboard modifier held while a key (or click) is delivered.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Modifier {
    Command,
    Shift,
    Option,
    Control,
}

/// A named key for [`Action::Press`]. `Char` carries a single character whose
/// US-ANSI virtual keycode is used (for chords like ⌘A). To enter arbitrary text
/// — including non-ASCII — use [`Action::Type`], which posts the string directly
/// and needs no keymap.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Key {
    Return,
    Tab,
    Space,
    Escape,
    Delete,
    ArrowUp,
    ArrowDown,
    ArrowLeft,
    ArrowRight,
    Char(char),
}

/// One input action to synthesize.
#[derive(Debug, Clone, PartialEq)]
pub enum Action {
    /// Move the pointer to `point` without pressing.
    MoveTo(Point),
    /// A single left click at `point`.
    Click(Point),
    /// A double left click at `point`.
    DoubleClick(Point),
    /// A right (secondary) click at `point`.
    RightClick(Point),
    /// Press at `from`, move to `to`, release — a drag.
    Drag { from: Point, to: Point },
    /// Type a string as Unicode text at the current keyboard focus.
    Type(String),
    /// Press `key` while holding `mods` (e.g. ⌘A, Return), then release.
    Press { key: Key, mods: Vec<Modifier> },
}

impl Key {
    /// The macOS ANSI virtual keycode for this key, or `None` for a `Char` whose
    /// key isn't mapped (the vendor errors on those rather than guess). These are
    /// physical-key codes; case/shift is expressed via [`Modifier::Shift`], so the
    /// lookup is case-insensitive for letters.
    pub fn key_code(self) -> Option<u16> {
        let code = match self {
            Key::Return => 0x24,
            Key::Tab => 0x30,
            Key::Space => 0x31,
            Key::Delete => 0x33,
            Key::Escape => 0x35,
            Key::ArrowLeft => 0x7B,
            Key::ArrowRight => 0x7C,
            Key::ArrowDown => 0x7D,
            Key::ArrowUp => 0x7E,
            Key::Char(c) => return char_key_code(c),
        };
        Some(code)
    }
}

/// The ANSI virtual keycode for a character, or `None` if unmapped. Letters are
/// matched case-insensitively (the physical key is the same; Shift handles case).
fn char_key_code(c: char) -> Option<u16> {
    let code = match c.to_ascii_lowercase() {
        'a' => 0x00, 's' => 0x01, 'd' => 0x02, 'f' => 0x03, 'h' => 0x04,
        'g' => 0x05, 'z' => 0x06, 'x' => 0x07, 'c' => 0x08, 'v' => 0x09,
        'b' => 0x0B, 'q' => 0x0C, 'w' => 0x0D, 'e' => 0x0E, 'r' => 0x0F,
        'y' => 0x10, 't' => 0x11, 'o' => 0x1F, 'u' => 0x20, 'i' => 0x22,
        'p' => 0x23, 'l' => 0x25, 'j' => 0x26, 'k' => 0x28, 'n' => 0x2D,
        'm' => 0x2E,
        '1' => 0x12, '2' => 0x13, '3' => 0x14, '4' => 0x15, '6' => 0x16,
        '5' => 0x17, '9' => 0x19, '7' => 0x1A, '8' => 0x1C, '0' => 0x1D,
        '-' => 0x1B, '=' => 0x18, '[' => 0x21, ']' => 0x1E, '\\' => 0x2A,
        ';' => 0x29, '\'' => 0x27, ',' => 0x2B, '.' => 0x2F, '/' => 0x2C,
        '`' => 0x32,
        ' ' => 0x31,
        _ => return None,
    };
    Some(code)
}

/// The CGEvent flag bits for a set of modifiers, OR-ed together. These bit values
/// are stable `CGEventFlags` constants (e.g. Command = `1 << 20`), kept here as
/// plain integers so the mapping is unit-testable off-macOS; the vendor wraps the
/// result in `CGEventFlags`.
pub fn modifier_mask(mods: &[Modifier]) -> u64 {
    mods.iter().fold(0, |acc, m| {
        acc | match m {
            Modifier::Command => 1 << 20,
            Modifier::Shift => 1 << 17,
            Modifier::Option => 1 << 19,
            Modifier::Control => 1 << 18,
        }
    })
}

/// Whether this build can synthesize input on the current platform. Compile-time,
/// not a permission check — a macOS build still needs the Accessibility grant for
/// posted events to take effect; without it [`perform`] returns `Ok` but the
/// system silently drops the event.
pub fn available() -> bool {
    cfg!(target_os = "macos")
}

/// Synthesize one input action. Errors on an unsupported platform.
pub async fn perform(action: Action) -> anyhow::Result<()> {
    #[cfg(target_os = "macos")]
    {
        crate::vendors::macos_input::perform(action).await
    }
    #[cfg(not(target_os = "macos"))]
    {
        let _ = action;
        anyhow::bail!("input actuation is not supported on this platform")
    }
}

/// The main display's logical size in points (origin top-left) — the unit
/// [`perform`] coordinates use. A normalized 0..1 screen fraction times this size
/// gives the global display point to act on. Errors on an unsupported platform.
pub fn main_display_point_size() -> anyhow::Result<(f64, f64)> {
    #[cfg(target_os = "macos")]
    {
        crate::vendors::macos_input::main_display_point_size()
    }
    #[cfg(not(target_os = "macos"))]
    {
        anyhow::bail!("display size is not supported on this platform")
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn named_keys_map_to_ansi_codes() {
        assert_eq!(Key::Return.key_code(), Some(0x24));
        assert_eq!(Key::Space.key_code(), Some(0x31));
        assert_eq!(Key::ArrowUp.key_code(), Some(0x7E));
    }

    #[test]
    fn char_keys_are_case_insensitive_and_partial() {
        assert_eq!(Key::Char('a').key_code(), Some(0x00));
        assert_eq!(Key::Char('A').key_code(), Some(0x00)); // shift handles case
        assert_eq!(Key::Char('0').key_code(), Some(0x1D));
        assert_eq!(Key::Char('é').key_code(), None); // use Action::Type instead
    }

    #[test]
    fn modifier_mask_ors_the_expected_bits() {
        assert_eq!(modifier_mask(&[]), 0);
        assert_eq!(modifier_mask(&[Modifier::Command]), 0x10_0000);
        assert_eq!(modifier_mask(&[Modifier::Shift]), 0x2_0000);
        assert_eq!(
            modifier_mask(&[Modifier::Command, Modifier::Shift]),
            0x10_0000 | 0x2_0000
        );
    }

    #[test]
    fn available_matches_platform() {
        assert_eq!(available(), cfg!(target_os = "macos"));
    }
}
