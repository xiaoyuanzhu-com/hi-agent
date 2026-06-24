//! macOS tray vendor — a menu-bar `NSStatusItem` with an "Open" / "Quit" menu, the
//! visible affordance of the desktop install shape ([`crate::body::capabilities::tray`]).
//!
//! AppKit (`NSApplication`, `NSStatusItem`, `NSMenu`) **must run on the process
//! main thread**, which must own the AppKit event loop — so [`run`] takes a
//! [`MainThreadMarker`] and blocks on `NSApplication::run` for the process life.
//! The activation policy is set to *Accessory*, so there is no Dock icon and no app
//! menu — just the menu-bar item (the programmatic equivalent of `LSUIElement`, so
//! no `.app`/Info.plist is needed).
//!
//! Unlike the cocoa-rs FFI the other macOS vendors use, the tray needs an
//! Objective-C action target for the menu clicks; this uses the `objc2` family,
//! whose `define_class!` generates that target safely. Only compiled on macOS.

use std::cell::Cell;
use std::sync::{Arc, OnceLock};

use anyhow::anyhow;
use objc2::rc::Retained;
use objc2::runtime::{AnyObject, NSObject, NSObjectProtocol};
use objc2::{define_class, msg_send, sel, AnyThread, ClassType, DefinedClass, MainThreadOnly};
use objc2_app_kit::{
    NSApplication, NSApplicationActivationPolicy, NSImage, NSMenu, NSMenuItem, NSStatusBar,
    NSStatusBarButton, NSVariableStatusItemLength,
};
use objc2_foundation::{MainThreadMarker, NSData, NSSize, NSString};
use tokio::sync::Notify;

/// State the menu actions close over: where to point the browser, and the trigger
/// that asks the server to drain. Held as the target object's instance variables.
struct Ivars {
    url: String,
    shutdown: Arc<Notify>,
}

define_class!(
    // A plain NSObject that serves as the menu items' target/action receiver.
    #[unsafe(super(NSObject))]
    #[thread_kind = MainThreadOnly]
    #[name = "HiAgentTrayTarget"]
    #[ivars = Ivars]
    struct TrayTarget;

    unsafe impl NSObjectProtocol for TrayTarget {}

    impl TrayTarget {
        /// "Open hi-agent" → open the web UI in the default browser. `open(1)` is
        /// the standard launch-services CLI; spawning it avoids pulling NSWorkspace.
        #[unsafe(method(open:))]
        fn open(&self, _sender: Option<&AnyObject>) {
            if let Err(e) = std::process::Command::new("open")
                .arg(&self.ivars().url)
                .spawn()
            {
                tracing::warn!(error = %e, "tray: failed to open the web UI");
            }
        }

        /// "Quit hi-agent" → ask the server to shut down. The server thread runs the
        /// normal graceful drain + ACP reap, then exits the process (which ends this
        /// run loop). We do not stop AppKit here — the process exit does it.
        #[unsafe(method(quit:))]
        fn quit(&self, _sender: Option<&AnyObject>) {
            self.ivars().shutdown.notify_waiters();
        }
    }
);

impl TrayTarget {
    fn new(mtm: MainThreadMarker, url: String, shutdown: Arc<Notify>) -> Retained<Self> {
        let this = Self::alloc(mtm).set_ivars(Ivars { url, shutdown });
        unsafe { msg_send![super(this), init] }
    }
}

/// The gesture-ack pulse: on a received gesture the menu-bar icon blinks between
/// its resting `sparkles` and an `eye` ("the agent looks") a few times over ~half a
/// second — long enough to read as a deliberate acknowledgement, short enough not to
/// linger. `STEPS` is the number of toggles after the initial lit frame.
const PULSE_STEPS: u32 = 4;
const PULSE_INTERVAL: f64 = 0.1;

/// What [`Blinker`] needs to animate the icon, plus the little pulse state. Touched
/// only on the main thread (the pulse self-schedules onto the main run loop), so
/// plain `Cell`s suffice.
struct BlinkIvars {
    button: Retained<NSStatusBarButton>,
    idle: Retained<NSImage>,
    active: Retained<NSImage>,
    lit: Cell<bool>,
    remaining: Cell<u32>,
}

define_class!(
    // Owns the status-item button and drives the brief icon pulse that acknowledges
    // a received gesture. Lives on the main thread for the process life; reached from
    // other threads only via `performSelectorOnMainThread:`.
    #[unsafe(super(NSObject))]
    #[thread_kind = MainThreadOnly]
    #[name = "HiAgentTrayBlinker"]
    #[ivars = BlinkIvars]
    struct Blinker;

    unsafe impl NSObjectProtocol for Blinker {}

    impl Blinker {
        /// Start (or restart) the pulse. Invoked on the main thread via
        /// `performSelectorOnMainThread:`; rapid re-taps coalesce by cancelling any
        /// in-flight steps before re-arming.
        #[unsafe(method(flash:))]
        fn flash(&self, _arg: Option<&AnyObject>) {
            let iv = self.ivars();
            unsafe {
                let _: () = msg_send![
                    Self::class(),
                    cancelPreviousPerformRequestsWithTarget: self,
                    selector: sel!(step:),
                    object: core::ptr::null_mut::<AnyObject>()
                ];
            }
            iv.lit.set(true);
            iv.remaining.set(PULSE_STEPS);
            iv.button.setImage(Some(&iv.active));
            self.schedule_step();
        }

        /// One toggle of the pulse, re-scheduling itself until the budget runs out,
        /// then settling back to the resting icon.
        #[unsafe(method(step:))]
        fn step(&self, _arg: Option<&AnyObject>) {
            let iv = self.ivars();
            let r = iv.remaining.get();
            if r == 0 {
                iv.button.setImage(Some(&iv.idle));
                iv.lit.set(false);
                return;
            }
            let now_lit = !iv.lit.get();
            iv.lit.set(now_lit);
            iv.button.setImage(Some(if now_lit { &iv.active } else { &iv.idle }));
            iv.remaining.set(r - 1);
            self.schedule_step();
        }
    }
);

impl Blinker {
    fn new(
        mtm: MainThreadMarker,
        button: Retained<NSStatusBarButton>,
        idle: Retained<NSImage>,
        active: Retained<NSImage>,
    ) -> Retained<Self> {
        let this = Self::alloc(mtm).set_ivars(BlinkIvars {
            button,
            idle,
            active,
            lit: Cell::new(false),
            remaining: Cell::new(0),
        });
        unsafe { msg_send![super(this), init] }
    }

    /// Re-arm `step:` after [`PULSE_INTERVAL`]. Schedules onto the current run loop,
    /// so it must run on the main thread (it always does — see [`flash`]).
    fn schedule_step(&self) {
        unsafe {
            let _: () = msg_send![
                self,
                performSelector: sel!(step:),
                withObject: core::ptr::null_mut::<AnyObject>(),
                afterDelay: PULSE_INTERVAL
            ];
        }
    }
}

/// A raw pointer to the leaked [`Blinker`] so [`flash`] can reach it from any
/// thread. The object is main-thread-only, but we only ever message it via
/// `performSelectorOnMainThread:` — the sanctioned cross-thread call — so holding
/// and sharing the bare pointer is sound.
struct BlinkerPtr(*const Blinker);
unsafe impl Send for BlinkerPtr {}
unsafe impl Sync for BlinkerPtr {}

static BLINKER: OnceLock<BlinkerPtr> = OnceLock::new();

/// Briefly pulse the menu-bar icon to acknowledge a received gesture. Safe to call
/// from any thread; a no-op until the status item is up — so before the tray loads,
/// or when running headless, it simply does nothing.
pub fn flash() {
    let Some(blinker) = BLINKER.get() else { return };
    // SAFETY: `blinker.0` is the leaked Blinker, alive for the whole process. It is a
    // main-thread object, but `performSelectorOnMainThread:` is documented to be
    // callable from any thread — it hops `flash:` onto the main run loop, where the
    // pulse actually runs. We pass nil for the unused argument and don't block.
    unsafe {
        let obj: &Blinker = &*blinker.0;
        let _: () = msg_send![
            obj,
            performSelectorOnMainThread: sel!(flash:),
            withObject: core::ptr::null_mut::<AnyObject>(),
            waitUntilDone: false
        ];
    }
}

/// Build the status item + menu and run the AppKit loop on the current (main)
/// thread. **Blocks for the process lifetime.** Errors only if we are not on the
/// main thread; a missing window-server session surfaces as the item simply not
/// drawing (the caller already treats this path as best-effort).
pub fn run(url: String, shutdown: Arc<Notify>) -> anyhow::Result<()> {
    let mtm = MainThreadMarker::new()
        .ok_or_else(|| anyhow!("the menu bar must be set up on the main thread"))?;

    let app = NSApplication::sharedApplication(mtm);
    // Accessory: live in the menu bar only — no Dock icon, no app menu.
    app.setActivationPolicy(NSApplicationActivationPolicy::Accessory);

    let target = TrayTarget::new(mtm, url, shutdown);

    // SAFETY: all of these are standard AppKit setup calls made on the main thread
    // (guaranteed by `mtm`); the objects are kept alive by the locals below, which
    // live until `run` returns (i.e. never, in the normal exit-by-process path).
    unsafe {
        let status_bar = NSStatusBar::systemStatusBar();
        let status_item = status_bar.statusItemWithLength(NSVariableStatusItemLength);

        // The button carries the icon. The resting icon is the hi mark as a
        // *template* image (the menu bar auto-tints it for light/dark); fall back
        // to a short title if the embedded PNG ever fails to decode.
        if let Some(button) = status_item.button(mtm) {
            let desc = NSString::from_str("hi-agent");
            let idle = {
                let data = NSData::with_bytes(include_bytes!("assets/tray-hi.png"));
                NSImage::initWithData(NSImage::alloc(), &data)
            };
            match idle {
                Some(idle) => {
                    idle.setTemplate(true);
                    idle.setSize(NSSize {
                        width: 18.0,
                        height: 18.0,
                    });
                    button.setImage(Some(&idle));
                    // The "looking" frame of the come-and-see ack pulse: the hi mark
                    // briefly morphs to an eye and back. Falls back to the resting
                    // mark if the symbol is missing, so the pulse still runs.
                    let active = NSImage::imageWithSystemSymbolName_accessibilityDescription(
                        &NSString::from_str("eye"),
                        Some(&desc),
                    )
                    .unwrap_or_else(|| idle.clone());
                    active.setTemplate(true);
                    // Build the blinker, leak it (lives as long as the menu bar), and
                    // publish a pointer so the gesture can pulse it from its thread.
                    let blinker = Blinker::new(mtm, button, idle, active);
                    let ptr: *const Blinker = &*blinker;
                    std::mem::forget(blinker);
                    let _ = BLINKER.set(BlinkerPtr(ptr));
                }
                None => button.setTitle(&NSString::from_str("hi")),
            }
        }

        let menu = NSMenu::new(mtm);

        let open_item = NSMenuItem::initWithTitle_action_keyEquivalent(
            NSMenuItem::alloc(mtm),
            &NSString::from_str("Open hi-agent"),
            Some(sel!(open:)),
            &NSString::from_str(""),
        );
        open_item.setTarget(Some(&target));
        menu.addItem(&open_item);

        menu.addItem(&NSMenuItem::separatorItem(mtm));

        let quit_item = NSMenuItem::initWithTitle_action_keyEquivalent(
            NSMenuItem::alloc(mtm),
            &NSString::from_str("Quit hi-agent"),
            Some(sel!(quit:)),
            &NSString::from_str("q"),
        );
        quit_item.setTarget(Some(&target));
        menu.addItem(&quit_item);

        status_item.setMenu(Some(&menu));

        // Keep the status item and menu alive for the process lifetime. `run` below
        // never returns in the normal path (the process exits on Quit), so leaking
        // is intentional and bounded — they live exactly as long as the menu bar.
        std::mem::forget(status_item);
        std::mem::forget(menu);
    }
    // The target is held by the menu items only weakly (NSMenuItem.target is weak),
    // so leak our strong reference too, or the actions would fire on a freed object.
    std::mem::forget(target);

    // Drives the AppKit run loop on this thread; returns only if the app is stopped
    // (not our path — we exit the process instead).
    app.run();
    Ok(())
}
