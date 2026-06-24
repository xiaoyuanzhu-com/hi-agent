//! macOS attention-overlay vendor — the system-wide visual feedback for the ⌘
//! gestures ([`crate::body::gesture`]).
//!
//! A borderless, **non-activating, click-through, always-on-top** `NSPanel`
//! spanning the top of the main screen, hosting a **transparent** `WKWebView`
//! that loads `/attention` and animates from `GET /api/out/attention`. Unlike the
//! tray, it isn't a control surface — it never takes focus and passes every click
//! through to whatever is beneath it; it's pure feedback.
//!
//! Like the tray ([`super::macos_tray`]), the AppKit objects live on the process
//! main thread: [`install`] is called from the tray's main-thread setup, before
//! the AppKit loop runs, and leaks the panel + web view so they live for the
//! process. The panel is **persistent** — it draws nothing when idle (the page is
//! transparent), so it can sit over the menu bar without disturbing it, and all
//! show/animate/hide logic lives in the web page driven by the event stream.
//!
//! On-device tuning points (can't be verified headless / over SSH — see the
//! GUI-session wall): the window **level** must sit above the menu bar and the
//! notch's flanks; the strip **height** should match the real menu-bar height;
//! and `WKWebView` enforces App Transport Security, so the bundled `.app` needs an
//! `NSAllowsLocalNetworking` exception for the `http://127.0.0.1` page + stream.

use objc2::rc::Retained;
use objc2::{MainThreadOnly, msg_send};
use objc2_app_kit::{
    NSBackingStoreType, NSColor, NSPanel, NSScreen, NSWindowCollectionBehavior, NSWindowStyleMask,
};
use objc2_foundation::{
    MainThreadMarker, NSNumber, NSPoint, NSRect, NSSize, NSString, NSURL, NSURLRequest,
};
use objc2_web_kit::{WKWebView, WKWebViewConfiguration};

/// Window level for the overlay. `NSStatusWindowLevel` (25) sits with the menu
/// bar's status items, so the strip draws over the menu bar and the notch flanks.
/// Tuned on-device if it needs to be higher (e.g. over full-screen chrome).
const OVERLAY_WINDOW_LEVEL: isize = 25;

/// Strip height in points — matches the web page's `--mb`. The menu bar is ~24pt
/// (no notch) to ~37pt (notch); a fixed value keeps native + web in lockstep and
/// is refined on-device.
const STRIP_HEIGHT: f64 = 32.0;

/// Create the attention overlay panel + web view and order it in. Best-effort:
/// with no main screen (headless / no window server) it logs and returns, leaving
/// the agent to run without the overlay — exactly like the tray falls back.
/// Must be called on the main thread (the caller holds the `MainThreadMarker`).
pub fn install(mtm: MainThreadMarker, base_url: &str) {
    let Some(screen) = NSScreen::mainScreen(mtm) else {
        tracing::warn!("attention overlay: no main screen; running without it");
        return;
    };
    let frame = screen.frame();
    // Cocoa is bottom-left origin: put the strip's origin at the very top edge.
    let strip = NSRect::new(
        NSPoint::new(frame.origin.x, frame.origin.y + frame.size.height - STRIP_HEIGHT),
        NSSize::new(frame.size.width, STRIP_HEIGHT),
    );

    // SAFETY: standard AppKit/WebKit setup on the main thread (guaranteed by
    // `mtm`). Every object is kept alive past `install` by leaking it below, so
    // none is used after free; the panel + web view live for the process, like
    // the tray's status item.
    unsafe {
        // Borderless, non-activating panel — it never becomes key/main, so it can
        // never steal focus from the app the user is actually working in.
        let style = NSWindowStyleMask::Borderless | NSWindowStyleMask::NonactivatingPanel;
        let panel: Retained<NSPanel> = msg_send![
            NSPanel::alloc(mtm),
            initWithContentRect: strip,
            styleMask: style,
            backing: NSBackingStoreType::Buffered,
            defer: false,
        ];

        panel.setOpaque(false);
        panel.setBackgroundColor(Some(&NSColor::clearColor()));
        panel.setLevel(OVERLAY_WINDOW_LEVEL);
        panel.setIgnoresMouseEvents(true); // click-through: pure feedback
        panel.setHasShadow(false);
        panel.setHidesOnDeactivate(false); // stay up while another app is focused
        panel.setFloatingPanel(true);
        // Float over every Space and survive full-screen apps; never cycle into ⌘`.
        panel.setCollectionBehavior(
            NSWindowCollectionBehavior::CanJoinAllSpaces
                | NSWindowCollectionBehavior::Stationary
                | NSWindowCollectionBehavior::FullScreenAuxiliary
                | NSWindowCollectionBehavior::IgnoresCycle,
        );

        // A transparent web view filling the strip renders the actual feedback.
        let config = WKWebViewConfiguration::new(mtm);
        let bounds = NSRect::new(NSPoint::new(0.0, 0.0), strip.size);
        let webview: Retained<WKWebView> =
            WKWebView::initWithFrame_configuration(WKWebView::alloc(mtm), bounds, &config);
        // Don't paint a white page background — let the panel's clear show through
        // so only the strip's own pixels are visible (KVC; the public API has no
        // equivalent for the page's drawn background).
        let no = NSNumber::numberWithBool(false);
        let _: () = msg_send![&*webview, setValue: &*no, forKey: &*NSString::from_str("drawsBackground")];

        let url_str = format!("{base_url}attention");
        if let Some(url) = NSURL::URLWithString(&NSString::from_str(&url_str)) {
            let req = NSURLRequest::requestWithURL(&url);
            let _: Option<Retained<objc2_web_kit::WKNavigation>> = webview.loadRequest(&req);
        } else {
            tracing::warn!(url = %url_str, "attention overlay: bad overlay URL; nothing to load");
        }

        let _: () = msg_send![&*panel, setContentView: &*webview];
        panel.orderFrontRegardless(); // show without taking key/main

        // Live for the process lifetime (mirrors the tray's leaked status item).
        std::mem::forget(panel);
        std::mem::forget(webview);
        std::mem::forget(config);
    }

    tracing::info!(
        level = OVERLAY_WINDOW_LEVEL,
        height = STRIP_HEIGHT,
        "attention overlay installed"
    );
}
