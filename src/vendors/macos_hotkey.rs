//! macOS hotkey vendor — a listen-only Quartz event tap that recognizes a
//! double-tap of the Command key and calls back. The OS trigger behind the "come
//! and see this" gesture ([`crate::gesture`]).
//!
//! A `CGEventTap` observes `FlagsChanged` (modifier transitions, to see Command
//! press *edges*) and `KeyDown` (to break a pending double-tap on a chord like
//! ⌘C). The pure recognition lives in [`crate::capabilities::hotkey::DoubleTap`];
//! this file is the FFI that turns real key events into its inputs and drives a
//! `CFRunLoop`. The tap is **ListenOnly** — it never consumes events, so the
//! user's own Command shortcuts are untouched.
//!
//! Creating the tap needs the **Accessibility / Input Monitoring** grant; without
//! it `CGEventTapCreate` returns null and [`run`] errors, leaving the gesture
//! inert (never fatal). Only compiled on macOS.

use std::cell::{Cell, RefCell};
use std::time::{Duration, Instant};

use anyhow::anyhow;
use core_foundation::runloop::{kCFRunLoopCommonModes, CFRunLoop};
use core_graphics::event::{
    CGEventFlags, CGEventTap, CGEventTapLocation, CGEventTapOptions, CGEventTapPlacement,
    CGEventType,
};

use crate::capabilities::hotkey::DoubleTap;

/// Run the event tap on the **current thread**, calling `on_fire` on each
/// double-tap of Command. **Blocks forever** — it drives this thread's run loop —
/// so call it from a thread dedicated to the gesture. Returns `Err` only if the
/// tap can't be created (missing grant) or its run-loop source can't be made.
pub fn run(window: Duration, on_fire: impl Fn() + 'static) -> anyhow::Result<()> {
    // Monotonic clock base + the pure recognizer. Both live for this thread's
    // life (the run loop below never returns in practice), so the tap callback
    // may borrow them.
    let base = Instant::now();
    let detector = RefCell::new(DoubleTap::new(window));
    // The Command bit's last observed state, to turn FlagsChanged — which reports
    // the whole modifier set — into press edges.
    let cmd_held = Cell::new(false);

    let current = CFRunLoop::get_current();
    let tap = CGEventTap::new(
        CGEventTapLocation::HID,
        CGEventTapPlacement::HeadInsertEventTap,
        CGEventTapOptions::ListenOnly,
        vec![CGEventType::FlagsChanged, CGEventType::KeyDown],
        |_proxy, etype, event| {
            match etype {
                CGEventType::FlagsChanged => {
                    let cmd_now = event.get_flags().contains(CGEventFlags::CGEventFlagCommand);
                    let was = cmd_held.replace(cmd_now);
                    // Act only on a press edge (up→down); releases and other
                    // modifiers changing are ignored.
                    if cmd_now && !was {
                        let t = base.elapsed().as_millis() as u64;
                        if detector.borrow_mut().on_command_down(t) {
                            on_fire();
                        }
                    }
                }
                CGEventType::KeyDown => detector.borrow_mut().on_other_input(),
                _ => {}
            }
            // ListenOnly: the return is ignored; pass the event through untouched.
            None
        },
    )
    .map_err(|()| {
        anyhow!("could not create event tap (Accessibility / Input Monitoring permission?)")
    })?;

    unsafe {
        let source = tap
            .mach_port
            .create_runloop_source(0)
            .map_err(|()| anyhow!("could not create run-loop source for the event tap"))?;
        current.add_source(&source, kCFRunLoopCommonModes);
        tap.enable();
    }
    // Drives the run loop on this thread; returns only if the loop is stopped.
    CFRunLoop::run_current();
    Ok(())
}
