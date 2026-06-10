//! macOS desktop context — captures the screen, clipboard, and frontmost-app
//! state for [`crate::capabilities::desktop_context`].
//!
//! The "API" here is the OS's stable CLI surface rather than framework
//! bindings — zero extra crate dependencies, and each tool degrades cleanly
//! when a permission is missing:
//!
//!   /usr/sbin/screencapture   screenshot          needs Screen Recording
//!   /usr/bin/pbpaste          clipboard text      no permission
//!   /usr/bin/osascript        clipboard image,    Automation ("System
//!                             window title,        Events" / per-browser),
//!                             browser URL          prompted once per host app
//!   /usr/bin/lsappinfo        frontmost app name  no permission
//!
//! Permissions attach to the *responsible* app: in dev that is the terminal
//! that launched the process; a bundled .app must hold the grants itself.
//!
//! Every capture is best-effort. Expected absences (empty clipboard, app with
//! no window, non-browser frontmost) log at `debug`; real failures (screenshot
//! refused) log at `warn`. Output parsing is kept in pure functions so the
//! wire shapes are unit-testable without a GUI session.

use anyhow::Context;
use bytes::Bytes;
use chrono::Utc;
use tokio::process::Command;

use crate::capabilities::desktop_context::ContextSnapshot;

/// Take a best-effort snapshot. Never fails: each field is independently
/// captured and a failed field is `None`.
pub async fn capture() -> ContextSnapshot {
    let captured_at = Utc::now();
    let (screenshot, clip_text, clip_image, app, title) = tokio::join!(
        screenshot_png(),
        clipboard_text(),
        clipboard_image_png(),
        frontmost_app(),
        frontmost_window_title(),
    );

    let frontmost_app = warn_missing("frontmost app", app);
    let browser_url = match frontmost_app.as_deref().and_then(browser_url_script) {
        Some(script) => debug_missing("browser url", osascript(&script).await)
            .map(|s| s.trim().to_string())
            .filter(|s| !s.is_empty()),
        None => None,
    };

    ContextSnapshot {
        captured_at,
        screenshot_png: warn_missing("screenshot", screenshot),
        clipboard_text: debug_missing("clipboard text", clip_text).flatten(),
        clipboard_image_png: debug_missing("clipboard image", clip_image),
        frontmost_app,
        frontmost_window_title: debug_missing("window title", title)
            .map(|s| s.trim().to_string())
            .filter(|s| !s.is_empty()),
        browser_url,
    }
}

/// A field whose absence usually means a denied permission or broken tool.
fn warn_missing<T>(field: &str, r: anyhow::Result<T>) -> Option<T> {
    r.inspect_err(|e| tracing::warn!("desktop context: {field} unavailable: {e:#}")).ok()
}

/// A field whose absence is routine (empty clipboard, windowless app, …).
fn debug_missing<T>(field: &str, r: anyhow::Result<T>) -> Option<T> {
    r.inspect_err(|e| tracing::debug!("desktop context: {field} absent: {e:#}")).ok()
}

async fn screenshot_png() -> anyhow::Result<Bytes> {
    let path = std::env::temp_dir().join(format!("hi-agent-ctx-{}.png", uuid::Uuid::now_v7()));
    // -x: no shutter sound; -C: include the cursor (shows where attention is).
    let status = Command::new("/usr/sbin/screencapture")
        .args(["-x", "-C", "-t", "png"])
        .arg(&path)
        .status()
        .await
        .context("spawning screencapture")?;
    anyhow::ensure!(status.success(), "screencapture exited {status}");
    let bytes = tokio::fs::read(&path).await.context("reading screencapture output")?;
    let _ = tokio::fs::remove_file(&path).await;
    Ok(Bytes::from(bytes))
}

async fn clipboard_text() -> anyhow::Result<Option<String>> {
    // pbpaste emits in the LANG encoding; pin UTF-8 so non-ASCII survives.
    let out = Command::new("/usr/bin/pbpaste")
        .env("LANG", "en_US.UTF-8")
        .output()
        .await
        .context("spawning pbpaste")?;
    anyhow::ensure!(out.status.success(), "pbpaste exited {}", out.status);
    let text = String::from_utf8_lossy(&out.stdout).into_owned();
    Ok(if text.is_empty() { None } else { Some(text) })
}

async fn clipboard_image_png() -> anyhow::Result<Bytes> {
    // Errors with -1700 when the clipboard has no PNG flavor — routine.
    let out = osascript("the clipboard as «class PNGf»").await?;
    parse_osascript_data(&out, "PNGf")
        .map(Bytes::from)
        .ok_or_else(|| anyhow::anyhow!("unexpected osascript data output"))
}

async fn frontmost_app() -> anyhow::Result<String> {
    let asn = run("/usr/bin/lsappinfo", &["front"]).await?;
    let asn = asn.trim();
    anyhow::ensure!(!asn.is_empty(), "lsappinfo front returned nothing");
    let info = run("/usr/bin/lsappinfo", &["info", "-only", "name", asn]).await?;
    parse_quoted_value(&info)
        .ok_or_else(|| anyhow::anyhow!("unexpected lsappinfo output: {info:?}"))
}

async fn frontmost_window_title() -> anyhow::Result<String> {
    osascript(
        "tell application \"System Events\" to tell \
         (first application process whose frontmost is true) to get name of front window",
    )
    .await
}

/// AppleScript that asks the named browser for its current tab's URL, or
/// `None` for apps that aren't a known scriptable browser. Matching on exact
/// names (and embedding the matched literal, never the input) keeps the
/// scripts static.
fn browser_url_script(app: &str) -> Option<String> {
    let (name, dialect) = match app {
        "Safari" | "Safari Technology Preview" => (app, "URL of front document"),
        "Google Chrome" | "Google Chrome Canary" | "Chromium" | "Microsoft Edge"
        | "Brave Browser" | "Vivaldi" | "Opera" | "Arc" => {
            (app, "URL of active tab of front window")
        }
        _ => return None,
    };
    Some(format!("tell application \"{name}\" to get {dialect}"))
}

async fn osascript(script: &str) -> anyhow::Result<String> {
    run("/usr/bin/osascript", &["-e", script]).await
}

async fn run(cmd: &str, args: &[&str]) -> anyhow::Result<String> {
    let out = Command::new(cmd)
        .args(args)
        .output()
        .await
        .with_context(|| format!("spawning {cmd}"))?;
    anyhow::ensure!(
        out.status.success(),
        "{cmd} exited {}: {}",
        out.status,
        String::from_utf8_lossy(&out.stderr).trim()
    );
    Ok(String::from_utf8_lossy(&out.stdout).into_owned())
}

/// Parse osascript's raw-data literal, e.g. `«data PNGf89504E47…»`, into bytes.
fn parse_osascript_data(out: &str, class: &str) -> Option<Vec<u8>> {
    let hex = out
        .trim()
        .strip_prefix(&format!("«data {class}"))?
        .strip_suffix('»')?;
    decode_hex(hex)
}

fn decode_hex(s: &str) -> Option<Vec<u8>> {
    if s.len() % 2 != 0 {
        return None;
    }
    (0..s.len())
        .step_by(2)
        .map(|i| u8::from_str_radix(s.get(i..i + 2)?, 16).ok())
        .collect()
}

/// Pull the value out of lsappinfo's `"key"="value"` line.
fn parse_quoted_value(line: &str) -> Option<String> {
    let value = line.trim().split_once('=')?.1.trim().trim_matches('"');
    (!value.is_empty()).then(|| value.to_string())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_osascript_png_data() {
        let out = "«data PNGf89504E470D0A1A0A»\n";
        let bytes = parse_osascript_data(out, "PNGf").unwrap();
        assert_eq!(&bytes[..4], b"\x89PNG");
        assert_eq!(bytes.len(), 8);
    }

    #[test]
    fn rejects_malformed_data_literals() {
        assert!(parse_osascript_data("89504E47", "PNGf").is_none()); // no guillemets
        assert!(parse_osascript_data("«data TIFF89504E47»", "PNGf").is_none()); // wrong class
        assert!(parse_osascript_data("«data PNGf89504E4»", "PNGf").is_none()); // odd length
        assert!(parse_osascript_data("«data PNGfZZZZ»", "PNGf").is_none()); // not hex
    }

    #[test]
    fn parses_lsappinfo_name_line() {
        // Observed key on macOS 26 is LSDisplayName; the parser is key-agnostic.
        assert_eq!(
            parse_quoted_value("\"LSDisplayName\"=\"Safari\"\n").as_deref(),
            Some("Safari")
        );
        assert_eq!(
            parse_quoted_value("\"name\"=\"Google Chrome\"").as_deref(),
            Some("Google Chrome")
        );
        assert!(parse_quoted_value("garbage").is_none());
        // No frontmost GUI app (e.g. SSH session) yields an empty value.
        assert!(parse_quoted_value("\"LSDisplayName\"=\"\"").is_none());
    }

    #[test]
    fn browser_scripts_use_the_right_dialect() {
        let safari = browser_url_script("Safari").unwrap();
        assert_eq!(safari, "tell application \"Safari\" to get URL of front document");

        let chrome = browser_url_script("Google Chrome").unwrap();
        assert!(chrome.contains("URL of active tab of front window"));

        assert!(browser_url_script("Finder").is_none());
        assert!(browser_url_script("").is_none());
    }
}
