//! macOS account vendor — the **BYOK configuration popup**.
//!
//! A small modal `NSAlert` with an accessory view of text fields (Anthropic API key
//! + optional base URL / model), opened from the tray's `Account ▸ BYOK…` item
//! ([`super::macos_tray`]). On *Save* it writes the credential store in-process
//! ([`crate::foundation::credentials`]) — flipping `mode` to `Byok` and storing the
//! LLM fields — so no HTTP round-trip is involved; the web Settings page it replaced
//! is gone. Credentials are read at bootstrap, so a change applies on the next
//! restart (the informative text says so).
//!
//! Runs entirely on the process main thread: it's invoked directly from a menu
//! action (already main-thread) and `NSAlert::runModal` blocks there for the
//! dialog's lifetime. Like the tray/popover, the AppKit objects are transient to
//! this call — the alert is torn down when `runModal` returns.
//!
//! Only the LLM credential lives here. Vendor keys (speech/vision/media) are
//! intentionally *not* surfaced — they're advanced/env, matching the collapse of
//! the config surface down to the one bootstrap decision.

use std::path::Path;

use objc2::rc::Retained;
use objc2::{msg_send, MainThreadOnly};
use objc2_app_kit::{
    NSAlert, NSApplication, NSSecureTextField, NSTextField, NSView,
};
use objc2_foundation::{MainThreadMarker, NSPoint, NSRect, NSSize, NSString};

use crate::foundation::credentials::{Credentials, Mode};

/// `NSAlertFirstButtonReturn` — the first button added (Save) in a run-modal alert.
const FIRST_BUTTON: isize = 1000;

/// Accessory layout (points). A single column of stacked fields; y grows upward in
/// AppKit, so the key sits at the top and the model at the bottom.
const W: f64 = 320.0;
const FIELD_H: f64 = 24.0;
const GAP: f64 = 8.0;

/// Show the BYOK configuration dialog and, on Save, persist to the credential store.
/// Main-thread only (called from the tray's menu action). Best-effort: a save error
/// is logged, not surfaced — the worst case is the user re-enters the key.
pub fn configure(mtm: MainThreadMarker, data_dir: &Path) {
    let mut creds = Credentials::load(data_dir);
    let configured = !creds.llm.api_key.trim().is_empty();

    // SAFETY: standard AppKit construction on the main thread (guaranteed by `mtm`).
    // Every object lives on the stack for the duration of `runModal` and is released
    // when this function returns; nothing is retained past the call.
    unsafe {
        // Three stacked rows: key (top), base URL, model (bottom).
        let total_h = 3.0 * FIELD_H + 2.0 * GAP;
        let container: Retained<NSView> = msg_send![
            NSView::alloc(mtm),
            initWithFrame: NSRect::new(NSPoint::new(0.0, 0.0), NSSize::new(W, total_h)),
        ];

        let y_key = 2.0 * (FIELD_H + GAP);
        let y_base = FIELD_H + GAP;
        let y_model = 0.0;

        // API key — a secure field. Never prefilled; the placeholder shows the
        // stored-key state instead so a blank Save keeps the existing key.
        let key_field: Retained<NSSecureTextField> = msg_send![
            NSSecureTextField::alloc(mtm),
            initWithFrame: NSRect::new(NSPoint::new(0.0, y_key), NSSize::new(W, FIELD_H)),
        ];
        let key_ph = if configured {
            "•••• (unchanged)"
        } else {
            "Anthropic API key (sk-ant-…)"
        };
        set_placeholder(&key_field, key_ph);

        // Base URL — optional; prefill the stored value (empty ⇒ Anthropic default).
        let base_field: Retained<NSTextField> = msg_send![
            NSTextField::alloc(mtm),
            initWithFrame: NSRect::new(NSPoint::new(0.0, y_base), NSSize::new(W, FIELD_H)),
        ];
        set_placeholder_plain(&base_field, "Base URL — optional (https://api.anthropic.com)");
        base_field.setStringValue(&NSString::from_str(&creds.llm.base_url));

        // Model — optional; prefill the stored override (empty ⇒ adapter default).
        let model_field: Retained<NSTextField> = msg_send![
            NSTextField::alloc(mtm),
            initWithFrame: NSRect::new(NSPoint::new(0.0, y_model), NSSize::new(W, FIELD_H)),
        ];
        set_placeholder_plain(&model_field, "Model — optional (adapter default)");
        model_field.setStringValue(&NSString::from_str(creds.llm.model.as_deref().unwrap_or("")));

        container.addSubview(&*key_field);
        container.addSubview(&*base_field);
        container.addSubview(&*model_field);

        let alert: Retained<NSAlert> = msg_send![NSAlert::alloc(mtm), init];
        alert.setMessageText(&NSString::from_str("Use your own API key (BYOK)"));
        alert.setInformativeText(&NSString::from_str(
            "Stored locally in this app. Restart Hi Agent to apply. Leave the key blank to keep the current one.",
        ));
        // Buttons: first added becomes the default (Save → FIRST_BUTTON). We only
        // need the run-modal response code, so the returned NSButton is ignored.
        let _: () = msg_send![&*alert, addButtonWithTitle: &*NSString::from_str("Save")];
        let _: () = msg_send![&*alert, addButtonWithTitle: &*NSString::from_str("Cancel")];
        alert.setAccessoryView(Some(&*container));

        // Bring the app forward so the alert's fields can take keystrokes under the
        // Accessory activation policy (same reason the popover activates on show).
        let app = NSApplication::sharedApplication(mtm);
        let _: () = msg_send![&*app, activateIgnoringOtherApps: true];

        let response: isize = msg_send![&*alert, runModal];
        if response != FIRST_BUTTON {
            return; // Cancel — leave the store untouched (mode stays as it was).
        }

        // Save: switch to BYOK and store the LLM fields. A blank key keeps the
        // existing one (matches the old web form); base/model are set as-is (empty
        // clears back to the built-in default).
        let key = field_string(&key_field);
        let base = field_string(&base_field);
        let model = field_string(&model_field);

        creds.mode = Mode::Byok;
        if !key.trim().is_empty() {
            creds.llm.api_key = key.trim().to_string();
        }
        creds.llm.base_url = base.trim().to_string();
        let m = model.trim();
        creds.llm.model = if m.is_empty() { None } else { Some(m.to_string()) };

        if let Err(e) = creds.save(data_dir) {
            tracing::error!(error = %e, "account: failed to save BYOK credentials");
        } else {
            tracing::info!("account: saved BYOK credentials (restart to apply)");
        }
    }
}

/// Read an `NSControl`'s string value as a Rust `String`.
fn field_string(field: &NSTextField) -> String {
    field.stringValue().to_string()
}

/// Set an `NSTextField`'s placeholder (the `NSTextFieldCell` string).
fn set_placeholder_plain(field: &NSTextField, text: &str) {
    // SAFETY: main-thread AppKit setter; `placeholderString` is a plain NSString.
    unsafe {
        let _: () = msg_send![field, setPlaceholderString: &*NSString::from_str(text)];
    }
}

/// Set a secure field's placeholder (same selector, distinct type).
fn set_placeholder(field: &NSSecureTextField, text: &str) {
    // SAFETY: main-thread AppKit setter inherited from NSTextField.
    unsafe {
        let _: () = msg_send![field, setPlaceholderString: &*NSString::from_str(text)];
    }
}
