//! macOS chat-popup vendor — the menu-bar conversation popover.
//!
//! An `NSPopover` anchored to the tray's status-item button, hosting the **native**
//! AppKit chat view ([`super::macos_chat`]). Opened by a left-click on the tray icon
//! ([`super::macos_tray`]) or a single right-⌘ tap ([`crate::body::gesture`]). Unlike
//! the attention feedback (which lives *on* the icon), this is an interactive control
//! surface: it takes focus and keystrokes, so the app is brought forward on show and
//! the popover is `transient` (an outside click dismisses it).
//!
//! Like the tray, the AppKit objects live on the process main thread and are leaked
//! for the process lifetime; cross-thread opens (the gesture runs off a background
//! thread) hop onto the main run loop via `performSelectorOnMainThread:`, exactly as
//! the tray's `flash` does.

use std::sync::OnceLock;

use objc2::rc::Retained;
use objc2::runtime::{AnyObject, NSObject, NSObjectProtocol, Sel};
use objc2::{DefinedClass, MainThreadOnly, define_class, msg_send, sel};
use objc2_app_kit::{NSApplication, NSPopover, NSPopoverBehavior, NSStatusBarButton, NSViewController};
use objc2_foundation::{MainThreadMarker, NSSize};

/// The popover's content size in points — a compact iMessage-style column.
pub(super) const POPOVER_W: f64 = 380.0;
pub(super) const POPOVER_H: f64 = 540.0;

/// `NSMinYEdge` (1) — anchor the popover to the bottom edge of the status button so
/// it drops below the menu bar toward the screen content.
const MIN_Y_EDGE: usize = 1;

/// What the host needs to show/hide the popover: the popover itself and the status
/// button it anchors to. Touched only on the main thread.
struct Ivars {
    popover: Retained<NSPopover>,
    button: Retained<NSStatusBarButton>,
}

define_class!(
    // Owns the chat popover + the status button it anchors to, and shows/toggles it
    // on the main thread. Reached from other threads only via
    // `performSelectorOnMainThread:`.
    #[unsafe(super(NSObject))]
    #[thread_kind = MainThreadOnly]
    #[name = "HiAgentChatPopoverHost"]
    #[ivars = Ivars]
    struct Host;

    unsafe impl NSObjectProtocol for Host {}

    impl Host {
        /// Show the popover (idempotent — keep an already-open one up).
        #[unsafe(method(open:))]
        fn open(&self, _arg: Option<&AnyObject>) {
            if self.ivars().popover.isShown() {
                return;
            }
            self.present();
        }

        /// Toggle: show if hidden, dismiss if shown.
        #[unsafe(method(toggle:))]
        fn toggle(&self, _arg: Option<&AnyObject>) {
            let pop = &self.ivars().popover;
            if pop.isShown() {
                // SAFETY: main-thread AppKit call (the host runs on the main thread).
                unsafe { pop.performClose(None) };
            } else {
                self.present();
            }
        }
    }
);

impl Host {
    /// Bring the app forward (so the chat input can take keys under the Accessory
    /// policy), show the popover anchored under the status button, and let the chat
    /// view seed itself on its first appearance.
    fn present(&self) {
        let mtm = MainThreadMarker::new().expect("popover host runs on the main thread");
        let iv = self.ivars();
        let app = NSApplication::sharedApplication(mtm);
        // SAFETY: main-thread AppKit calls. `activateIgnoringOtherApps:` is the
        // pre-Sonoma activation call but still works; the popover then becomes key so
        // its text field can take keystrokes despite the Accessory activation policy.
        unsafe {
            let _: () = msg_send![&*app, activateIgnoringOtherApps: true];
            let bounds = iv.button.bounds();
            let button: &NSStatusBarButton = &iv.button;
            let _: () = msg_send![
                &*iv.popover,
                showRelativeToRect: bounds,
                ofView: button,
                preferredEdge: MIN_Y_EDGE,
            ];
        }
        // First open seeds history + starts the live feed (guarded in the bridge).
        super::macos_chat::notify_opened();
        // Make the input the first responder so the user can type immediately.
        super::macos_chat::focus_input();
    }
}

/// A raw pointer to the leaked [`Host`] so the any-thread entry points can reach it.
/// Main-thread-only, but messaged only via `performSelectorOnMainThread:`, so sharing
/// the bare pointer across threads is sound (same contract as the tray's `Blinker`).
struct HostPtr(*const Host);
unsafe impl Send for HostPtr {}
unsafe impl Sync for HostPtr {}

static HOST: OnceLock<HostPtr> = OnceLock::new();

/// Build the chat popover hosting the native chat view, anchored to `button`.
/// Best-effort and called once from the tray's main-thread setup; the popover stays
/// hidden until opened.
pub fn install(mtm: MainThreadMarker, button: Retained<NSStatusBarButton>) {
    let view = super::macos_chat::make_view(mtm);
    // SAFETY: standard AppKit setup on the main thread (guaranteed by `mtm`). Every
    // object is kept alive past `install` by leaking it below, so none is used after
    // free; the popover + view live for the process, like the tray's items.
    unsafe {
        // A bare view controller whose view *is* the chat view — NSPopover needs a
        // contentViewController, not just a content view.
        let vc: Retained<NSViewController> = msg_send![NSViewController::alloc(mtm), init];
        let _: () = msg_send![&*vc, setView: &*view];

        let popover: Retained<NSPopover> = msg_send![NSPopover::alloc(mtm), init];
        popover.setContentSize(NSSize::new(POPOVER_W, POPOVER_H));
        popover.setBehavior(NSPopoverBehavior::Transient);
        popover.setAnimates(true);
        popover.setContentViewController(Some(&vc));

        let host = Host::alloc(mtm).set_ivars(Ivars { popover, button });
        let host: Retained<Host> = msg_send![super(host), init];
        let ptr: *const Host = &*host;
        std::mem::forget(host);
        std::mem::forget(vc);
        let _ = HOST.set(HostPtr(ptr));
    }
    tracing::info!("chat popover installed");
}

/// Show the chat popover (idempotent). Safe to call from any thread — a no-op until
/// the popover is installed (headless / before the tray loads).
pub fn open() {
    hop(sel!(open:));
}

/// Toggle the chat popover (show / dismiss). Safe to call from any thread; a no-op
/// until the popover is installed.
pub fn toggle() {
    hop(sel!(toggle:));
}

/// Hop `selector` (`open:` / `toggle:`) onto the main run loop where the popover
/// lives. A no-op until [`install`] has published the host.
fn hop(selector: Sel) {
    let Some(host) = HOST.get() else { return };
    // SAFETY: `host.0` is the leaked, process-lived `Host`; `performSelectorOnMainThread:`
    // is callable from any thread and hops the selector onto the main run loop.
    unsafe {
        let obj: &Host = &*host.0;
        let _: () = msg_send![
            obj,
            performSelectorOnMainThread: selector,
            withObject: core::ptr::null_mut::<AnyObject>(),
            waitUntilDone: false
        ];
    }
}
