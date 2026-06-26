//! macOS popover vendor — the menu-bar window that hosts the agent **face**.
//!
//! An `NSPopover` anchored to the tray's status-item button, hosting a `WKWebView`
//! that loads the face (`/`) — the same appearance UI as the main desktop window, just
//! in a popover. Opened by a left-click on the tray icon ([`super::macos_tray`]) or a
//! single right-⌘ tap ([`crate::body::gesture`]). It's an interactive surface: it takes
//! focus and keystrokes, so the app is brought forward on show and the popover is
//! `transient` (an outside click dismisses it).
//!
//! Media permission: the popover is a trusted local surface, so a [`MediaGrant`]
//! `WKUIDelegate` **auto-grants** the page's mic/camera requests — WebKit's per-site
//! prompt never shows. (The first-use macOS *system* prompt is a separate OS gate and
//! still fires once; no app can self-grant that.)
//!
//! Like the tray, the AppKit objects live on the process main thread and are leaked for
//! the process lifetime; cross-thread opens (the gesture runs off a background thread)
//! hop onto the main run loop via `performSelectorOnMainThread:`. `WKWebView` enforces
//! App Transport Security, so a bundled `.app` needs an `NSAllowsLocalNetworking`
//! exception for the `http://127.0.0.1` page.

use std::sync::OnceLock;

use objc2::rc::Retained;
use objc2::runtime::{AnyObject, NSObject, NSObjectProtocol, ProtocolObject, Sel};
use objc2::{DefinedClass, MainThreadOnly, define_class, msg_send, sel};
use objc2_app_kit::{NSApplication, NSPopover, NSPopoverBehavior, NSStatusBarButton, NSViewController};
use objc2_foundation::{MainThreadMarker, NSPoint, NSRect, NSSize, NSString, NSURL, NSURLRequest};
use objc2_web_kit::{
    WKFrameInfo, WKMediaCaptureType, WKPermissionDecision, WKSecurityOrigin, WKUIDelegate, WKWebView,
    WKWebViewConfiguration,
};

/// The popover's content size in points — a compact column for the face.
const POPOVER_W: f64 = 380.0;
const POPOVER_H: f64 = 540.0;

/// `NSMinYEdge` (1) — anchor the popover to the bottom edge of the status button so it
/// drops below the menu bar toward the screen content.
const MIN_Y_EDGE: usize = 1;

// ---------------------------------------------------------------------------
// Media-permission delegate — auto-grant so WebKit never prompts per-site
// ---------------------------------------------------------------------------

define_class!(
    // A WKUIDelegate that auto-grants camera/microphone for the page. The popover is
    // our own trusted local surface, so there's no per-site prompt to show; the macOS
    // system permission (TCC) is separate and still asks once on first real use.
    #[unsafe(super(NSObject))]
    #[thread_kind = MainThreadOnly]
    #[name = "HiAgentMediaGrant"]
    struct MediaGrant;

    unsafe impl NSObjectProtocol for MediaGrant {}

    unsafe impl WKUIDelegate for MediaGrant {
        #[unsafe(method(webView:requestMediaCapturePermissionForOrigin:initiatedByFrame:type:decisionHandler:))]
        fn request_media_capture_permission(
            &self,
            _web_view: &WKWebView,
            _origin: &WKSecurityOrigin,
            _frame: &WKFrameInfo,
            _capture_type: WKMediaCaptureType,
            decision_handler: &block2::DynBlock<dyn Fn(WKPermissionDecision)>,
        ) {
            decision_handler.call((WKPermissionDecision::Grant,));
        }
    }
);

/// What the host needs to show/hide the popover: the popover itself and the status
/// button it anchors to. Touched only on the main thread.
struct Ivars {
    popover: Retained<NSPopover>,
    button: Retained<NSStatusBarButton>,
}

define_class!(
    // Owns the popover + the status button it anchors to, and shows/toggles it on the
    // main thread. Reached from other threads only via `performSelectorOnMainThread:`.
    #[unsafe(super(NSObject))]
    #[thread_kind = MainThreadOnly]
    #[name = "HiAgentPopoverHost"]
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
    /// Bring the app forward (so the face's input can take keys under the Accessory
    /// policy) and show the popover anchored under the status button.
    fn present(&self) {
        let mtm = MainThreadMarker::new().expect("popover host runs on the main thread");
        let iv = self.ivars();
        let app = NSApplication::sharedApplication(mtm);
        // SAFETY: main-thread AppKit calls. `activateIgnoringOtherApps:` is the pre-Sonoma
        // activation call but still works; the popover then becomes key so its web view's
        // input can take keystrokes despite the Accessory activation policy.
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
    }
}

/// A raw pointer to the leaked [`Host`] so the any-thread entry points can reach it.
/// Main-thread-only, but messaged only via `performSelectorOnMainThread:`, so sharing
/// the bare pointer across threads is sound (same contract as the tray's `Blinker`).
struct HostPtr(*const Host);
unsafe impl Send for HostPtr {}
unsafe impl Sync for HostPtr {}

static HOST: OnceLock<HostPtr> = OnceLock::new();

/// Build the popover hosting a `WKWebView` on the face, anchored to `button`.
/// Best-effort and called once from the tray's main-thread setup; the web view loads
/// `url` (the face base, `http://127.0.0.1:{port}/`) and the popover stays hidden until
/// opened.
pub fn install(mtm: MainThreadMarker, button: Retained<NSStatusBarButton>, url: &str) {
    // SAFETY: standard AppKit/WebKit setup on the main thread (guaranteed by `mtm`).
    // Every object is kept alive past `install` by leaking it below, so none is used
    // after free; they live for the process, like the tray's items.
    unsafe {
        let config = WKWebViewConfiguration::new(mtm);
        let frame = NSRect::new(NSPoint::new(0.0, 0.0), NSSize::new(POPOVER_W, POPOVER_H));
        let webview: Retained<WKWebView> =
            WKWebView::initWithFrame_configuration(WKWebView::alloc(mtm), frame, &config);

        // Auto-grant the page's mic/camera (no per-site WebKit prompt). The delegate is
        // weakly held by the web view, so leak our strong reference to keep it alive.
        let media: Retained<MediaGrant> = msg_send![MediaGrant::alloc(mtm), init];
        webview.setUIDelegate(Some(ProtocolObject::from_ref(&*media)));
        std::mem::forget(media);

        if let Some(nsurl) = NSURL::URLWithString(&NSString::from_str(url)) {
            let req = NSURLRequest::requestWithURL(&nsurl);
            let _: Option<Retained<objc2_web_kit::WKNavigation>> = webview.loadRequest(&req);
        } else {
            tracing::warn!(url, "popover: bad URL; nothing to load");
        }

        // A bare view controller whose view *is* the web view — NSPopover needs a
        // contentViewController, not just a content view.
        let vc: Retained<NSViewController> = msg_send![NSViewController::alloc(mtm), init];
        let _: () = msg_send![&*vc, setView: &*webview];

        let popover: Retained<NSPopover> = msg_send![NSPopover::alloc(mtm), init];
        popover.setContentSize(NSSize::new(POPOVER_W, POPOVER_H));
        popover.setBehavior(NSPopoverBehavior::Transient);
        popover.setAnimates(true);
        popover.setContentViewController(Some(&vc));

        let host = Host::alloc(mtm).set_ivars(Ivars { popover, button });
        let host: Retained<Host> = msg_send![super(host), init];
        let ptr: *const Host = &*host;
        std::mem::forget(host);
        std::mem::forget(webview);
        std::mem::forget(config);
        std::mem::forget(vc);
        let _ = HOST.set(HostPtr(ptr));
    }
    tracing::info!("face popover installed");
}

/// Show the popover (idempotent). Safe to call from any thread — a no-op until the
/// popover is installed (headless / before the tray loads).
pub fn open() {
    hop(sel!(open:));
}

/// Toggle the popover (show / dismiss). Safe to call from any thread; a no-op until the
/// popover is installed.
pub fn toggle() {
    hop(sel!(toggle:));
}

/// Hop `selector` (`open:` / `toggle:`) onto the main run loop where the popover lives.
/// A no-op until [`install`] has published the host.
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
