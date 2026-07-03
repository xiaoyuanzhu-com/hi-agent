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
use std::path::PathBuf;
use std::sync::{Arc, Mutex, OnceLock};

use anyhow::anyhow;
use objc2::rc::Retained;
use objc2::runtime::{AnyObject, NSObject, NSObjectProtocol, ProtocolObject};
use objc2::{define_class, msg_send, sel, AnyThread, ClassType, DefinedClass, MainThreadOnly};
use objc2_app_kit::{
    NSApplication, NSApplicationActivationPolicy, NSCellImagePosition, NSControlStateValueOff,
    NSControlStateValueOn,
    NSEventModifierFlags, NSEventType, NSImage, NSMenu, NSMenuDelegate, NSMenuItem, NSStatusBar,
    NSStatusBarButton, NSStatusItem, NSVariableStatusItemLength,
};
use objc2_foundation::{MainThreadMarker, NSData, NSSize, NSString};
use tokio::sync::Notify;

use crate::foundation::credentials::{Credentials, Mode};

/// State the menu actions close over: where the credential store lives (for the
/// Account submenu) and the trigger that asks the server to drain. Held as the target
/// object's instance variables.
struct Ivars {
    data_dir: PathBuf,
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
        /// "Open Hi Agent" → show the dedicated face window (a borderless `WKWebView`
        /// window; see [`super::macos_window`]). Idempotent — reopening just brings the
        /// existing window forward.
        #[unsafe(method(open:))]
        fn open(&self, _sender: Option<&AnyObject>) {
            super::macos_window::open();
        }

        /// "Quit hi-agent" → ask the server to shut down. The server thread runs the
        /// normal graceful drain + ACP reap, then exits the process (which ends this
        /// run loop). We do not stop AppKit here — the process exit does it.
        #[unsafe(method(quit:))]
        fn quit(&self, _sender: Option<&AnyObject>) {
            self.ivars().shutdown.notify_waiters();
        }

        /// "小圆猪 ▸ Use 小圆猪" → select the managed broker account (free tier works
        /// immediately; the broker keeps energy topped up on its own loop). Applies
        /// on the next restart; checkmarks refresh when the submenu next opens.
        #[unsafe(method(selectXiaoyuanzhu:))]
        fn select_xiaoyuanzhu(&self, _sender: Option<&AnyObject>) {
            self.set_mode(Mode::Xiaoyuanzhu);
        }

        /// "Your own keys ▸ Use my own keys" → select BYOK. Only switches the active
        /// source; it does NOT require a key to be configured first (a valid, if
        /// not-yet-working, state). Keys are entered separately per feature.
        #[unsafe(method(useByok:))]
        fn use_byok(&self, _sender: Option<&AnyObject>) {
            self.set_mode(Mode::Byok);
        }

        /// A per-feature "…" row under "Your own keys" → open that feature's key
        /// dialog. The sender's tag identifies the feature ([`feature_from_tag`]);
        /// editing a key does not switch the active mode.
        #[unsafe(method(configureFeature:))]
        fn configure_feature(&self, sender: Option<&AnyObject>) {
            let mtm = MainThreadMarker::new().expect("tray menu action runs on the main thread");
            let tag: isize = sender.map(|s| unsafe { msg_send![s, tag] }).unwrap_or(0);
            let Some(feature) = feature_from_tag(tag) else { return };
            crate::foundation::vendors::macos_account::configure_feature(mtm, &self.ivars().data_dir, feature);
        }

        /// "小圆猪 ▸ Subscribe…" → open the pricing page in the browser, signed in as
        /// this device account. The broker round-trip (mint a one-time ticket) runs
        /// off the main thread; the resulting URL is handed to `open`.
        #[unsafe(method(subscribe:))]
        fn subscribe(&self, _sender: Option<&AnyObject>) {
            let data_dir = self.ivars().data_dir.clone();
            std::thread::spawn(move || {
                let rt = match tokio::runtime::Builder::new_current_thread().enable_all().build() {
                    Ok(rt) => rt,
                    Err(e) => {
                        tracing::error!(error = %e, "tray: subscribe runtime build failed");
                        return;
                    }
                };
                match rt.block_on(crate::foundation::broker::subscribe_url(&data_dir)) {
                    Ok(url) => {
                        if let Err(e) = std::process::Command::new("open").arg(&url).spawn() {
                            tracing::error!(error = %e, "tray: failed to open subscribe url");
                        }
                    }
                    Err(e) => tracing::warn!(error = %e, "tray: could not open subscribe page"),
                }
            });
        }

    }
);

impl TrayTarget {
    fn new(mtm: MainThreadMarker, data_dir: PathBuf, shutdown: Arc<Notify>) -> Retained<Self> {
        let this = Self::alloc(mtm).set_ivars(Ivars { data_dir, shutdown });
        unsafe { msg_send![super(this), init] }
    }

    /// Persist the selected credential mode (best-effort; applies on restart). Used
    /// by both `Use …` rows — selection is independent of configuration.
    fn set_mode(&self, mode: Mode) {
        let data_dir = &self.ivars().data_dir;
        let mut creds = Credentials::load(data_dir);
        creds.mode = mode;
        if let Err(e) = creds.save(data_dir) {
            tracing::error!(error = %e, ?mode, "tray: failed to switch mode");
        }
    }
}

/// The BYOK feature rows under "Your own keys", as `(label, tag)`. The tag routes
/// a click to a [`macos_account::Feature`] via [`feature_from_tag`] and, on open,
/// selects which stored key drives that row's "configured / not set" suffix.
const BYOK_FEATURES: &[(&str, isize)] = &[
    ("LLM", 10),
    ("Speech-to-text", 11),
    ("Text-to-speech", 12),
    ("Vision", 13),
    ("Image", 14),
    ("Video", 15),
];

/// Map a feature row's tag back to the credential feature it configures.
fn feature_from_tag(tag: isize) -> Option<crate::foundation::vendors::macos_account::Feature> {
    use crate::foundation::vendors::macos_account::Feature;
    Some(match tag {
        10 => Feature::Llm,
        11 => Feature::Stt,
        12 => Feature::Tts,
        13 => Feature::Vision,
        14 => Feature::Image,
        15 => Feature::Video,
        _ => return None,
    })
}

/// Whether a feature's BYOK key is set (drives the row's "configured / not set"
/// suffix). Tag order mirrors [`BYOK_FEATURES`].
fn feature_key_set(creds: &Credentials, tag: isize) -> bool {
    let vk = match tag {
        10 => return !creds.llm.api_key.trim().is_empty(),
        11 => &creds.stt,
        12 => &creds.tts,
        13 => &creds.vision,
        14 => &creds.image,
        15 => &creds.video,
        _ => return false,
    };
    !vk.api_key.trim().is_empty()
}

/// Format an energy count compactly: 3_000_000 → "3.0M", 82_000 → "82K".
fn fmt_energy(n: i64) -> String {
    if n >= 1_000_000 {
        format!("{:.1}M", (n as f64) / 1_000_000.0)
    } else if n >= 1_000 {
        format!("{}K", n / 1_000)
    } else {
        n.to_string()
    }
}

/// Set the state (checkmark) of the menu item carrying `tag`, if present.
fn set_item_state(menu: &NSMenu, tag: isize, on: bool) {
    let v = if on { NSControlStateValueOn } else { NSControlStateValueOff };
    for i in 0..menu.numberOfItems() {
        if let Some(item) = menu.itemAtIndex(i) {
            if item.tag() == tag {
                item.setState(v);
            }
        }
    }
}

/// Set the title of the menu item carrying `tag`, if present.
fn set_item_title(menu: &NSMenu, tag: isize, title: &str) {
    let s = NSString::from_str(title);
    for i in 0..menu.numberOfItems() {
        if let Some(item) = menu.itemAtIndex(i) {
            if item.tag() == tag {
                item.setTitle(&s);
            }
        }
    }
}

/// Refresh the top Account submenu: tick which mode is active on the parent rows
/// (小圆猪 = tag 100, Your own keys = tag 101).
fn refresh_account(menu: &NSMenu, mode: Mode) {
    set_item_state(menu, 100, mode == Mode::Xiaoyuanzhu);
    set_item_state(menu, 101, mode == Mode::Byok);
}

/// Refresh the 小圆猪 submenu: the Use checkmark (tag 1) + the plan/energy/resets
/// info rows (tags 20/21/22) from the last energy snapshot.
fn refresh_xiaoyuanzhu(menu: &NSMenu, creds: &Credentials) {
    set_item_state(menu, 1, creds.mode == Mode::Xiaoyuanzhu);
    let (plan, energy, resets) = match &creds.energy {
        Some(e) => (
            format!(
                "Plan: {}",
                match e.tier.as_str() {
                    "pro" => "Pro",
                    "max" => "Max",
                    _ => "Standard",
                }
            ),
            format!("Energy: {} / {}", fmt_energy(e.remaining), fmt_energy(e.total)),
            if e.resets_at.len() >= 10 {
                format!("Resets: {}", &e.resets_at[..10])
            } else {
                "Resets: —".to_string()
            },
        ),
        None => (
            "Plan: Standard".to_string(),
            "Energy: —".to_string(),
            "Resets: —".to_string(),
        ),
    };
    set_item_title(menu, 20, &plan);
    set_item_title(menu, 21, &energy);
    set_item_title(menu, 22, &resets);
}

/// Refresh the "Your own keys" submenu: the Use checkmark (tag 2) + each feature
/// row's "configured / not set" suffix.
fn refresh_byok(menu: &NSMenu, creds: &Credentials) {
    set_item_state(menu, 2, creds.mode == Mode::Byok);
    for &(label, tag) in BYOK_FEATURES {
        let state = if feature_key_set(creds, tag) { "configured" } else { "not set" };
        set_item_title(menu, tag, &format!("{label} — {state}"));
    }
}

/// Which submenu an [`AccountMenu`] delegate refreshes on open.
#[derive(Clone, Copy)]
enum SubmenuKind {
    /// The top Account submenu — checkmarks on the 小圆猪 / BYOK parent rows.
    Account,
    /// The 小圆猪 submenu — the Use checkmark + plan/energy/resets info rows.
    Xiaoyuanzhu,
    /// The "Your own keys" submenu — the Use checkmark + per-feature status rows.
    Byok,
}

/// What an Account-family submenu delegate needs to refresh on open: the store
/// location and which submenu it is.
struct SubmenuIvars {
    data_dir: PathBuf,
    kind: SubmenuKind,
}

define_class!(
    // Delegate shared by the Account submenu and its two child submenus. On open it
    // reloads the store and refreshes that submenu's checkmarks / info rows, so a
    // mode switch or key edit shows without rebuilding the menu.
    #[unsafe(super(NSObject))]
    #[thread_kind = MainThreadOnly]
    #[name = "HiAgentAccountMenu"]
    #[ivars = SubmenuIvars]
    struct AccountMenu;

    unsafe impl NSObjectProtocol for AccountMenu {}

    unsafe impl NSMenuDelegate for AccountMenu {
        #[unsafe(method(menuNeedsUpdate:))]
        fn menu_needs_update(&self, menu: &NSMenu) {
            let creds = Credentials::load(&self.ivars().data_dir);
            match self.ivars().kind {
                SubmenuKind::Account => refresh_account(menu, creds.mode),
                SubmenuKind::Xiaoyuanzhu => refresh_xiaoyuanzhu(menu, &creds),
                SubmenuKind::Byok => refresh_byok(menu, &creds),
            }
        }
    }
);

impl AccountMenu {
    fn new(mtm: MainThreadMarker, data_dir: PathBuf, kind: SubmenuKind) -> Retained<Self> {
        let this = Self::alloc(mtm).set_ivars(SubmenuIvars { data_dir, kind });
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
        /// `sendActionOn:` in [`run`]). A primary click opens the face window; a
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
                crate::foundation::vendors::macos_window::open();
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
pub fn run(url: String, data_dir: PathBuf, shutdown: Arc<Notify>) -> anyhow::Result<()> {
    let mtm = MainThreadMarker::new()
        .ok_or_else(|| anyhow!("the menu bar must be set up on the main thread"))?;

    let app = NSApplication::sharedApplication(mtm);
    // Accessory: live in the menu bar only — no Dock icon, no app menu.
    app.setActivationPolicy(NSApplicationActivationPolicy::Accessory);

    // The face window's web view loads the base URL; moved into `install` below. The
    // menu target no longer needs it (the "Open" action just shows the window).
    let window_url = url;

    let target = TrayTarget::new(mtm, data_dir.clone(), shutdown);

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

        // Account ▸ [ 小圆猪 ▸ … ] [ Your own keys ▸ … ] — pick which source powers
        // the agent, and set it up. Each mode is a submenu carrying its own detail;
        // the top Account submenu just ticks which is active. Delegates refresh the
        // checkmarks / info rows on open, so switches + key edits show live.
        //
        // A small builder to cut the repetition: title + optional action selector +
        // tag. Items with an action are targeted at `target`; action-less rows (info
        // rows, submenu parents) are left untargeted — with the menu's default
        // auto-enable, that greys the info rows exactly as intended.
        let make = |title: &str, action: Option<objc2::runtime::Sel>, tag: isize| -> Retained<NSMenuItem> {
            let item = NSMenuItem::initWithTitle_action_keyEquivalent(
                NSMenuItem::alloc(mtm),
                &NSString::from_str(title),
                action,
                &NSString::from_str(""),
            );
            if action.is_some() {
                item.setTarget(Some(&target));
            }
            item.setTag(tag);
            item
        };

        // 小圆猪 submenu: use it · plan/energy/resets · subscribe.
        let xyz_menu = NSMenu::new(mtm);
        xyz_menu.addItem(&make("Use 小圆猪", Some(sel!(selectXiaoyuanzhu:)), 1));
        xyz_menu.addItem(&NSMenuItem::separatorItem(mtm));
        xyz_menu.addItem(&make("Plan: Free", None, 20));
        xyz_menu.addItem(&make("Energy: —", None, 21));
        xyz_menu.addItem(&make("Resets: —", None, 22));
        xyz_menu.addItem(&NSMenuItem::separatorItem(mtm));
        xyz_menu.addItem(&make("Subscribe…", Some(sel!(subscribe:)), 30));
        let xyz_delegate = AccountMenu::new(mtm, data_dir.clone(), SubmenuKind::Xiaoyuanzhu);
        xyz_menu.setDelegate(Some(ProtocolObject::from_ref(&*xyz_delegate)));
        std::mem::forget(xyz_delegate);

        // Your own keys submenu: use it · a status/config row per feature.
        let byok_menu = NSMenu::new(mtm);
        byok_menu.addItem(&make("Use my own keys", Some(sel!(useByok:)), 2));
        byok_menu.addItem(&NSMenuItem::separatorItem(mtm));
        for &(label, tag) in BYOK_FEATURES {
            byok_menu.addItem(&make(&format!("{label} — not set"), Some(sel!(configureFeature:)), tag));
        }
        let byok_delegate = AccountMenu::new(mtm, data_dir.clone(), SubmenuKind::Byok);
        byok_menu.setDelegate(Some(ProtocolObject::from_ref(&*byok_delegate)));
        std::mem::forget(byok_delegate);

        // Top Account submenu: the two mode parents (checkmark = active source).
        let account_menu = NSMenu::new(mtm);
        let xyz_parent = make("小圆猪", None, 100);
        xyz_parent.setSubmenu(Some(&*xyz_menu));
        account_menu.addItem(&xyz_parent);
        let byok_parent = make("Your own keys", None, 101);
        byok_parent.setSubmenu(Some(&*byok_menu));
        account_menu.addItem(&byok_parent);
        let acct_delegate = AccountMenu::new(mtm, data_dir.clone(), SubmenuKind::Account);
        account_menu.setDelegate(Some(ProtocolObject::from_ref(&*acct_delegate)));
        std::mem::forget(acct_delegate);

        let account_parent = make("Account", None, 0);
        account_parent.setSubmenu(Some(&*account_menu));
        menu.addItem(&account_parent);

        menu.addItem(&NSMenuItem::separatorItem(mtm));

        let quit_item = NSMenuItem::initWithTitle_action_keyEquivalent(
            NSMenuItem::alloc(mtm),
            &NSString::from_str("Quit Hi Agent"),
            Some(sel!(quit:)),
            &NSString::from_str("q"),
        );
        quit_item.setTarget(Some(&target));
        menu.addItem(&quit_item);

        // A left-click on the icon opens the face window; a right-/control-click shows
        // the menu above. So the button drives a `TrayClick` action rather than owning
        // the menu permanently (a permanent menu would route *every* click to the menu
        // and the button action would never fire).
        if let Some(button) = &button {
            // The face window is button-independent (it's not anchored to the tray), so
            // it's installed unconditionally; the single right-⌘ tap still opens the
            // menu-bar popover, so that's installed too (anchored to the button).
            crate::foundation::vendors::macos_window::install(mtm, &window_url);
            crate::foundation::vendors::macos_popover::install(mtm, button.clone(), &window_url);

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
