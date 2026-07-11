//! macOS hotkey vendor — a listen-only Quartz event tap that turns raw **right**-Command
//! events into [`Edge`]s and drives a `CFRunLoop`. The OS trigger behind the
//! Command-key gestures ([`crate::body::gesture`]).
//!
//! A `CGEventTap` observes `FlagsChanged` (modifier transitions, to see the right
//! Command's press *and* release edges) and `KeyDown` (an `Other` edge, to break a
//! pending tap / disarm a pending hold on a chord). All recognition — double-tap,
//! hold, and the hold's threshold timing — lives in [`crate::body::capabilities::hotkey`]
//! and [`crate::body::gesture`]; this file is the FFI that emits edges and runs the loop,
//! doing no timing of its own. The tap is **ListenOnly** — it never consumes events,
//! so the user's own Command shortcuts are untouched.
//!
//! **Only the right Command key triggers the gestures.** Left Command is the everyday
//! shortcut modifier — users hold it while reaching for the next key, and rest on it
//! mid-thought deciding which shortcut to press — so a hold/double-tap detector bound
//! to it would fire on that noise. The right Command is almost never chorded or rested
//! on, so it makes a quiet, dedicated trigger.
//!
//! Creating the tap needs the **Accessibility / Input Monitoring** grant; without
//! it `CGEventTapCreate` returns null and [`run`] errors, leaving the gestures
//! inert (never fatal). Only compiled on macOS.

use std::cell::Cell;

use anyhow::anyhow;
use core_foundation::runloop::{kCFRunLoopCommonModes, CFRunLoop};
use core_graphics::event::{
    CallbackResult, CGEventTap, CGEventTapLocation, CGEventTapOptions, CGEventTapPlacement,
    CGEventType,
};

use crate::body::capabilities::hotkey::Edge;

/// The device-dependent flag bit for the **right** Command key (`NX_DEVICERCMDKEYMASK`).
/// The general `CGEventFlagCommand` bit (`0x0010_0000`) is set whenever *either* Command
/// is down, so it can't tell the two keys apart; this low device bit — present on the
/// HID-level events this tap observes — isolates the right key alone. (Left Command is
/// `NX_DEVICELCMDKEYMASK`, `0x08`.)
const RIGHT_COMMAND_MASK: u64 = 0x10;

/// Run the event tap on the **current thread**, calling `on_edge` for each raw
/// right-Command edge. **Blocks forever** — it drives this thread's run loop — so call
/// it from a thread dedicated to the gesture. Returns `Err` only if the tap can't be
/// created (missing grant) or its run-loop source can't be made.
pub fn run(on_edge: impl Fn(Edge) + 'static) -> anyhow::Result<()> {
    // The right-Command bit's last observed state, to turn FlagsChanged — which reports
    // the whole modifier set — into press/release edges for that one key. Lives for this
    // thread's life (the run loop below never returns in practice), so the tap callback
    // may borrow it.
    let cmd_held = Cell::new(false);

    let current = CFRunLoop::get_current();
    // SAFETY: core-graphics 0.25's safe `new` requires the callback be `Send +
    // 'static`, but ours borrows non-Send thread-local state (`cmd_held`, `on_edge`).
    // `new_unchecked`'s contract is satisfied instead: this tap is installed only on
    // the current thread's run loop (below) and never handed elsewhere, so the
    // callback is only ever invoked on this thread.
    let tap = unsafe { CGEventTap::new_unchecked(
        CGEventTapLocation::HID,
        CGEventTapPlacement::HeadInsertEventTap,
        CGEventTapOptions::ListenOnly,
        vec![CGEventType::FlagsChanged, CGEventType::KeyDown],
        |_proxy, etype, event| {
            match etype {
                CGEventType::FlagsChanged => {
                    // Track the right Command key alone via its device bit; the general
                    // command flag would also be set by the left key.
                    let cmd_now = event.get_flags().bits() & RIGHT_COMMAND_MASK != 0;
                    let was = cmd_held.replace(cmd_now);
                    // Emit the right-Command edge; other modifiers changing produce neither.
                    if cmd_now && !was {
                        on_edge(Edge::CmdDown);
                    } else if !cmd_now && was {
                        on_edge(Edge::CmdUp);
                    }
                }
                CGEventType::KeyDown => on_edge(Edge::Other),
                _ => {}
            }
            // ListenOnly: the return is ignored; pass the event through untouched.
            CallbackResult::Keep
        },
    ) }
    .map_err(|()| {
        anyhow!("could not create event tap (Accessibility / Input Monitoring permission?)")
    })?;

    unsafe {
        let source = tap
            .mach_port()
            .create_runloop_source(0)
            .map_err(|()| anyhow!("could not create run-loop source for the event tap"))?;
        current.add_source(&source, kCFRunLoopCommonModes);
        tap.enable();
    }
    // Drives the run loop on this thread; returns only if the loop is stopped.
    CFRunLoop::run_current();
    Ok(())
}
