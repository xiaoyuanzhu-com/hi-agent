//! macOS account vendor — the **per-feature BYOK configuration popup**.
//!
//! A small modal `NSAlert` with an accessory view of text fields (API key +
//! optional base URL / model), opened from a row in the tray's
//! `Account ▸ Your own keys ▸ <feature>` submenu ([`super::macos_tray`]). On *Save*
//! it writes that feature's credential fields to the store in-process
//! ([`crate::foundation::credentials`]); no HTTP round-trip. Credentials are read
//! at bootstrap, so a change applies on the next restart (the text says so).
//!
//! Configuring a key does **not** switch the active mode — selection (which source
//! powers the agent) is a separate choice made by the `Use my own keys` row. So a
//! key can be entered while still on the managed account, and a mode can be BYOK
//! with nothing configured yet (a valid, if not-yet-working, state).
//!
//! Runs entirely on the process main thread: it's invoked directly from a menu
//! action (already main-thread) and `NSAlert::runModal` blocks there for the
//! dialog's lifetime. The AppKit objects are transient to this call — the alert is
//! torn down when `runModal` returns.

use std::path::Path;

use objc2::rc::Retained;
use objc2::{msg_send, MainThreadOnly};
use objc2_app_kit::{NSAlert, NSApplication, NSSecureTextField, NSTextField, NSView};
use objc2_foundation::{MainThreadMarker, NSPoint, NSRect, NSSize, NSString};

use crate::foundation::credentials::{Credentials, VendorKey};

/// `NSAlertFirstButtonReturn` — the first button added (Save) in a run-modal alert.
const FIRST_BUTTON: isize = 1000;

/// Accessory layout (points). A single column of stacked fields; y grows upward in
/// AppKit, so the key sits at the top and the model at the bottom.
const W: f64 = 320.0;
const FIELD_H: f64 = 24.0;
const GAP: f64 = 8.0;

/// A keyed capability whose BYOK credential can be configured from the tray. Each
/// maps to a field on the credential store; `Llm` is the flat `llm` fields, the
/// rest are `VendorKey`s. Face/voiceprint are local ONNX (no key) and absent here.
#[derive(Clone, Copy)]
pub enum Feature {
    Llm,
    Stt,
    Tts,
    Vision,
    Image,
    Video,
}

impl Feature {
    /// Human title shown in the dialog.
    fn title(self) -> &'static str {
        match self {
            Feature::Llm => "LLM (Anthropic-compatible)",
            Feature::Stt => "Speech-to-text",
            Feature::Tts => "Text-to-speech",
            Feature::Vision => "Vision",
            Feature::Image => "Image",
            Feature::Video => "Video",
        }
    }

    /// Placeholder for the key field when nothing is stored yet.
    fn key_placeholder(self) -> &'static str {
        match self {
            Feature::Llm => "API key (sk-ant-…)",
            _ => "API key",
        }
    }
}

/// Show the config dialog for `feature` and, on Save, persist its fields. Does not
/// change the active mode. Main-thread only (called from a menu action). Best-
/// effort: a save error is logged, not surfaced — worst case the user re-enters it.
pub fn configure_feature(mtm: MainThreadMarker, data_dir: &Path, feature: Feature) {
    let mut creds = Credentials::load(data_dir);
    let (configured, cur_base, cur_model) = read_fields(&creds, feature);

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
        let key_ph = if configured { "•••• (unchanged)" } else { feature.key_placeholder() };
        set_placeholder(&key_field, key_ph);

        // Base URL — optional; prefill the stored value (empty ⇒ vendor default).
        let base_field: Retained<NSTextField> = msg_send![
            NSTextField::alloc(mtm),
            initWithFrame: NSRect::new(NSPoint::new(0.0, y_base), NSSize::new(W, FIELD_H)),
        ];
        set_placeholder_plain(&base_field, "Base URL — optional");
        base_field.setStringValue(&NSString::from_str(&cur_base));

        // Model — optional; prefill the stored override (empty ⇒ vendor default).
        let model_field: Retained<NSTextField> = msg_send![
            NSTextField::alloc(mtm),
            initWithFrame: NSRect::new(NSPoint::new(0.0, y_model), NSSize::new(W, FIELD_H)),
        ];
        set_placeholder_plain(&model_field, "Model — optional (vendor default)");
        model_field.setStringValue(&NSString::from_str(&cur_model));

        container.addSubview(&*key_field);
        container.addSubview(&*base_field);
        container.addSubview(&*model_field);

        let alert: Retained<NSAlert> = msg_send![NSAlert::alloc(mtm), init];
        alert.setMessageText(&NSString::from_str(&format!("Your own {} key", feature.title())));
        alert.setInformativeText(&NSString::from_str(
            "Stored locally in this app. Restart Hi Agent to apply. Leave the key blank to keep the current one. This does not switch which account is active.",
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
            return; // Cancel — leave the store untouched.
        }

        // Save this feature's fields. A blank key keeps the existing one; base/model
        // are set as-is (empty clears back to the built-in default). Mode is left
        // alone — selection is the `Use …` row's job.
        let key = field_string(&key_field);
        let base = field_string(&base_field);
        let model = field_string(&model_field);
        write_fields(&mut creds, feature, &key, &base, &model);

        if let Err(e) = creds.save(data_dir) {
            tracing::error!(error = %e, "account: failed to save BYOK credentials");
        } else {
            tracing::info!(feature = feature.title(), "account: saved BYOK credentials (restart to apply)");
        }
    }
}

/// Snapshot a feature's stored fields: `(has_key, base_url, model)`.
fn read_fields(creds: &Credentials, feature: Feature) -> (bool, String, String) {
    let vk = match feature {
        Feature::Llm => {
            return (
                !creds.llm.api_key.trim().is_empty(),
                creds.llm.base_url.clone(),
                creds.llm.model.clone().unwrap_or_default(),
            );
        }
        Feature::Stt => &creds.stt,
        Feature::Tts => &creds.tts,
        Feature::Vision => &creds.vision,
        Feature::Image => &creds.image,
        Feature::Video => &creds.video,
    };
    (!vk.api_key.trim().is_empty(), vk.base_url.clone(), vk.model.clone().unwrap_or_default())
}

/// Write a feature's fields back (blank key = keep existing).
fn write_fields(creds: &mut Credentials, feature: Feature, key: &str, base: &str, model: &str) {
    let key = key.trim();
    let base = base.trim().to_string();
    let m = model.trim();
    let model = if m.is_empty() { None } else { Some(m.to_string()) };
    match feature {
        Feature::Llm => {
            if !key.is_empty() {
                creds.llm.api_key = key.to_string();
            }
            creds.llm.base_url = base;
            creds.llm.model = model;
        }
        Feature::Stt => write_vendor(&mut creds.stt, key, base, model),
        Feature::Tts => write_vendor(&mut creds.tts, key, base, model),
        Feature::Vision => write_vendor(&mut creds.vision, key, base, model),
        Feature::Image => write_vendor(&mut creds.image, key, base, model),
        Feature::Video => write_vendor(&mut creds.video, key, base, model),
    }
}

fn write_vendor(vk: &mut VendorKey, key: &str, base: String, model: Option<String>) {
    if !key.is_empty() {
        vk.api_key = key.to_string();
    }
    vk.base_url = base;
    vk.model = model;
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
