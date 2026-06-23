//! Hotkey-gesture capability — recognize Command-key gestures from a stream of key
//! events. Two gestures share the one key:
//! - a **double-tap** ("come and see this") hands the agent a screenshot
//!   ([`DoubleTap`]); and
//! - a **press-and-hold** opens continuous attention — the agent listens for as long
//!   as Command is held, then stops on release ([`Hold`]).
//!
//! The pure recognizers live here so they are unit-testable off-macOS; the OS event
//! tap that feeds them real key presses is the vendor
//! ([`crate::foundation::vendors::macos_hotkey`]), selected at compile time like the other
//! desktop capabilities ([`super::input`], [`super::screencast`]). The vendor only
//! translates raw key events into [`Edge`]s; the recognizers — and the hold's
//! threshold timer — are driven from [`crate::body::gesture`] against one monotonic clock.
//! Observing global key events needs the **Accessibility / Input Monitoring** grant;
//! without it the tap can't be created and the gestures are simply inert — never fatal.

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

/// Default time Command must be held before a press becomes a *hold* (continuous
/// attention) rather than a tap. Longer than [`DEFAULT_WINDOW`] so a deliberate hold
/// is unambiguous and a normal tap or double-tap never trips it.
pub const DEFAULT_HOLD: Duration = Duration::from_millis(450);

/// One raw Command-key edge from the OS tap. The vendor emits these (it does no
/// timing); [`crate::body::gesture`] stamps each on arrival against its own clock and
/// drives the recognizers — so host and recognizer share one clock and the vendor
/// stays a dumb translator.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Edge {
    /// Command went down (a rising modifier edge).
    CmdDown,
    /// Command came up (a falling modifier edge).
    CmdUp,
    /// Some other key went down — breaks a pending tap and disarms a pending hold,
    /// so chords like ⌘C are neither a glance nor an attention hold.
    Other,
}

/// A recognized gesture, ready for the host to act on.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum GestureEvent {
    /// Double-tap completed — hand over a screenshot.
    Glance,
    /// Command has been held past the threshold — open continuous attention.
    HoldStart,
    /// The held Command was released — close continuous attention.
    HoldEnd,
}

/// Recognizes a **press-and-hold** of Command: a single press still down after the
/// hold threshold is a `HoldStart`, and its release is a `HoldEnd`. Pure and
/// time-injected — it can't see the clock itself, so the host calls [`Hold::poll`]
/// (using [`Hold::next_deadline`] to know when) to let a still-down press cross the
/// threshold. Runs alongside [`DoubleTap`]; the host disarms one when the other
/// fires (a completed double-tap, or another key) via [`Hold::cancel`].
#[derive(Debug)]
pub struct Hold {
    hold_ms: u64,
    press: Option<Press>,
}

#[derive(Debug)]
struct Press {
    down_at: u64,
    /// Whether this press can still become a hold (cleared once it does, or by a
    /// chord / a completed double-tap).
    armed: bool,
    holding: bool,
}

impl Hold {
    pub fn new(hold: Duration) -> Self {
        Self { hold_ms: hold.as_millis() as u64, press: None }
    }

    /// Command went down at `t_ms`: a fresh press, armed to become a hold.
    pub fn on_command_down(&mut self, t_ms: u64) {
        self.press = Some(Press { down_at: t_ms, armed: true, holding: false });
    }

    /// Command came up. Returns `HoldEnd` only if this press had become a hold;
    /// a quick tap (released before the threshold) returns `None`.
    pub fn on_command_up(&mut self, _t_ms: u64) -> Option<GestureEvent> {
        match self.press.take() {
            Some(p) if p.holding => Some(GestureEvent::HoldEnd),
            _ => None,
        }
    }

    /// Let an armed, still-down press cross the threshold. Returns `HoldStart` the
    /// first time `now_ms` is at least the threshold past the press; idempotent
    /// afterward. The host calls this when [`Hold::next_deadline`] elapses.
    pub fn poll(&mut self, now_ms: u64) -> Option<GestureEvent> {
        if let Some(p) = self.press.as_mut()
            && p.armed
            && !p.holding
            && now_ms.saturating_sub(p.down_at) >= self.hold_ms
        {
            p.armed = false;
            p.holding = true;
            return Some(GestureEvent::HoldStart);
        }
        None
    }

    /// Disarm the current press so it can't become a hold — for a chord (other key)
    /// or when the same down completed a double-tap. A press already *holding* is
    /// left alone, so its release still yields `HoldEnd`.
    pub fn cancel(&mut self) {
        if let Some(p) = self.press.as_mut() {
            p.armed = false;
        }
    }

    /// When the host should next [`poll`](Hold::poll): the absolute time (same clock
    /// as the fed timestamps) an armed press would cross the threshold, or `None`
    /// when nothing is waiting to become a hold.
    pub fn next_deadline(&self) -> Option<u64> {
        self.press
            .as_ref()
            .filter(|p| p.armed && !p.holding)
            .map(|p| p.down_at + self.hold_ms)
    }
}

/// Whether this build can observe global key events (and thus arm the gesture).
/// Compile-time, not a permission check — a macOS build still needs the
/// Accessibility / Input Monitoring grant for the tap to actually receive events.
pub fn available() -> bool {
    cfg!(target_os = "macos")
}

/// Listen for raw Command-key edges, calling `on_edge` for each. **Blocks** for the
/// lifetime of the process (it drives an OS run loop), so call it from a dedicated
/// thread. Recognition (double-tap, hold) is the caller's — it stamps edges against
/// its own clock and drives the recognizers. Errors if the platform has no impl or
/// the OS won't grant the event tap — the caller logs and leaves the gestures inert.
pub fn listen(on_edge: impl Fn(Edge) + Send + 'static) -> anyhow::Result<()> {
    #[cfg(target_os = "macos")]
    {
        crate::foundation::vendors::macos_hotkey::run(on_edge)
    }
    #[cfg(not(target_os = "macos"))]
    {
        let _ = on_edge;
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

    fn hold() -> Hold {
        Hold::new(Duration::from_millis(450))
    }

    #[test]
    fn press_held_past_threshold_starts_then_release_ends() {
        let mut h = hold();
        h.on_command_down(1_000);
        assert_eq!(h.poll(1_449), None, "not yet past the threshold");
        assert_eq!(h.poll(1_450), Some(GestureEvent::HoldStart), "exactly at the threshold starts");
        assert_eq!(h.poll(1_600), None, "HoldStart is one-shot");
        assert_eq!(h.on_command_up(2_000), Some(GestureEvent::HoldEnd));
    }

    #[test]
    fn quick_tap_is_not_a_hold() {
        let mut h = hold();
        h.on_command_down(1_000);
        assert_eq!(h.on_command_up(1_100), None, "released before the threshold: no hold");
        assert_eq!(h.poll(2_000), None, "and nothing fires afterward");
    }

    #[test]
    fn cancel_prevents_a_hold() {
        let mut h = hold();
        h.on_command_down(1_000);
        h.cancel(); // e.g. a chord, or this down completed a double-tap
        assert_eq!(h.poll(2_000), None);
        assert_eq!(h.on_command_up(2_100), None);
    }

    #[test]
    fn cancel_after_holding_still_ends_on_release() {
        let mut h = hold();
        h.on_command_down(1_000);
        assert_eq!(h.poll(1_500), Some(GestureEvent::HoldStart));
        h.cancel(); // a key pressed *during* attention must not drop it
        assert_eq!(h.on_command_up(2_000), Some(GestureEvent::HoldEnd));
    }

    #[test]
    fn next_deadline_tracks_the_armed_press() {
        let mut h = hold();
        assert_eq!(h.next_deadline(), None, "nothing pending");
        h.on_command_down(1_000);
        assert_eq!(h.next_deadline(), Some(1_450));
        h.poll(1_450);
        assert_eq!(h.next_deadline(), None, "a started hold has no pending deadline");
    }
}
