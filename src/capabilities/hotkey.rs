//! Hotkey-gesture capability — recognize a **double-tap of a modifier key**
//! (today: Command) from a stream of key events. This is the trigger behind the
//! "come and see this" gesture: the user double-taps Command and the agent is
//! handed a screenshot of their current screen as a file (see [`crate::gesture`]).
//!
//! The pure recognizer ([`DoubleTap`]) lives here so it is unit-testable off-macOS;
//! the OS event tap that feeds it real key presses is the vendor
//! ([`crate::vendors::macos_hotkey`]), selected at compile time like the other
//! desktop capabilities ([`super::input`], [`super::screencast`]). Observing global
//! key events needs the **Accessibility / Input Monitoring** grant; without it the
//! tap can't be created and the gesture is simply inert — never fatal.

use std::time::Duration;

/// Default maximum gap between the two Command presses to count as a double-tap.
/// Tuned like a double-click: snappy enough not to fire on two deliberate, spaced
/// presses, loose enough for a natural double-tap.
pub const DEFAULT_WINDOW: Duration = Duration::from_millis(400);

/// Recognizes a double-tap of Command from a sequence of Command *presses* (rising
/// edges) and other-key events. Pure and time-injected (milliseconds on a
/// monotonic clock) so it needs neither a real keyboard nor a real clock to test.
#[derive(Debug)]
pub struct DoubleTap {
    window_ms: u64,
    /// When the first, still-pairable Command press happened, if one is pending.
    pending: Option<u64>,
}

impl DoubleTap {
    pub fn new(window: Duration) -> Self {
        Self { window_ms: window.as_millis() as u64, pending: None }
    }

    /// Feed a Command-key **press** (a rising edge) at `t_ms`. Returns `true` when
    /// it completes a double-tap — a prior press within the window — and arms for
    /// the next one. A lone press (or one too late) becomes the new pending press.
    pub fn on_command_down(&mut self, t_ms: u64) -> bool {
        match self.pending.take() {
            Some(prev) if t_ms.saturating_sub(prev) <= self.window_ms => true,
            _ => {
                self.pending = Some(t_ms);
                false
            }
        }
    }

    /// Feed any other key press. It breaks an in-progress double-tap, so chords
    /// like ⌘C — Command held, then C — never masquerade as the gesture.
    pub fn on_other_input(&mut self) {
        self.pending = None;
    }
}

/// Whether this build can observe global key events (and thus arm the gesture).
/// Compile-time, not a permission check — a macOS build still needs the
/// Accessibility / Input Monitoring grant for the tap to actually receive events.
pub fn available() -> bool {
    cfg!(target_os = "macos")
}

/// Listen for the double-tap-Command gesture, calling `on_fire` each time it
/// happens. **Blocks** for the lifetime of the process (it drives an OS run loop),
/// so call it from a dedicated thread. Errors if the platform has no impl or the
/// OS won't grant the event tap — the caller logs and leaves the gesture inert.
pub fn listen(window: Duration, on_fire: impl Fn() + Send + 'static) -> anyhow::Result<()> {
    #[cfg(target_os = "macos")]
    {
        crate::vendors::macos_hotkey::run(window, on_fire)
    }
    #[cfg(not(target_os = "macos"))]
    {
        let _ = (window, on_fire);
        anyhow::bail!("hotkey gesture is not supported on this platform")
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn det() -> DoubleTap {
        DoubleTap::new(Duration::from_millis(400))
    }

    #[test]
    fn two_presses_within_window_fire() {
        let mut d = det();
        assert!(!d.on_command_down(1_000), "first press only arms");
        assert!(d.on_command_down(1_300), "second within 400ms fires");
    }

    #[test]
    fn second_press_too_late_does_not_fire_but_re_arms() {
        let mut d = det();
        assert!(!d.on_command_down(1_000));
        assert!(!d.on_command_down(1_500), "500ms > window: no fire, becomes new pending");
        assert!(d.on_command_down(1_700), "now a pair within window fires");
    }

    #[test]
    fn other_key_between_presses_cancels() {
        let mut d = det();
        assert!(!d.on_command_down(1_000));
        d.on_other_input(); // e.g. the C in ⌘C
        assert!(!d.on_command_down(1_200), "the chord broke the pending tap");
    }

    #[test]
    fn boundary_gap_equal_to_window_fires() {
        let mut d = det();
        assert!(!d.on_command_down(1_000));
        assert!(d.on_command_down(1_400), "exactly at the window is inclusive");
    }

    #[test]
    fn available_matches_platform() {
        assert_eq!(available(), cfg!(target_os = "macos"));
    }
}
