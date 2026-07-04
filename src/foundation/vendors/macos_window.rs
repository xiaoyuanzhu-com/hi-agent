//! macOS window vendor — the dedicated desktop window that hosts the agent **face**.
//!
//! A standard titled `NSWindow` hosting a `WKWebView` that loads the face (`/`) — the
//! same appearance UI the tray popover used to show, but as a free-standing window with
//! native macOS chrome (titlebar, traffic lights, drag, resize) instead of a menu-bar
//! popup. Opened by a left-click on the tray icon, the "Open Hi Agent" menu item, or a
//! single right-⌘ tap ([`crate::body::gesture`]).
//!
//! **A real interactive window under the Accessory policy.** A titled window becomes key
//! on its own, but [`KeyWindow`] still overrides `canBecomeKeyWindow` /
//! `canBecomeMainWindow` as a belt-and-braces guarantee that the web view's text input
//! takes keystrokes even though the app runs Accessory (no Dock icon). The mask carries
//! `Titled | Closable | Miniaturizable | Resizable`, so the standard titlebar handles
//! dragging and the traffic lights close/minimize the window. The window opts into
//! `FullScreenPrimary` collection behavior so the green button enters native full-screen
//! (⌥-click still zooms). Double-clicking the title-bar strip zooms (maximize / restore):
//! AppKit's own handler for that never fires because `FullSizeContentView` puts our
//! content under the transparent titlebar, so [`KeyWindow`] catches the double-click in
//! `sendEvent:` and drives `zoom:` itself.
//!
//! **Themed, centered title bar.** The native titlebar is made transparent and its text
//! hidden; with `FullSizeContentView` the content view spans under it, so the window's
//! background color paints a flat bar. That bar and the centered `NSTextField` title are
//! tinted to the face's "Paper & Ink" theme — white paper / ink in light, espresso / ivory in
//! dark, following the OS appearance so the native chrome matches the web content below
//! (see [`apply_face_theme`]). The label clears the traffic lights on the left, and the
//! web view is inset below the bar strip.
//!
//! Media permission mirrors the popover: a [`MediaGrant`] `WKUIDelegate` auto-grants
//! the page's mic/camera so WebKit never shows its per-site *page-level* prompt. That is
//! only the first of two gates — the macOS *system* prompt (TCC) is separate, and it only
//! works when the host process is a bundled `.app` (Info.plist usage strings + a code
//! signature) that is its own "responsible process". A packaged build gets both for free;
//! under `make dev` they're arranged by scripts/dev.sh + `reexec_disclaiming_responsibility`
//! in src/main.rs — see dev.sh for the full explanation. A bare binary satisfies neither,
//! so `getUserMedia` there hangs with no prompt.
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
use objc2_app_kit::{
    NSApplication, NSAutoresizingMaskOptions, NSBackingStoreType, NSColor, NSEvent, NSEventType,
    NSWindowCollectionBehavior, NSTextAlignment, NSTextField, NSView, NSWindow, NSWindowStyleMask,
    NSWindowTitleVisibility,
};
use objc2_foundation::{
    MainThreadMarker, NSPoint, NSRect, NSSize, NSString, NSURL, NSURLRequest, NSUserDefaults,
};
use objc2_web_kit::{
    WKFrameInfo, WKMediaCaptureType, WKPermissionDecision, WKSecurityOrigin, WKUIDelegate,
    WKWebView, WKWebViewConfiguration,
};

/// The window's initial content size in points — a roomy desktop column for the face.
const WINDOW_W: f64 = 1000.0;
const WINDOW_H: f64 = 720.0;

/// Height of the title-bar strip, matching the standard macOS titlebar so the traffic
/// lights sit centered in it.
const BAR_H: f64 = 28.0;
/// Height of the centered title label; kept near the font's line height and centered
/// vertically within [`BAR_H`].
const LABEL_H: f64 = 18.0;
/// Left-edge inset that the three traffic-light buttons occupy. Double-clicks landing
/// here are left to the buttons, not swallowed for a zoom (see [`KeyWindow::send_event`]).
const TRAFFIC_LIGHT_W: f64 = 78.0;

// ---------------------------------------------------------------------------
// Title-bar theme — match the face's "Paper & Ink" skin so the native bar reads
// as one surface with the web content below it.
// ---------------------------------------------------------------------------

/// Whether the OS is in dark mode. The face themes via `prefers-color-scheme`
/// (i.e. the OS appearance), so reading the same signal here keeps the native
/// bar in step with the web content. Reads the global `AppleInterfaceStyle`
/// default — `"Dark"` in dark mode, absent in light; we set no per-window
/// appearance, so this is the effective appearance for our window too.
fn os_is_dark() -> bool {
    let defaults = NSUserDefaults::standardUserDefaults();
    defaults
        .stringForKey(&NSString::from_str("AppleInterfaceStyle"))
        .is_some_and(|s| s.to_string().eq_ignore_ascii_case("dark"))
}

/// An sRGB colour (opaque) — matches how the web tokens are authored (hex/sRGB).
fn srgb(r: f64, g: f64, b: f64) -> Retained<NSColor> {
    NSColor::colorWithSRGBRed_green_blue_alpha(r, g, b, 1.0)
}

/// Paint the bar background + centered title to the current theme's paper/ink.
/// The bar is matched to `--bg-1`, not `--bg-0`: the face's visible background
/// is the `.hi-presence` radial gradient centered at the top edge — `--bg-1` at
/// `50% 0%` fading to `--bg-0` at 62% — so the strip of web content directly
/// under the title bar is `--bg-1`. Painting the bar `--bg-1` (light white
/// `#ffffff`, dark espresso `#2b2720`) makes the native chrome read as one
/// surface with the content where they actually meet. The title uses `--fg`
/// (light ink `#3a352c`, dark ivory `#e8dfce`). Called at install and again on
/// each open so a light/dark switch since the last open is picked up.
fn apply_face_theme(window: &NSWindow, label: &NSTextField) {
    let (bg, fg) = if os_is_dark() {
        (srgb(0.169, 0.153, 0.125), srgb(0.910, 0.875, 0.808))
    } else {
        (srgb(1.0, 1.0, 1.0), srgb(0.227, 0.208, 0.173))
    };
    window.setBackgroundColor(Some(&bg));
    label.setTextColor(Some(&fg));
}

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
    // A titled NSWindow that can still become key/main. Titled windows do so by default;
    // these overrides are a belt-and-braces guarantee that the web view's text input
    // keeps taking keystrokes under the Accessory activation policy.
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

        /// Double-click the title-bar strip to zoom (maximize) and back — the standard
        /// macOS title-bar gesture. AppKit's own handler for it never fires here: with
        /// `FullSizeContentView` the content view (our web view + centered label) spans
        /// under the transparent titlebar, so the double-click lands on our content, not
        /// the native title-bar view. We catch it in the window's event funnel instead —
        /// `sendEvent:` sees every event dispatched to the window regardless of which
        /// subview would handle it — and drive `zoom:` ourselves.
        #[unsafe(method(sendEvent:))]
        fn send_event(&self, event: &NSEvent) {
            // A left double-click landing in the title-bar strip (top `BAR_H` points),
            // but clear of the traffic lights on the left. `locationInWindow` is in base
            // coordinates (origin bottom-left); with `FullSizeContentView` the content
            // view spans the full height, so the strip is the top `BAR_H` of it.
            let in_titlebar_zoom_zone = event.r#type() == NSEventType::LeftMouseDown
                && event.clickCount() == 2
                && {
                    let loc = event.locationInWindow();
                    let height = self.contentView().map_or(0.0, |v| v.bounds().size.height);
                    loc.y >= height - BAR_H && loc.x >= TRAFFIC_LIGHT_W
                };
            if in_titlebar_zoom_zone {
                // SAFETY: main-thread AppKit call; `zoom:` toggles between the user frame
                // and the zoomed (standard) frame, i.e. maximize / restore.
                unsafe {
                    let _: () = msg_send![self, zoom: core::ptr::null_mut::<AnyObject>()];
                }
                return;
            }
            // Not our gesture — hand the event back to AppKit untouched.
            // SAFETY: forwarding to the superclass implementation on the main thread.
            unsafe {
                let _: () = msg_send![super(self), sendEvent: event];
            }
        }
    }
);

/// What the host needs to show/focus the window. Touched only on the main thread.
struct Ivars {
    window: Retained<KeyWindow>,
    /// Kept so [`Host::present`] can re-issue the load if the initial attempt (fired at
    /// install, possibly before the server bound its listener) never committed.
    webview: Retained<WKWebView>,
    request: Retained<NSURLRequest>,
    /// The centered title label — kept so the bar theme can be re-applied on reopen.
    label: Retained<NSTextField>,
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
        // Re-match the bar to the OS appearance in case dark/light toggled since the
        // last open (the window is reused, not rebuilt).
        apply_face_theme(&iv.window, &iv.label);
        // SAFETY: main-thread AppKit calls. `activateIgnoringOtherApps:` is the pre-Sonoma
        // activation call but still works; the window then becomes key so its web view's
        // input can take keystrokes despite the Accessory activation policy.
        unsafe {
            // If the load fired at install never committed — the server thread hadn't
            // bound its listener yet, so the page is still about:blank — re-issue it now.
            // By the time the user opens the window the server is up, so this succeeds.
            // A committed page reports its URL; a failed provisional load leaves it nil.
            if iv.webview.URL().is_none() {
                let _: Option<Retained<objc2_web_kit::WKNavigation>> =
                    iv.webview.loadRequest(&iv.request);
            }
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
        let full = NSRect::new(NSPoint::new(0.0, 0.0), NSSize::new(WINDOW_W, WINDOW_H));

        // The web view fills the window below the title-bar strip and grows with it.
        let web_frame = NSRect::new(
            NSPoint::new(0.0, 0.0),
            NSSize::new(WINDOW_W, WINDOW_H - BAR_H),
        );
        let webview: Retained<WKWebView> =
            WKWebView::initWithFrame_configuration(WKWebView::alloc(mtm), web_frame, &config);
        webview.setAutoresizingMask(
            NSAutoresizingMaskOptions::ViewWidthSizable | NSAutoresizingMaskOptions::ViewHeightSizable,
        );

        // Auto-grant the page's mic/camera (no per-site WebKit prompt). The delegate is
        // weakly held by the web view, so leak our strong reference to keep it alive.
        let media: Retained<MediaGrant> = msg_send![MediaGrant::alloc(mtm), init];
        webview.setUIDelegate(Some(ProtocolObject::from_ref(&*media)));
        std::mem::forget(media);

        // Allow Web Inspector on the window's own local page (right-click → Inspect
        // Element) — it's the app's own content, so leaving it on lets the UI be debugged.
        webview.setInspectable(true);

        let Some(nsurl) = NSURL::URLWithString(&NSString::from_str(url)) else {
            tracing::warn!(url, "window: bad URL; face window not installed");
            return;
        };
        let request = NSURLRequest::requestWithURL(&nsurl);
        // Kick off the first load. It may fail silently if the server thread hasn't bound
        // its listener yet (config bootstrap can lag startup); `present` re-issues it when
        // the user first opens the window, by which point the server is listening.
        let _: Option<Retained<objc2_web_kit::WKNavigation>> = webview.loadRequest(&request);

        // Native window chrome (traffic lights, drag, resize), but with the titlebar made
        // transparent and its text hidden so the window background paints a flat bar we own.
        // `FullSizeContentView` lets the content view span under the titlebar so our label
        // can sit in that strip.
        let style = NSWindowStyleMask::Titled
            | NSWindowStyleMask::Closable
            | NSWindowStyleMask::Miniaturizable
            | NSWindowStyleMask::Resizable
            | NSWindowStyleMask::FullSizeContentView;
        let window: Retained<KeyWindow> = msg_send![
            KeyWindow::alloc(mtm),
            initWithContentRect: full,
            styleMask: style,
            backing: NSBackingStoreType::Buffered,
            defer: false,
        ];
        window.setTitlebarAppearsTransparent(true);
        window.setTitleVisibility(NSWindowTitleVisibility::Hidden);
        // Make the green traffic light enter native full-screen (not zoom/maximize):
        // without `FullScreenPrimary` in the collection behavior the button just zooms.
        window.setCollectionBehavior(NSWindowCollectionBehavior::FullScreenPrimary);
        // Don't free the window when its close button dismisses it; we keep it around
        // and reopen the same one on the next tray click.
        let _: () = msg_send![&*window, setReleasedWhenClosed: false];

        // Our own centered title, standing in for the hidden native one — vertically
        // centered in the title-bar strip, horizontally centered across the full width so
        // it clears the traffic lights on the left.
        let label = NSTextField::labelWithString(&NSString::from_str("Hi Agent"), mtm);
        // NSTextAlignmentCenter (1) — omitted from the generated bindings, so spelled out.
        label.setAlignment(NSTextAlignment(1));
        label.setFrame(NSRect::new(
            NSPoint::new(0.0, WINDOW_H - BAR_H + (BAR_H - LABEL_H) / 2.0),
            NSSize::new(WINDOW_W, LABEL_H),
        ));
        // Stay pinned to the top edge and stretch horizontally as the window resizes.
        label.setAutoresizingMask(
            NSAutoresizingMaskOptions::ViewWidthSizable | NSAutoresizingMaskOptions::ViewMinYMargin,
        );

        // Paint the bar + title to the face's theme (paper/ink, following the OS
        // light/dark appearance) so the native chrome matches the web content.
        apply_face_theme(&window, &label);

        // Content view: the web view below, the title label floating in the bar strip.
        let container = NSView::initWithFrame(NSView::alloc(mtm), full);
        container.addSubview(&webview);
        container.addSubview(&label);
        let _: () = msg_send![&*window, setContentView: &*container];
        std::mem::forget(container);

        // The host owns the window, web view, request, and title label for the process
        // lifetime (the ivars keep them alive; the host itself is leaked below).
        let host = Host::alloc(mtm).set_ivars(Ivars { window, webview, request, label });
        let host: Retained<Host> = msg_send![super(host), init];
        let ptr: *const Host = &*host;
        std::mem::forget(host);
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
