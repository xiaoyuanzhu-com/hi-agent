//! macOS window vendor — the dedicated desktop window that hosts the agent **face**.
//!
//! A borderless `NSWindow` hosting a `WKWebView` that loads the face (`/`) — the same
//! appearance UI the tray popover used to show, but as a free-standing, movable window
//! instead of a menu-bar popup. Opened by a left-click on the tray icon, the "Open Hi
//! Agent" menu item, or a single right-⌘ tap ([`crate::body::gesture`]).
//!
//! **Borderless, but a real interactive window.** A borderless `NSWindow` refuses to
//! become key by default, which would leave the web view's text input dead under the
//! Accessory activation policy — so [`KeyWindow`] overrides `canBecomeKeyWindow` /
//! `canBecomeMainWindow`. Dragging is by the window background (there's no titlebar to
//! grab) and the mask carries `Resizable` so the edges still resize. To swap the naked
//! borderless look for a native-chrome-hidden window instead (traffic-light-free but
//! with system corner rounding, drag, and resize), give the window the
//! `Titled | Closable | Miniaturizable | Resizable | FullSizeContentView` mask, set
//! `titlebarAppearsTransparent`/`titleVisibility(Hidden)`, and hide the standard
//! buttons — the web view still fills to all four edges.
//!
//! Media permission mirrors the popover: a [`MediaGrant`] `WKUIDelegate` auto-grants
//! the page's mic/camera so WebKit never shows its per-site prompt. (The macOS *system*
//! TCC prompt is separate and still fires once on first real use.)
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
use objc2_app_kit::{NSApplication, NSBackingStoreType, NSWindow, NSWindowStyleMask};
use objc2_foundation::{MainThreadMarker, NSPoint, NSRect, NSSize, NSString, NSURL, NSURLRequest};
use objc2_web_kit::{
    WKFrameInfo, WKMediaCaptureType, WKPermissionDecision, WKSecurityOrigin, WKUIDelegate,
    WKWebView, WKWebViewConfiguration,
};

/// The window's initial content size in points — a roomy desktop column for the face.
const WINDOW_W: f64 = 1000.0;
const WINDOW_H: f64 = 720.0;

// ---------------------------------------------------------------------------
// Media-permission delegate — auto-grant so WebKit never prompts per-site
// ---------------------------------------------------------------------------

define_class!(
    // A WKUIDelegate that auto-grants camera/microphone for the page. The window is our
    // own trusted local surface, so there's no per-site prompt to show; the macOS system
    // permission (TCC) is separate and still asks once on first real use.
    #[unsafe(super(NSObject))]
    #[thread_kind = MainThreadOnly]
    #[name = "HiAgentWindowMediaGrant"]
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

// ---------------------------------------------------------------------------
// Key-capable borderless window
// ---------------------------------------------------------------------------

define_class!(
    // A borderless NSWindow that can still become key/main. AppKit refuses key status
    // to borderless windows by default, which would starve the web view's text input of
    // keystrokes under the Accessory activation policy; overriding these restores it.
    #[unsafe(super(NSWindow))]
    #[thread_kind = MainThreadOnly]
    #[name = "HiAgentFaceWindow"]
    struct KeyWindow;

    impl KeyWindow {
        #[unsafe(method(canBecomeKeyWindow))]
        fn can_become_key(&self) -> bool {
            true
        }

        #[unsafe(method(canBecomeMainWindow))]
        fn can_become_main(&self) -> bool {
            true
        }
    }
);

/// What the host needs to show/focus the window. Touched only on the main thread.
struct Ivars {
    window: Retained<KeyWindow>,
}

define_class!(
    // Owns the face window and shows/focuses it on the main thread. Reached from other
    // threads only via `performSelectorOnMainThread:`.
    #[unsafe(super(NSObject))]
    #[thread_kind = MainThreadOnly]
    #[name = "HiAgentWindowHost"]
    #[ivars = Ivars]
    struct Host;

    unsafe impl NSObjectProtocol for Host {}

    impl Host {
        /// Show + focus the window (idempotent — a second open just brings it forward).
        #[unsafe(method(open:))]
        fn open(&self, _arg: Option<&AnyObject>) {
            self.present();
        }
    }
);

impl Host {
    /// Bring the app forward (so the face's input can take keys under the Accessory
    /// policy) and show the window key + front.
    fn present(&self) {
        let mtm = MainThreadMarker::new().expect("window host runs on the main thread");
        let iv = self.ivars();
        let app = NSApplication::sharedApplication(mtm);
        // SAFETY: main-thread AppKit calls. `activateIgnoringOtherApps:` is the pre-Sonoma
        // activation call but still works; the window then becomes key so its web view's
        // input can take keystrokes despite the Accessory activation policy.
        unsafe {
            let _: () = msg_send![&*app, activateIgnoringOtherApps: true];
            let window: &KeyWindow = &iv.window;
            let _: () = msg_send![window, center];
            let _: () = msg_send![window, makeKeyAndOrderFront: core::ptr::null_mut::<AnyObject>()];
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

/// Build the borderless window hosting a `WKWebView` on the face. Best-effort and
/// called once from the tray's main-thread setup; the web view loads `url` (the face
/// base, `http://127.0.0.1:{port}/`) and the window stays hidden until [`open`].
pub fn install(mtm: MainThreadMarker, url: &str) {
    // SAFETY: standard AppKit/WebKit setup on the main thread (guaranteed by `mtm`).
    // Every object is kept alive past `install` by leaking it below, so none is used
    // after free; they live for the process, like the tray's items.
    unsafe {
        let config = WKWebViewConfiguration::new(mtm);
        let frame = NSRect::new(NSPoint::new(0.0, 0.0), NSSize::new(WINDOW_W, WINDOW_H));
        let webview: Retained<WKWebView> =
            WKWebView::initWithFrame_configuration(WKWebView::alloc(mtm), frame, &config);

        // Auto-grant the page's mic/camera (no per-site WebKit prompt). The delegate is
        // weakly held by the web view, so leak our strong reference to keep it alive.
        let media: Retained<MediaGrant> = msg_send![MediaGrant::alloc(mtm), init];
        webview.setUIDelegate(Some(ProtocolObject::from_ref(&*media)));
        std::mem::forget(media);

        // Allow Web Inspector on the window's own local page (right-click → Inspect
        // Element) — it's the app's own content, so leaving it on lets the UI be debugged.
        webview.setInspectable(true);

        if let Some(nsurl) = NSURL::URLWithString(&NSString::from_str(url)) {
            let req = NSURLRequest::requestWithURL(&nsurl);
            let _: Option<Retained<objc2_web_kit::WKNavigation>> = webview.loadRequest(&req);
        } else {
            tracing::warn!(url, "window: bad URL; nothing to load");
        }

        // A naked borderless window (no titlebar / traffic lights) that is still
        // resizable at the edges; see the module doc for the native-chrome-hidden
        // alternative if the missing affordances get in the way.
        let style = NSWindowStyleMask::Borderless | NSWindowStyleMask::Resizable;
        let window: Retained<KeyWindow> = msg_send![
            KeyWindow::alloc(mtm),
            initWithContentRect: frame,
            styleMask: style,
            backing: NSBackingStoreType::Buffered,
            defer: false,
        ];
        // Drag the window by its background — there's no titlebar to grab. Don't free
        // the window when it's dismissed; we keep it hidden and reopen the same one.
        let _: () = msg_send![&*window, setMovableByWindowBackground: true];
        let _: () = msg_send![&*window, setReleasedWhenClosed: false];
        let _: () = msg_send![&*window, setContentView: &*webview];

        let host = Host::alloc(mtm).set_ivars(Ivars { window });
        let host: Retained<Host> = msg_send![super(host), init];
        let ptr: *const Host = &*host;
        std::mem::forget(host);
        std::mem::forget(webview);
        std::mem::forget(config);
        let _ = HOST.set(HostPtr(ptr));
    }
    tracing::info!("face window installed");
}

/// Show + focus the face window (idempotent). Safe to call from any thread — a no-op
/// until the window is installed (headless / before the tray loads).
pub fn open() {
    hop(sel!(open:));
}

/// Hop `selector` (`open:`) onto the main run loop where the window lives. A no-op until
/// [`install`] has published the host.
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
