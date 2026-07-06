//! macOS settings vendor — the **native preferences window** (Open ▸ Settings…).
//!
//! A classic left-list / right-detail `NSWindow`: a source-list `NSTableView` in the
//! sidebar (General · Account · About) swaps a detail `NSView` on the right. All pure
//! AppKit (`objc2`), matching the sibling vendors ([`super::macos_window`],
//! [`super::macos_tray`], [`super::macos_account`]) — no web view, no extra framework.
//!
//! **General** — the appearance + language preferences and the attention-gesture switch:
//! - Theme (`NSPopUpButton`, System/Light/Dark) writes [`KEY_THEME`] and immediately
//!   forces the app-wide `NSAppearance` ([`apply_app_theme`]) — one lever that drives both
//!   the native chrome and the face web view's `prefers-color-scheme`, so everything
//!   re-themes together, live.
//! - Language (`NSPopUpButton`) writes [`KEY_LANGUAGE`]; surfaced to the mind as one seed
//!   line (`crate::identity::load_soul`), so it applies on the next restart.
//! - Attention gestures (checkbox) writes [`KEY_GESTURES`]; arming the global key tap is a
//!   boot-time decision, so it too applies on restart.
//!
//! **Account** — an `NSTabView` with two tabs mirroring the two credential modes:
//! - 小圆猪: the managed account, **surface only** — plan / energy / reset read from the
//!   cached [`Energy`] snapshot (refreshed by a background `poll_energy_now` when the tab
//!   shows). "Use 小圆猪" selects the mode; "Manage on the web…" opens the signed-in account
//!   page in the browser (there is no in-app editing — the website owns it).
//! - Your own keys (BYOK): a row per feature with a "configured / not set" status and an
//!   Edit… button that opens the existing per-feature dialog
//!   ([`super::macos_account::configure_feature`]); "Use my own keys" selects the mode.
//!
//! **About** — name, version, a one-liner, and a link to the website.
//!
//! Like the sibling vendors the AppKit objects live on the process main thread and are
//! leaked for the process lifetime; the cross-thread [`open`] hops onto the main run loop
//! via `performSelectorOnMainThread:`. The window is reused (not released on close), so a
//! reopen just re-syncs the controls from the store and brings it forward.

use std::cell::Cell;
use std::path::{Path, PathBuf};
use std::sync::OnceLock;

use objc2::rc::Retained;
use objc2::runtime::{AnyObject, NSObject, NSObjectProtocol, ProtocolObject, Sel};
use objc2::{define_class, msg_send, sel, AnyThread, ClassType, DefinedClass, MainThreadOnly};
use objc2_app_kit::{
    NSApplication, NSAppearance, NSAppearanceNameAqua, NSAppearanceNameDarkAqua, NSBackingStoreType,
    NSButton, NSControlStateValueOff, NSControlStateValueOn, NSControlTextEditingDelegate,
    NSPopUpButton, NSScrollView, NSTableColumn, NSTableView, NSTableViewDataSource,
    NSTableViewDelegate, NSTableViewStyle, NSTabView, NSTabViewItem, NSTextField, NSView, NSWindow,
    NSWindowStyleMask,
};
use objc2_foundation::{
    MainThreadMarker, NSIndexSet, NSNotification, NSPoint, NSRect, NSSize, NSString,
};

use crate::foundation::config::{self, KEY_GESTURES, KEY_LANGUAGE, KEY_THEME, LANGUAGES, THEMES};
use crate::foundation::credentials::{get_setting, set_setting, Credentials, Mode};
use super::macos_account::{configure_feature, Feature};

// ---------------------------------------------------------------------------
// Layout (points) — a fixed-size, non-resizable preferences window.
// ---------------------------------------------------------------------------

const WIN_W: f64 = 660.0;
const WIN_H: f64 = 460.0;
const SIDEBAR_W: f64 = 184.0;
const DETAIL_W: f64 = WIN_W - SIDEBAR_W;
const MARGIN: f64 = 24.0;
const ROW_H: f64 = 24.0;
const LABEL_W: f64 = 110.0;

/// The three sidebar sections, in order. The row index is the selector.
const SECTIONS: &[&str] = &["General", "Account", "About"];

/// BYOK feature rows, as `(label, tag, feature)`. The tag routes an Edit… click back to
/// its [`Feature`]; the order is the display order in the BYOK tab.
const FEATURES: &[(&str, isize, Feature)] = &[
    ("LLM", 10, Feature::Llm),
    ("Speech-to-text", 11, Feature::Stt),
    ("Text-to-speech", 12, Feature::Tts),
    ("Vision", 13, Feature::Vision),
    ("Image", 14, Feature::Image),
    ("Video", 15, Feature::Video),
];

// ---------------------------------------------------------------------------
// Theme — force the app-wide appearance from the stored setting
// ---------------------------------------------------------------------------

/// Force the whole app's `NSAppearance` from [`KEY_THEME`]: `light` → Aqua, `dark` →
/// DarkAqua, anything else (incl. `system` / unset) → `None`, which lets every window
/// follow the OS. Setting it on `NSApplication` cascades to all our windows *and* makes
/// each `WKWebView`'s `prefers-color-scheme` track the choice, so the face + its native
/// bar re-theme together. Called once at boot (before the face window installs, so its
/// pre-paint bar color reads the right appearance) and again whenever the picker changes.
pub fn apply_app_theme(mtm: MainThreadMarker, data_dir: &Path) {
    let app = NSApplication::sharedApplication(mtm);
    // SAFETY: main-thread AppKit calls; the named appearances are process-lived system
    // singletons and `setAppearance:` accepts nil (= follow the OS).
    unsafe {
        let appearance: Option<Retained<NSAppearance>> =
            match get_setting(data_dir, KEY_THEME).as_deref().map(str::trim) {
                Some("light") => NSAppearance::appearanceNamed(NSAppearanceNameAqua),
                Some("dark") => NSAppearance::appearanceNamed(NSAppearanceNameDarkAqua),
                _ => None,
            };
        app.setAppearance(appearance.as_deref());
    }
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

/// The index of `value` in an option list (`(stored, label)` pairs), or 0 (the default,
/// always `system`) when unset / unknown. Case-insensitive on the stored value.
fn option_index(list: &[(&str, &str)], value: Option<&str>) -> usize {
    let v = value.map(str::trim).filter(|s| !s.is_empty()).unwrap_or("system");
    list.iter().position(|(code, _)| code.eq_ignore_ascii_case(v)).unwrap_or(0)
}

// ---------------------------------------------------------------------------
// Host — owns the window, is the table's data source/delegate, and the action target
// ---------------------------------------------------------------------------

/// Everything the settings window holds, all on the main thread. One object doubles as
/// the sidebar table's data source + delegate and every control's action target, so all
/// the control references live in one place.
struct Ivars {
    data_dir: PathBuf,
    window: Retained<NSWindow>,
    /// The right-hand container the detail panes swap into.
    detail: Retained<NSView>,
    /// The three detail panes, indexed by [`SECTIONS`]; only one is a subview of `detail`
    /// at a time.
    panes: [Retained<NSView>; 3],
    /// Which pane is currently shown (so the swap removes the right one).
    current: Cell<usize>,
    // General controls.
    theme_popup: Retained<NSPopUpButton>,
    lang_popup: Retained<NSPopUpButton>,
    gestures_check: Retained<NSButton>,
    // Account · 小圆猪.
    plan_label: Retained<NSTextField>,
    energy_label: Retained<NSTextField>,
    resets_label: Retained<NSTextField>,
    use_xyz: Retained<NSButton>,
    // Account · BYOK.
    use_byok: Retained<NSButton>,
    byok_status: [Retained<NSTextField>; 6],
}

define_class!(
    #[unsafe(super(NSObject))]
    #[thread_kind = MainThreadOnly]
    #[name = "HiAgentSettingsHost"]
    #[ivars = Ivars]
    struct Host;

    unsafe impl NSObjectProtocol for Host {}

    // `NSTableViewDelegate` inherits `NSControlTextEditingDelegate` (all optional
    // methods); declare the empty conformance so the delegate impl below satisfies it.
    unsafe impl NSControlTextEditingDelegate for Host {}

    // --- sidebar table: 3 static rows of section names (cell-based) ---
    unsafe impl NSTableViewDataSource for Host {
        #[unsafe(method(numberOfRowsInTableView:))]
        fn number_of_rows(&self, _table: &NSTableView) -> isize {
            SECTIONS.len() as isize
        }

        // `method_id` (not `method`): the return is an object, so objc2 applies the
        // selector's retain semantics (`objectValue…` = no family = +0 autoreleased) and
        // lets the impl hand back an owned `Retained` — a plain `method` only permits
        // scalar (`Encode`) returns.
        #[unsafe(method_id(tableView:objectValueForTableColumn:row:))]
        fn object_value(
            &self,
            _table: &NSTableView,
            _column: Option<&NSTableColumn>,
            row: isize,
        ) -> Retained<NSString> {
            // objc2 applies the +0 (autoreleased) return convention for this non-family
            // selector, so returning the owned string here is correct — the table copies
            // the value it needs.
            let name = SECTIONS.get(row as usize).copied().unwrap_or("");
            NSString::from_str(name)
        }
    }

    unsafe impl NSTableViewDelegate for Host {
        #[unsafe(method(tableViewSelectionDidChange:))]
        fn selection_did_change(&self, notification: &NSNotification) {
            // The notification object is the table; read its selected row and swap.
            let obj = notification.object();
            let row = obj
                .and_then(|o| o.downcast::<NSTableView>().ok())
                .map(|t| t.selectedRow())
                .unwrap_or(-1);
            if row >= 0 {
                self.show_pane(row as usize);
            }
        }
    }

    impl Host {
        /// Show + focus the window (idempotent). Re-syncs controls from the store first.
        #[unsafe(method(open:))]
        fn open(&self, _arg: Option<&AnyObject>) {
            self.present();
        }

        /// Theme picker changed → persist + apply the app appearance live.
        #[unsafe(method(themeChanged:))]
        fn theme_changed(&self, _sender: Option<&AnyObject>) {
            let idx = self.ivars().theme_popup.indexOfSelectedItem().max(0) as usize;
            let value = THEMES.get(idx).map(|(v, _)| *v).unwrap_or("system");
            self.store(KEY_THEME, value);
            let mtm = MainThreadMarker::new().expect("settings action runs on the main thread");
            apply_app_theme(mtm, &self.ivars().data_dir);
        }

        /// Language picker changed → persist (applies on restart, like other tunables).
        #[unsafe(method(languageChanged:))]
        fn language_changed(&self, _sender: Option<&AnyObject>) {
            let idx = self.ivars().lang_popup.indexOfSelectedItem().max(0) as usize;
            let value = LANGUAGES.get(idx).map(|(v, _)| *v).unwrap_or("system");
            self.store(KEY_LANGUAGE, value);
        }

        /// Attention-gestures checkbox toggled → persist on/off (applies on restart).
        #[unsafe(method(gesturesToggled:))]
        fn gestures_toggled(&self, _sender: Option<&AnyObject>) {
            let on = self.ivars().gestures_check.state() == NSControlStateValueOn;
            self.store(KEY_GESTURES, if on { "on" } else { "off" });
        }

        /// "Use 小圆猪" → make the managed account the active source (applies on restart).
        #[unsafe(method(useXiaoyuanzhu:))]
        fn use_xiaoyuanzhu(&self, _sender: Option<&AnyObject>) {
            self.set_mode(Mode::Xiaoyuanzhu);
        }

        /// "Use my own keys" → make BYOK the active source (applies on restart).
        #[unsafe(method(useByok:))]
        fn use_byok(&self, _sender: Option<&AnyObject>) {
            self.set_mode(Mode::Byok);
        }

        /// "Manage on the web…" → open the signed-in account page in the browser. The
        /// broker round-trip (mint a one-time ticket) runs off the main thread; the URL
        /// is handed to `open` (mirrors [`super::macos_tray`]'s Subscribe).
        #[unsafe(method(manageAccount:))]
        fn manage_account(&self, _sender: Option<&AnyObject>) {
            let data_dir = self.ivars().data_dir.clone();
            std::thread::spawn(move || {
                let rt = match tokio::runtime::Builder::new_current_thread().enable_all().build() {
                    Ok(rt) => rt,
                    Err(e) => {
                        tracing::error!(error = %e, "settings: manage runtime build failed");
                        return;
                    }
                };
                match rt.block_on(crate::foundation::broker::subscribe_url(&data_dir, Some("/account"))) {
                    Ok(url) => open_url(&url),
                    Err(e) => tracing::warn!(error = %e, "settings: could not open account page"),
                }
            });
        }

        /// A BYOK feature's Edit… button → open its key dialog (the sender's tag names the
        /// feature), then refresh the account tab's status rows.
        #[unsafe(method(editFeature:))]
        fn edit_feature(&self, sender: Option<&AnyObject>) {
            let mtm = MainThreadMarker::new().expect("settings action runs on the main thread");
            let tag: isize = sender.map(|s| unsafe { msg_send![s, tag] }).unwrap_or(0);
            let Some(feature) = FEATURES.iter().find(|(_, t, _)| *t == tag).map(|(_, _, f)| *f) else {
                return;
            };
            configure_feature(mtm, &self.ivars().data_dir, feature);
            self.sync_account();
        }

        /// "Visit hi.xiaoyuanzhu.com" (About) → open the site in the browser.
        #[unsafe(method(visitWebsite:))]
        fn visit_website(&self, _sender: Option<&AnyObject>) {
            open_url("https://hi.xiaoyuanzhu.com");
        }

        /// Reload the 小圆猪 energy labels from the freshly polled snapshot. Hopped onto the
        /// main thread by the background poll (see [`Host::show_pane`]).
        #[unsafe(method(refreshEnergy:))]
        fn refresh_energy(&self, _arg: Option<&AnyObject>) {
            self.sync_account();
        }
    }
);

impl Host {
    /// Bring the app forward, re-sync the controls from the store, and show the window.
    fn present(&self) {
        let mtm = MainThreadMarker::new().expect("settings host runs on the main thread");
        self.sync_general();
        self.sync_account();
        let iv = self.ivars();
        let app = NSApplication::sharedApplication(mtm);
        // SAFETY: main-thread AppKit calls; the window is kept alive by the ivars.
        unsafe {
            let _: () = msg_send![&*app, activateIgnoringOtherApps: true];
            let window: &NSWindow = &iv.window;
            let _: () = msg_send![window, center];
            let _: () = msg_send![window, makeKeyAndOrderFront: core::ptr::null_mut::<AnyObject>()];
        }
    }

    /// Swap the detail pane to `idx` (removing the current one). When the Account pane
    /// shows, kick a background energy poll and refresh its labels when it lands.
    fn show_pane(&self, idx: usize) {
        let iv = self.ivars();
        if idx >= iv.panes.len() {
            return;
        }
        let cur = iv.current.get();
        if cur != idx {
            iv.panes[cur].removeFromSuperview();
            iv.detail.addSubview(&iv.panes[idx]);
            iv.current.set(idx);
        }
        if idx == 1 {
            self.poll_energy_async();
        }
    }

    /// Persist a setting best-effort (a write error is logged, not surfaced).
    fn store(&self, key: &str, value: &str) {
        if let Err(e) = set_setting(&self.ivars().data_dir, key, value) {
            tracing::error!(error = %e, key, "settings: failed to persist");
        }
    }

    /// Persist the active credential mode (applies on restart) and re-sync the tab.
    fn set_mode(&self, mode: Mode) {
        let data_dir = &self.ivars().data_dir;
        let mut creds = Credentials::load(data_dir);
        creds.mode = mode;
        if let Err(e) = creds.save(data_dir) {
            tracing::error!(error = %e, ?mode, "settings: failed to switch mode");
        }
        self.sync_account();
    }

    /// Set the General controls from the store: theme + language popups, gesture checkbox.
    fn sync_general(&self) {
        let iv = self.ivars();
        let dd = &iv.data_dir;
        iv.theme_popup
            .selectItemAtIndex(option_index(THEMES, get_setting(dd, KEY_THEME).as_deref()) as isize);
        iv.lang_popup.selectItemAtIndex(
            option_index(LANGUAGES, get_setting(dd, KEY_LANGUAGE).as_deref()) as isize,
        );
        let on = config::flag_on(get_setting(dd, KEY_GESTURES));
        iv.gestures_check
            .setState(if on { NSControlStateValueOn } else { NSControlStateValueOff });
    }

    /// Set the Account tab from the store: energy labels, the two "Use" buttons (the
    /// active mode's reads "✓ Active" and is disabled), and each BYOK status row.
    fn sync_account(&self) {
        let iv = self.ivars();
        let creds = Credentials::load(&iv.data_dir);
        let (plan, energy, resets) = match &creds.energy {
            Some(e) => (
                format!("Plan: {}", if e.tier.eq_ignore_ascii_case("sub") { "Subscribed" } else { "Free" }),
                format!("Energy: {} / {}", fmt_energy(e.remaining), fmt_energy(e.total)),
                if e.resets_at.len() >= 10 { format!("Resets: {}", &e.resets_at[..10]) } else { "Resets: —".to_string() },
            ),
            None => ("Plan: Free".to_string(), "Energy: —".to_string(), "Resets: —".to_string()),
        };
        set_label(&iv.plan_label, &plan);
        set_label(&iv.energy_label, &energy);
        set_label(&iv.resets_label, &resets);

        let xyz_active = creds.mode == Mode::Xiaoyuanzhu;
        set_use_button(&iv.use_xyz, "Use 小圆猪", xyz_active);
        set_use_button(&iv.use_byok, "Use my own keys", !xyz_active);

        for (i, (label, _, feature)) in FEATURES.iter().enumerate() {
            let set = feature_key_set(&creds, *feature);
            let status = if set { "configured" } else { "not set" };
            if let Some(field) = iv.byok_status.get(i) {
                set_label(field, &format!("{label} — {status}"));
            }
        }
    }

    /// Background energy poll → refresh the 小圆猪 labels on the main thread when it lands.
    /// Best-effort: a failed poll just leaves the cached values in place.
    fn poll_energy_async(&self) {
        let data_dir = self.ivars().data_dir.clone();
        std::thread::spawn(move || {
            let rt = match tokio::runtime::Builder::new_current_thread().enable_all().build() {
                Ok(rt) => rt,
                Err(_) => return,
            };
            if rt.block_on(crate::foundation::broker::poll_energy_now(&data_dir)).is_some() {
                // The store now holds the fresh snapshot; hop to the main thread to redraw.
                if let Some(host) = HOST.get() {
                    // SAFETY: `host.0` is the leaked, process-lived Host, messaged only via
                    // `performSelectorOnMainThread:` (callable from any thread).
                    unsafe {
                        let obj: &Host = &*host.0;
                        let _: () = msg_send![
                            obj,
                            performSelectorOnMainThread: sel!(refreshEnergy:),
                            withObject: core::ptr::null_mut::<AnyObject>(),
                            waitUntilDone: false
                        ];
                    }
                }
            }
        });
    }
}

/// Whether a feature's BYOK key is set (drives the row's "configured / not set" suffix).
fn feature_key_set(creds: &Credentials, feature: Feature) -> bool {
    match feature {
        Feature::Llm => !creds.llm.api_key.trim().is_empty(),
        Feature::Stt => !creds.stt.api_key.trim().is_empty(),
        Feature::Tts => !creds.tts.api_key.trim().is_empty(),
        Feature::Vision => !creds.vision.api_key.trim().is_empty(),
        Feature::Image => !creds.image.api_key.trim().is_empty(),
        Feature::Video => !creds.video.api_key.trim().is_empty(),
    }
}

/// Hand a URL to the system browser (best-effort; a spawn error is logged).
fn open_url(url: &str) {
    if let Err(e) = std::process::Command::new("open").arg(url).spawn() {
        tracing::error!(error = %e, "settings: failed to open url");
    }
}

/// Set a label's string value.
fn set_label(field: &NSTextField, text: &str) {
    field.setStringValue(&NSString::from_str(text));
}

/// Style a "Use …" button by the active state: active reads "✓ <title>" and is disabled
/// (nothing to do), inactive reads the plain title and is enabled.
fn set_use_button(btn: &NSButton, title: &str, active: bool) {
    let text = if active { format!("✓ {title} (active)") } else { title.to_string() };
    btn.setTitle(&NSString::from_str(&text));
    btn.setEnabled(!active);
}

// ---------------------------------------------------------------------------
// Small AppKit builders (manual frames, y grows up — same idiom as macos_account.rs)
// ---------------------------------------------------------------------------

/// A left-aligned static label.
fn label(mtm: MainThreadMarker, text: &str, frame: NSRect) -> Retained<NSTextField> {
    let l = NSTextField::labelWithString(&NSString::from_str(text), mtm);
    l.setFrame(frame);
    l
}

/// A popup button pre-filled with an option list's labels.
fn popup(mtm: MainThreadMarker, list: &[(&str, &str)], frame: NSRect) -> Retained<NSPopUpButton> {
    // SAFETY: main-thread AppKit construction; every item title is an owned NSString.
    unsafe {
        let p: Retained<NSPopUpButton> =
            msg_send![NSPopUpButton::alloc(mtm), initWithFrame: frame, pullsDown: false];
        for (_, lbl) in list {
            p.addItemWithTitle(&NSString::from_str(lbl));
        }
        p
    }
}

/// A standard push button (no target yet — wired after the host exists).
fn push_button(_mtm: MainThreadMarker, title: &str, frame: NSRect) -> Retained<NSButton> {
    // SAFETY: main-thread AppKit construction; nil target/action set later.
    unsafe {
        let b: Retained<NSButton> = msg_send![
            NSButton::class(),
            buttonWithTitle: &*NSString::from_str(title),
            target: core::ptr::null_mut::<AnyObject>(),
            action: None::<Sel>,
        ];
        b.setFrame(frame);
        b
    }
}

/// A checkbox (no target yet).
fn checkbox(_mtm: MainThreadMarker, title: &str, frame: NSRect) -> Retained<NSButton> {
    // SAFETY: main-thread AppKit construction; nil target/action set later.
    unsafe {
        let b: Retained<NSButton> = msg_send![
            NSButton::class(),
            checkboxWithTitle: &*NSString::from_str(title),
            target: core::ptr::null_mut::<AnyObject>(),
            action: None::<Sel>,
        ];
        b.setFrame(frame);
        b
    }
}

/// A y-coordinate for row `i` from the top of a pane of height `h` (y grows up).
fn row_y(h: f64, i: f64) -> f64 {
    h - MARGIN - ROW_H - i * (ROW_H + 12.0)
}

/// Wire a control's target + action to the host.
fn wire(control: &AnyObject, host: &Host, action: Sel) {
    // SAFETY: main-thread AppKit setters; the host outlives the control (both leaked).
    unsafe {
        let _: () = msg_send![control, setTarget: host];
        let _: () = msg_send![control, setAction: action];
    }
}

// ---------------------------------------------------------------------------
// Install / open
// ---------------------------------------------------------------------------

struct HostPtr(*const Host);
unsafe impl Send for HostPtr {}
unsafe impl Sync for HostPtr {}

static HOST: OnceLock<HostPtr> = OnceLock::new();

/// Build the settings window (hidden until [`open`]). Called once from the tray's
/// main-thread setup; leaks its AppKit objects for the process lifetime like the siblings.
pub fn install(mtm: MainThreadMarker, data_dir: PathBuf) {
    // SAFETY: standard AppKit construction on the main thread (guaranteed by `mtm`); every
    // object is leaked below, so none is used after free — they live for the process.
    unsafe {
        let content = NSRect::new(NSPoint::new(0.0, 0.0), NSSize::new(WIN_W, WIN_H));
        let style = NSWindowStyleMask::Titled
            | NSWindowStyleMask::Closable
            | NSWindowStyleMask::Miniaturizable;
        let window: Retained<NSWindow> = msg_send![
            NSWindow::alloc(mtm),
            initWithContentRect: content,
            styleMask: style,
            backing: NSBackingStoreType::Buffered,
            defer: false,
        ];
        window.setTitle(&NSString::from_str("Settings"));
        let _: () = msg_send![&*window, setReleasedWhenClosed: false];

        // --- sidebar: a source-list table in a scroll view ---
        let sidebar_frame = NSRect::new(NSPoint::new(0.0, 0.0), NSSize::new(SIDEBAR_W, WIN_H));
        let scroll: Retained<NSScrollView> = msg_send![NSScrollView::alloc(mtm), initWithFrame: sidebar_frame];
        let table: Retained<NSTableView> = msg_send![
            NSTableView::alloc(mtm),
            initWithFrame: NSRect::new(NSPoint::new(0.0, 0.0), NSSize::new(SIDEBAR_W, WIN_H))
        ];
        let column: Retained<NSTableColumn> = msg_send![
            NSTableColumn::alloc(mtm),
            initWithIdentifier: &*NSString::from_str("section")
        ];
        column.setWidth(SIDEBAR_W - 8.0);
        table.addTableColumn(&column);
        table.setHeaderView(None);
        table.setStyle(NSTableViewStyle::SourceList);
        let _: () = msg_send![&*scroll, setDocumentView: &*table];
        scroll.setHasVerticalScroller(true);

        // --- detail container + the three panes ---
        let detail = NSView::initWithFrame(
            NSView::alloc(mtm),
            NSRect::new(NSPoint::new(SIDEBAR_W, 0.0), NSSize::new(DETAIL_W, WIN_H)),
        );
        let (general, theme_popup, lang_popup, gestures_check) = build_general(mtm);
        let (account, plan_label, energy_label, resets_label, use_xyz, manage, use_byok, byok_status, edit_buttons) =
            build_account(mtm);
        let (about, visit) = build_about(mtm);

        // Content view holds the sidebar scroll view + the detail container.
        let container = NSView::initWithFrame(NSView::alloc(mtm), content);
        container.addSubview(&scroll);
        container.addSubview(&detail);
        let _: () = msg_send![&*window, setContentView: &*container];

        // General shows first.
        detail.addSubview(&general);

        // --- assemble the host with every reference, then wire targets ---
        let panes = [general, account, about];
        let host = Host::alloc(mtm).set_ivars(Ivars {
            data_dir,
            window: window.clone(),
            detail: detail.clone(),
            panes,
            current: Cell::new(0),
            theme_popup: theme_popup.clone(),
            lang_popup: lang_popup.clone(),
            gestures_check: gestures_check.clone(),
            plan_label,
            energy_label,
            resets_label,
            use_xyz: use_xyz.clone(),
            use_byok: use_byok.clone(),
            byok_status,
        });
        let host: Retained<Host> = msg_send![super(host), init];

        // Table data source + delegate.
        table.setDataSource(Some(ProtocolObject::from_ref(&*host)));
        table.setDelegate(Some(ProtocolObject::from_ref(&*host)));
        table.reloadData();
        // Select General so its row highlights (the pane already shows).
        table.selectRowIndexes_byExtendingSelection(&NSIndexSet::indexSetWithIndex(0), false);

        // Wire every control to the host.
        wire(&theme_popup, &host, sel!(themeChanged:));
        wire(&lang_popup, &host, sel!(languageChanged:));
        wire(&gestures_check, &host, sel!(gesturesToggled:));
        wire(&use_xyz, &host, sel!(useXiaoyuanzhu:));
        wire(&use_byok, &host, sel!(useByok:));
        wire(&manage, &host, sel!(manageAccount:));
        wire(&visit, &host, sel!(visitWebsite:));
        std::mem::forget(manage);
        std::mem::forget(visit);
        for (btn, tag) in edit_buttons {
            btn.setTag(tag);
            wire(&btn, &host, sel!(editFeature:));
            std::mem::forget(btn);
        }

        // Initial control state from the store.
        host.sync_general();
        host.sync_account();

        let ptr: *const Host = &*host;
        std::mem::forget(host);
        std::mem::forget(window);
        std::mem::forget(scroll);
        std::mem::forget(table);
        std::mem::forget(column);
        std::mem::forget(container);
        let _ = HOST.set(HostPtr(ptr));
    }
    tracing::info!("settings window installed");
}

/// Show + focus the settings window (idempotent). Safe to call from any thread — a no-op
/// until [`install`] has published the host (headless / before the tray loads).
pub fn open() {
    let Some(host) = HOST.get() else { return };
    // SAFETY: `host.0` is the leaked, process-lived Host; `performSelectorOnMainThread:`
    // is callable from any thread and hops `open:` onto the main run loop.
    unsafe {
        let obj: &Host = &*host.0;
        let _: () = msg_send![
            obj,
            performSelectorOnMainThread: sel!(open:),
            withObject: core::ptr::null_mut::<AnyObject>(),
            waitUntilDone: false
        ];
    }
}

// ---------------------------------------------------------------------------
// Pane builders
// ---------------------------------------------------------------------------

/// General: theme + language popups and the attention-gestures checkbox.
fn build_general(
    mtm: MainThreadMarker,
) -> (Retained<NSView>, Retained<NSPopUpButton>, Retained<NSPopUpButton>, Retained<NSButton>) {
    let pane = NSView::initWithFrame(
        NSView::alloc(mtm),
        NSRect::new(NSPoint::new(0.0, 0.0), NSSize::new(DETAIL_W, WIN_H)),
    );
    let field_x = MARGIN + LABEL_W;
    let field_w = DETAIL_W - field_x - MARGIN;

    let y0 = row_y(WIN_H, 0.0);
    pane.addSubview(&label(mtm, "Language", NSRect::new(NSPoint::new(MARGIN, y0), NSSize::new(LABEL_W, ROW_H))));
    let lang_popup = popup(mtm, LANGUAGES, NSRect::new(NSPoint::new(field_x, y0 - 2.0), NSSize::new(field_w, ROW_H + 2.0)));
    pane.addSubview(&lang_popup);

    let y1 = row_y(WIN_H, 1.0);
    pane.addSubview(&label(mtm, "Theme", NSRect::new(NSPoint::new(MARGIN, y1), NSSize::new(LABEL_W, ROW_H))));
    let theme_popup = popup(mtm, THEMES, NSRect::new(NSPoint::new(field_x, y1 - 2.0), NSSize::new(field_w, ROW_H + 2.0)));
    pane.addSubview(&theme_popup);

    let y2 = row_y(WIN_H, 2.0);
    let gestures_check = checkbox(
        mtm,
        "Attention gestures (right-⌘) — applies after restart",
        NSRect::new(NSPoint::new(MARGIN, y2), NSSize::new(DETAIL_W - 2.0 * MARGIN, ROW_H)),
    );
    pane.addSubview(&gestures_check);

    (pane, theme_popup, lang_popup, gestures_check)
}

/// Account: an NSTabView with the 小圆猪 (surface) and BYOK (config) tabs. Returns every
/// control the host needs to sync/wire, plus the per-feature Edit buttons with their tags.
#[allow(clippy::type_complexity)]
fn build_account(
    mtm: MainThreadMarker,
) -> (
    Retained<NSView>,
    Retained<NSTextField>,
    Retained<NSTextField>,
    Retained<NSTextField>,
    Retained<NSButton>,
    Retained<NSButton>,
    Retained<NSButton>,
    [Retained<NSTextField>; 6],
    Vec<(Retained<NSButton>, isize)>,
) {
    let pane = NSView::initWithFrame(
        NSView::alloc(mtm),
        NSRect::new(NSPoint::new(0.0, 0.0), NSSize::new(DETAIL_W, WIN_H)),
    );
    let tab_frame = NSRect::new(
        NSPoint::new(MARGIN, MARGIN),
        NSSize::new(DETAIL_W - 2.0 * MARGIN, WIN_H - 2.0 * MARGIN),
    );
    // SAFETY: main-thread AppKit construction.
    let tabs: Retained<NSTabView> = unsafe { msg_send![NSTabView::alloc(mtm), initWithFrame: tab_frame] };
    let inner_w = DETAIL_W - 2.0 * MARGIN;
    let inner_h = WIN_H - 2.0 * MARGIN;

    // --- 小圆猪 tab: surface-only account + quota ---
    let xyz = NSView::initWithFrame(
        NSView::alloc(mtm),
        NSRect::new(NSPoint::new(0.0, 0.0), NSSize::new(inner_w, inner_h)),
    );
    let lw = inner_w - 2.0 * MARGIN;
    let plan_label = label(mtm, "Plan: —", NSRect::new(NSPoint::new(MARGIN, row_y(inner_h, 0.0)), NSSize::new(lw, ROW_H)));
    let energy_label = label(mtm, "Energy: —", NSRect::new(NSPoint::new(MARGIN, row_y(inner_h, 1.0)), NSSize::new(lw, ROW_H)));
    let resets_label = label(mtm, "Resets: —", NSRect::new(NSPoint::new(MARGIN, row_y(inner_h, 2.0)), NSSize::new(lw, ROW_H)));
    xyz.addSubview(&plan_label);
    xyz.addSubview(&energy_label);
    xyz.addSubview(&resets_label);
    let use_xyz = push_button(mtm, "Use 小圆猪", NSRect::new(NSPoint::new(MARGIN, row_y(inner_h, 4.0)), NSSize::new(180.0, ROW_H + 4.0)));
    let manage = push_button(mtm, "Manage on the web…", NSRect::new(NSPoint::new(MARGIN + 190.0, row_y(inner_h, 4.0)), NSSize::new(190.0, ROW_H + 4.0)));
    xyz.addSubview(&use_xyz);
    xyz.addSubview(&manage);

    // --- BYOK tab: a status row + Edit button per feature, plus "Use my own keys" ---
    let byok = NSView::initWithFrame(
        NSView::alloc(mtm),
        NSRect::new(NSPoint::new(0.0, 0.0), NSSize::new(inner_w, inner_h)),
    );
    let mut edit_buttons: Vec<(Retained<NSButton>, isize)> = Vec::new();
    let mut status_vec: Vec<Retained<NSTextField>> = Vec::new();
    for (i, (_lbl, tag, _feature)) in FEATURES.iter().enumerate() {
        let y = row_y(inner_h, i as f64);
        let status = label(mtm, "—", NSRect::new(NSPoint::new(MARGIN, y), NSSize::new(inner_w - 2.0 * MARGIN - 90.0, ROW_H)));
        byok.addSubview(&status);
        status_vec.push(status);
        let edit = push_button(mtm, "Edit…", NSRect::new(NSPoint::new(inner_w - MARGIN - 80.0, y - 2.0), NSSize::new(80.0, ROW_H + 4.0)));
        byok.addSubview(&edit);
        edit_buttons.push((edit, *tag));
    }
    let use_byok = push_button(mtm, "Use my own keys", NSRect::new(NSPoint::new(MARGIN, row_y(inner_h, FEATURES.len() as f64 + 0.5)), NSSize::new(200.0, ROW_H + 4.0)));
    byok.addSubview(&use_byok);

    // SAFETY: main-thread AppKit; add both tabs.
    unsafe {
        let t1: Retained<NSTabViewItem> = msg_send![NSTabViewItem::alloc(), initWithIdentifier: core::ptr::null_mut::<AnyObject>()];
        t1.setLabel(&NSString::from_str("小圆猪"));
        let _: () = msg_send![&*t1, setView: &*xyz];
        tabs.addTabViewItem(&t1);
        std::mem::forget(t1);
        let t2: Retained<NSTabViewItem> = msg_send![NSTabViewItem::alloc(), initWithIdentifier: core::ptr::null_mut::<AnyObject>()];
        t2.setLabel(&NSString::from_str("Your own keys"));
        let _: () = msg_send![&*t2, setView: &*byok];
        tabs.addTabViewItem(&t2);
        std::mem::forget(t2);
    }
    pane.addSubview(&tabs);
    std::mem::forget(tabs);

    let status_arr: [Retained<NSTextField>; 6] = status_vec
        .try_into()
        .unwrap_or_else(|_| panic!("FEATURES has exactly 6 entries"));

    (pane, plan_label, energy_label, resets_label, use_xyz, manage, use_byok, status_arr, edit_buttons)
}

/// About: name, version, blurb, and a website link. Returns the pane and the "Visit"
/// button (wired to the host in `install`).
fn build_about(mtm: MainThreadMarker) -> (Retained<NSView>, Retained<NSButton>) {
    let pane = NSView::initWithFrame(
        NSView::alloc(mtm),
        NSRect::new(NSPoint::new(0.0, 0.0), NSSize::new(DETAIL_W, WIN_H)),
    );
    let w = DETAIL_W - 2.0 * MARGIN;
    pane.addSubview(&label(mtm, "Hi Agent", NSRect::new(NSPoint::new(MARGIN, row_y(WIN_H, 0.0)), NSSize::new(w, ROW_H))));
    pane.addSubview(&label(
        mtm,
        &format!("Version {}", env!("CARGO_PKG_VERSION")),
        NSRect::new(NSPoint::new(MARGIN, row_y(WIN_H, 1.0)), NSSize::new(w, ROW_H)),
    ));
    pane.addSubview(&label(
        mtm,
        "Your warm, capable companion — always glad to help.",
        NSRect::new(NSPoint::new(MARGIN, row_y(WIN_H, 2.0)), NSSize::new(w, ROW_H)),
    ));
    let visit = push_button(mtm, "Visit hi.xiaoyuanzhu.com", NSRect::new(NSPoint::new(MARGIN, row_y(WIN_H, 4.0)), NSSize::new(220.0, ROW_H + 4.0)));
    pane.addSubview(&visit);
    (pane, visit)
}
