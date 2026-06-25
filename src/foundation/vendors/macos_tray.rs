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

/// The glance ack: on a double-tap the menu-bar icon plays a short scripted
/// sequence — **resting "hi" → blink → full-colour "hi" (brief stay) → blink →
/// resting "hi"** — where each "blink" is a quick transparent frame (the icon
/// winks off and back, keeping its width so neighbours don't shift). Long enough
/// to read as a deliberate "I looked", short enough not to linger.
#[derive(Clone, Copy)]
enum Frame {
    /// The resting template mark (menu-bar-tinted).
    Idle,
    /// The full-colour mark (red "h" + blue "i").
    Colour,
    /// A transparent frame — the icon winks off (the "blink").
    Blank,
}

/// The glance script: `(frame to show, seconds to hold it before the next)`. The
/// last frame ends the sequence (its hold time is unused). It starts from the
/// resting icon, so the leading Blank reads as a blink *off* the resting mark.
const GLANCE: &[(Frame, f64)] = &[
    (Frame::Blank, 0.10),  // wink off the resting "hi"
    (Frame::Colour, 0.60), // bloom to colour, brief stay
    (Frame::Blank, 0.10),  // wink off the colour
    (Frame::Idle, 0.0),    // back to resting
];

/// What [`Blinker`] needs to drive the icon. Touched only on the main thread (it
/// self-schedules onto the main run loop), so plain `Cell`s suffice.
struct BlinkIvars {
    button: Retained<NSStatusBarButton>,
    idle: Retained<NSImage>,
    active: Retained<NSImage>,
    blank: Retained<NSImage>,
    /// While true (⌘ held for press-hold attention) the icon stays at full colour;
    /// a glance can't fire mid-hold (you can't double-tap while holding ⌘).
    holding: Cell<bool>,
    /// Cursor into [`GLANCE`] while a glance sequence is playing.
    step: Cell<usize>,
}

define_class!(
    // Owns the status-item button and drives the glance sequence + the sustained
    // listening colour. Lives on the main thread for the process life; reached from
    // other threads only via `performSelectorOnMainThread:`.
    #[unsafe(super(NSObject))]
    #[thread_kind = MainThreadOnly]
    #[name = "HiAgentTrayBlinker"]
    #[ivars = BlinkIvars]
    struct Blinker;

    unsafe impl NSObjectProtocol for Blinker {}

    impl Blinker {
        /// Start (or restart) the glance sequence. Invoked on the main thread via
        /// `performSelectorOnMainThread:`; rapid re-taps coalesce by cancelling any
        /// in-flight steps before re-arming.
        #[unsafe(method(flash:))]
        fn flash(&self, _arg: Option<&AnyObject>) {
            self.cancel_pending();
            self.ivars().step.set(0);
            self.play();
        }

        /// Advance the glance sequence by one frame.
        #[unsafe(method(step:))]
        fn step(&self, _arg: Option<&AnyObject>) {
            self.play();
        }

        /// Enter the sustained "listening" state: the icon holds at full colour for
        /// as long as ⌘ is held (press-hold attention). Cancels any glance in flight.
        #[unsafe(method(listenOn:))]
        fn listen_on(&self, _arg: Option<&AnyObject>) {
            self.cancel_pending();
            self.ivars().holding.set(true);
            self.ivars().button.setImage(Some(&self.ivars().active));
        }

        /// Leave the listening state: settle back to the resting mark.
        #[unsafe(method(listenOff:))]
        fn listen_off(&self, _arg: Option<&AnyObject>) {
            self.cancel_pending();
            self.ivars().holding.set(false);
            self.ivars().button.setImage(Some(&self.ivars().idle));
        }
    }
);

impl Blinker {
    fn new(
        mtm: MainThreadMarker,
        button: Retained<NSStatusBarButton>,
        idle: Retained<NSImage>,
        active: Retained<NSImage>,
        blank: Retained<NSImage>,
    ) -> Retained<Self> {
        let this = Self::alloc(mtm).set_ivars(BlinkIvars {
            button,
            idle,
            active,
            blank,
            holding: Cell::new(false),
            step: Cell::new(0),
        });
        unsafe { msg_send![super(this), init] }
    }

    /// Cancel any scheduled `step:` so a re-tap or a listening change can't collide
    /// with an in-flight glance sequence.
    fn cancel_pending(&self) {
        unsafe {
            let _: () = msg_send![
                Self::class(),
                cancelPreviousPerformRequestsWithTarget: self,
                selector: sel!(step:),
                object: core::ptr::null_mut::<AnyObject>()
            ];
        }
    }

    /// Show the current glance frame and, if more remain, schedule the next after
    /// its hold time. Runs on the main thread (see [`flash`] / [`step`]).
    fn play(&self) {
        let iv = self.ivars();
        let i = iv.step.get();
        let Some(&(frame, delay)) = GLANCE.get(i) else { return };
        let img = match frame {
            Frame::Idle => &iv.idle,
            Frame::Colour => &iv.active,
            Frame::Blank => &iv.blank,
        };
        iv.button.setImage(Some(img));
        if i + 1 < GLANCE.len() {
            iv.step.set(i + 1);
            unsafe {
                let _: () = msg_send![
                    self,
                    performSelector: sel!(step:),
                    withObject: core::ptr::null_mut::<AnyObject>(),
                    afterDelay: delay
                ];
            }
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

/// Enter (`true`) or leave (`false`) the sustained "listening" tray state for the
/// press-hold attention gesture — the icon holds at its full colour while ⌘ is
/// held, then settles back. Safe to call from any thread; a no-op until the status
/// item is up (headless / before the tray loads).
pub fn set_listening(on: bool) {
    let Some(blinker) = BLINKER.get() else { return };
    // SAFETY: same contract as `flash` — `blinker.0` is the leaked, process-lived
    // Blinker, messaged only via `performSelectorOnMainThread:` (callable from any
    // thread), which hops the toggle onto the main run loop.
    unsafe {
        let obj: &Blinker = &*blinker.0;
        let sel = if on { sel!(listenOn:) } else { sel!(listenOff:) };
        let _: () = msg_send![
            obj,
            performSelectorOnMainThread: sel,
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
                    // The "lit" frame of the gesture ack: the hi mark briefly
                    // blooms to its full colour (red "h" + blue "i") and blinks
                    // back to the resting template — the menu-bar echo of the
                    // double-tap glance. Falls back to the resting mark if the
                    // colour PNG ever fails to decode, so the pulse still runs.
                    let active = {
                        let data = NSData::with_bytes(include_bytes!("assets/tray-hi-color.png"));
                        NSImage::initWithData(NSImage::alloc(), &data)
                    }
                    .unwrap_or_else(|| idle.clone());
                    // Colourful, NOT a template — keep the red/blue (a template
                    // would be flattened to a single menu-bar tint).
                    active.setTemplate(false);
                    active.setSize(NSSize {
                        width: 18.0,
                        height: 18.0,
                    });
                    // The "blink" frame: a transparent mark of the same size, so the
                    // icon winks fully off mid-glance without the status item
                    // collapsing its width (which would jostle the neighbours).
                    let blank = {
                        let data = NSData::with_bytes(include_bytes!("assets/tray-hi-blank.png"));
                        NSImage::initWithData(NSImage::alloc(), &data)
                    }
                    .unwrap_or_else(|| idle.clone());
                    blank.setSize(NSSize {
                        width: 18.0,
                        height: 18.0,
                    });
                    // Build the blinker, leak it (lives as long as the menu bar), and
                    // publish a pointer so the gesture can drive it from its thread.
                    let blinker = Blinker::new(mtm, button, idle, active, blank);
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
            &NSString::from_str("Open Hi Agent"),
            Some(sel!(open:)),
            &NSString::from_str(""),
        );
        open_item.setTarget(Some(&target));
        menu.addItem(&open_item);

        menu.addItem(&NSMenuItem::separatorItem(mtm));

        let quit_item = NSMenuItem::initWithTitle_action_keyEquivalent(
            NSMenuItem::alloc(mtm),
            &NSString::from_str("Quit Hi Agent"),
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
