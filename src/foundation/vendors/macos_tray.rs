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
//!
//! ## What you can and can't do with the macOS menu bar (so we don't relitigate it)
//!
//! The bar is two regions with very different rules:
//!
//! - **Left — the active app's menus** (Apple menu, app name, File/Edit/…). Owned
//!   by the frontmost app + the system. There is **no public API to inject your own
//!   clickable entries** into another app's menus. That is the *only* genuinely
//!   off-limits thing, and only as *menu content*.
//! - **Right — the status region.** Yours, via `NSStatusItem`. A status item is far
//!   more than a 22pt icon: its length can be **variable** (auto-sizes to content)
//!   and its `button` can carry **text and/or a custom `NSView`**, not just an image.
//!   The "now playing / lyrics" tickers some apps show in the bar are exactly this —
//!   a wide status item rendering text. So we *can* show the attention transcript
//!   right here (icon + title), which is what `set_text` below does.
//!
//! **Pixels *over* the bar are not restricted either.** You can float a borderless
//! window over *any* part of the menu bar (left included) — that's how menu-bar
//! managers (Bartender, Ice) and notch apps draw custom bars. The catch is the
//! **window level**: it must sit *above* the menu bar. `NSStatusWindowLevel` (25) is
//! NOT enough — it renders *under* the modern menu bar (an earlier overlay of ours
//! showed down in the content area for exactly this reason). Those managers also use
//! the **Accessibility API** to read/move *other apps'* status items — that's the
//! part that needs the scary permission; rendering only our own item/window does not.
//!
//! Net: two clean ways to put our own UI up top — a rich status item (what this file
//! does) or a high-level borderless overlay window. The one true wall is injecting
//! into another app's *menus*.

use std::cell::Cell;
use std::sync::{Arc, Mutex, OnceLock};

use anyhow::anyhow;
use objc2::rc::Retained;
use objc2::runtime::{AnyObject, NSObject, NSObjectProtocol};
use objc2::{define_class, msg_send, sel, AnyThread, ClassType, DefinedClass, MainThreadOnly};
use objc2_app_kit::{
    NSApplication, NSApplicationActivationPolicy, NSCellImagePosition, NSEventModifierFlags,
    NSEventType, NSImage, NSMenu, NSMenuItem, NSStatusBar, NSStatusBarButton, NSStatusItem,
    NSVariableStatusItemLength,
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

        /// Set the status item's text (the title beside the icon) — the live
        /// attention transcript, then the reply. A non-empty string widens the
        /// item to show `icon + text`; an empty string collapses back to icon-only.
        #[unsafe(method(setText:))]
        fn set_text(&self, text: Option<&NSString>) {
            let button = &self.ivars().button;
            match text {
                Some(t) if t.length() > 0 => {
                    button.setTitle(t);
                    button.setImagePosition(NSCellImagePosition::ImageLeft);
                }
                _ => {
                    button.setTitle(&NSString::from_str(""));
                    button.setImagePosition(NSCellImagePosition::ImageOnly);
                }
            }
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

/// The latest text requested via [`set_text`], kept so a request that lands before
/// the status item exists (e.g. an early config error surfaced from the server
/// thread, which can win the race against this thread's AppKit setup) isn't
/// dropped — [`run`] applies it once the item is up.
static PENDING_TEXT: Mutex<Option<String>> = Mutex::new(None);

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

/// Set the status item's text (the title beside the icon) — empty string collapses
/// to icon-only. Safe to call from any thread; a no-op until the status item is up.
pub fn set_text(text: &str) {
    // Record it first so a call that beats the status item's creation isn't lost —
    // `run` flushes this once the item is up.
    *PENDING_TEXT.lock().unwrap() = Some(text.to_string());
    let Some(blinker) = BLINKER.get() else { return };
    // The NSString is created here (off the main thread, which is fine for an
    // immutable string) and retained by `performSelectorOnMainThread:` until the
    // `setText:` hop runs on the main thread.
    let ns = NSString::from_str(text);
    // SAFETY: same contract as `flash` / `set_listening`.
    unsafe {
        let obj: &Blinker = &*blinker.0;
        let _: () = msg_send![
            obj,
            performSelectorOnMainThread: sel!(setText:),
            withObject: &*ns,
            waitUntilDone: false
        ];
    }
}

/// What [`TrayClick`] needs to route a click: the status item (to attach the menu
/// just for a right-click), its button, and the Open/Quit menu.
struct ClickIvars {
    status_item: Retained<NSStatusItem>,
    button: Retained<NSStatusBarButton>,
    menu: Retained<NSMenu>,
}

define_class!(
    // The status-item button's click target. Left-click toggles the chat popover;
    // right-/control-click shows the Open/Quit menu. Lives on the main thread.
    #[unsafe(super(NSObject))]
    #[thread_kind = MainThreadOnly]
    #[name = "HiAgentTrayClick"]
    #[ivars = ClickIvars]
    struct TrayClick;

    unsafe impl NSObjectProtocol for TrayClick {}

    impl TrayClick {
        /// The button's action (fired on left *and* right mouse-up — see
        /// `sendActionOn:` in [`run`]). A primary click toggles the chat popover; a
        /// secondary click (right button, or control-click) shows the Open/Quit menu.
        ///
        /// We never keep a permanent `statusItem.menu` — that would hijack *every*
        /// click into the menu and the button action would never fire — so the menu
        /// is attached for this one secondary click (`performClick` shows it because a
        /// menu is set) and detached again immediately after.
        #[unsafe(method(click:))]
        fn click(&self, _sender: Option<&AnyObject>) {
            let mtm = MainThreadMarker::new().expect("tray click runs on the main thread");
            let app = NSApplication::sharedApplication(mtm);
            let secondary = app.currentEvent().is_some_and(|ev| {
                let kind = ev.r#type();
                kind == NSEventType::RightMouseUp
                    || (kind == NSEventType::LeftMouseUp
                        && ev.modifierFlags().contains(NSEventModifierFlags::Control))
            });
            let iv = self.ivars();
            if secondary {
                iv.status_item.setMenu(Some(&iv.menu));
                // SAFETY: main-thread NSControl call; with a menu set, this pops the
                // menu rather than re-firing the action, so there's no recursion.
                unsafe {
                    let _: () =
                        msg_send![&*iv.button, performClick: core::ptr::null_mut::<AnyObject>()];
                }
                iv.status_item.setMenu(None);
            } else {
                crate::foundation::vendors::macos_popover::toggle();
            }
        }
    }
);

impl TrayClick {
    fn new(
        mtm: MainThreadMarker,
        status_item: Retained<NSStatusItem>,
        button: Retained<NSStatusBarButton>,
        menu: Retained<NSMenu>,
    ) -> Retained<Self> {
        let this = Self::alloc(mtm).set_ivars(ClickIvars { status_item, button, menu });
        unsafe { msg_send![super(this), init] }
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

    // The popover's web view loads the face at this base URL. In dev, `make dev` sets
    // `HI_AGENT_POPOVER_URL=http://127.0.0.1:5173/` so the popover talks to the Vite dev
    // server (live web, hot-reload) instead of the binary's embedded `dist/`; in prod the
    // var is unset and it falls back to the binary's own port (there is no Vite). Derived
    // before `url` is moved into the menu's "Open" target below.
    let popover_url = std::env::var("HI_AGENT_POPOVER_URL").unwrap_or_else(|_| url.clone());

    let target = TrayTarget::new(mtm, url, shutdown);

    // SAFETY: all of these are standard AppKit setup calls made on the main thread
    // (guaranteed by `mtm`); the objects are kept alive by the locals below, which
    // live until `run` returns (i.e. never, in the normal exit-by-process path).
    unsafe {
        let status_bar = NSStatusBar::systemStatusBar();
        let status_item = status_bar.statusItemWithLength(NSVariableStatusItemLength);

        // The button carries the icon. The resting icon is the hi mark as a
        // *template* image (the menu bar auto-tints it for light/dark); fall back
        // to a short title if the embedded PNG ever fails to decode. Held in an
        // `Option` (not consumed inline) so the click wiring + popover anchor below
        // can share it with the blinker.
        let button = status_item.button(mtm);
        if let Some(button) = &button {
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
                    // Resting state is icon-only; `set_text` flips this to
                    // image-left when there's attention text to show beside it.
                    button.setImagePosition(NSCellImagePosition::ImageOnly);
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
                    // Build the blinker on a clone of the button (the click wiring +
                    // popover anchor below keep the original), leak it, and publish a
                    // pointer so the gesture can drive it from its thread.
                    let blinker = Blinker::new(mtm, button.clone(), idle, active, blank);
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

        // A left-click on the icon toggles the chat popover; a right-/control-click
        // shows the menu above. So the button drives a `TrayClick` action rather than
        // owning the menu permanently (a permanent menu would route *every* click to
        // the menu and the button action would never fire).
        if let Some(button) = &button {
            crate::foundation::vendors::macos_popover::install(mtm, button.clone(), &popover_url);

            let click = TrayClick::new(mtm, status_item.clone(), button.clone(), menu.clone());
            button.setTarget(Some(&click));
            button.setAction(Some(sel!(click:)));
            // Fire the action on left *and* right mouse-up so a secondary click also
            // reaches `click:` (NSEventMaskLeftMouseUp | NSEventMaskRightMouseUp).
            let mask: u64 = (1 << 2) | (1 << 4);
            let _: isize = msg_send![&**button, sendActionOn: mask];
            // The button's target is weak — leak our strong ref so the action target
            // outlives this function (mirrors `target` below).
            std::mem::forget(click);
        }

        // Keep the status item and menu alive for the process lifetime. `run` below
        // never returns in the normal path (the process exits on Quit), so leaking
        // is intentional and bounded — they live exactly as long as the menu bar.
        std::mem::forget(status_item);
        std::mem::forget(menu);
    }
    // The target is held by the menu items only weakly (NSMenuItem.target is weak),
    // so leak our strong reference too, or the actions would fire on a freed object.
    std::mem::forget(target);

    // Apply any text requested before the status item existed — e.g. an early
    // config error from the server thread that won the race with this setup. Bind to
    // a local first so the lock guard is dropped before `set_text` re-locks.
    let pending = PENDING_TEXT.lock().unwrap().clone();
    if let Some(text) = pending {
        set_text(&text);
    }

    // Drives the AppKit run loop on this thread; returns only if the app is stopped
    // (not our path — we exit the process instead).
    app.run();
    Ok(())
}
