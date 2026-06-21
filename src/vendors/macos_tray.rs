//! macOS tray vendor — a menu-bar `NSStatusItem` with an "Open" / "Quit" menu, the
//! visible affordance of the desktop install shape ([`crate::capabilities::tray`]).
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

use std::sync::Arc;

use anyhow::anyhow;
use objc2::rc::Retained;
use objc2::runtime::{AnyObject, NSObject, NSObjectProtocol};
use objc2::{define_class, msg_send, sel, DefinedClass, MainThreadOnly};
use objc2_app_kit::{
    NSApplication, NSApplicationActivationPolicy, NSImage, NSMenu, NSMenuItem, NSStatusBar,
    NSVariableStatusItemLength,
};
use objc2_foundation::{MainThreadMarker, NSString};
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

        // The button carries the icon. Prefer an SF Symbol as a *template* image so
        // the menu bar auto-tints it for light/dark; fall back to a short title.
        if let Some(button) = status_item.button(mtm) {
            let symbol = NSString::from_str("sparkles");
            let desc = NSString::from_str("hi-agent");
            match NSImage::imageWithSystemSymbolName_accessibilityDescription(&symbol, Some(&desc)) {
                Some(image) => {
                    image.setTemplate(true);
                    button.setImage(Some(&image));
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
