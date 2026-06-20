//! macOS window-capture vendor — enumerate windows via CGWindowList and grab one
//! via `/usr/sbin/screencapture`.
//!
//! `list_windows` reads the CoreGraphics on-screen window list (the only source
//! of the window *number* that `screencapture -l<id>` needs — AppleScript window
//! objects don't expose it). `grab_window_png` shells out exactly like
//! [`crate::vendors::macos_desktop_context`] does for full-screen capture.
//!
//! Both need the **Screen Recording** grant for real pixels/titles; without it
//! the grab returns a desktop-only image and window titles come back empty. Only
//! compiled on macOS.

use anyhow::Context;
use bytes::Bytes;
use core_foundation::base::TCFType;
use core_foundation::dictionary::{CFDictionary, CFDictionaryRef};
use core_foundation::number::CFNumber;
use core_foundation::string::{CFString, CFStringRef};
use core_graphics::window::{
    copy_window_info, kCGNullWindowID, kCGWindowListExcludeDesktopElements,
    kCGWindowListOptionOnScreenOnly, kCGWindowName, kCGWindowNumber, kCGWindowOwnerName,
};
use tokio::process::Command;

use crate::capabilities::screencast::WindowRef;

/// Enumerate on-screen windows (excluding desktop elements). Runs the blocking
/// CoreGraphics call on a blocking thread.
pub async fn list_windows() -> anyhow::Result<Vec<WindowRef>> {
    tokio::task::spawn_blocking(list_windows_blocking)
        .await
        .context("window list task panicked")?
}

fn list_windows_blocking() -> anyhow::Result<Vec<WindowRef>> {
    let option = kCGWindowListOptionOnScreenOnly | kCGWindowListExcludeDesktopElements;
    let infos = copy_window_info(option, kCGNullWindowID)
        .ok_or_else(|| anyhow::anyhow!("CGWindowListCopyWindowInfo returned null"))?;

    let mut out = Vec::new();
    for item in infos.iter() {
        // Each array item is a CFDictionaryRef describing one window.
        let dict: CFDictionary<CFString, core_foundation::base::CFType> =
            unsafe { CFDictionary::wrap_under_get_rule(*item as CFDictionaryRef) };
        let Some(id) = dict_u32(&dict, unsafe { kCGWindowNumber }) else {
            continue;
        };
        out.push(WindowRef {
            id,
            app: dict_string(&dict, unsafe { kCGWindowOwnerName }).unwrap_or_default(),
            title: dict_string(&dict, unsafe { kCGWindowName }).unwrap_or_default(),
        });
    }
    Ok(out)
}

/// Read a CFString value out of a window-info dictionary by its CFString key.
fn dict_string(
    dict: &CFDictionary<CFString, core_foundation::base::CFType>,
    key: CFStringRef,
) -> Option<String> {
    let key = unsafe { CFString::wrap_under_get_rule(key) };
    dict.find(&key)?.downcast::<CFString>().map(|s| s.to_string())
}

/// Read an integer value out of a window-info dictionary by its CFString key.
fn dict_u32(
    dict: &CFDictionary<CFString, core_foundation::base::CFType>,
    key: CFStringRef,
) -> Option<u32> {
    let key = unsafe { CFString::wrap_under_get_rule(key) };
    let n = dict.find(&key)?.downcast::<CFNumber>()?.to_i64()?;
    u32::try_from(n).ok()
}

/// Grab one window as PNG. `-o` drops the window shadow (clean edges), `-x`
/// silences the shutter, `-l<id>` targets the window number.
pub async fn grab_window_png(window_id: u32) -> anyhow::Result<Bytes> {
    let path = std::env::temp_dir().join(format!("hi-agent-win-{}.png", uuid::Uuid::now_v7()));
    let path_str = path.to_string_lossy().into_owned();
    let status = Command::new("/usr/sbin/screencapture")
        .args(grab_args(window_id, &path_str))
        .status()
        .await
        .context("spawning screencapture")?;
    anyhow::ensure!(status.success(), "screencapture exited {status}");
    let bytes = tokio::fs::read(&path).await.context("reading screencapture output")?;
    let _ = tokio::fs::remove_file(&path).await;
    Ok(Bytes::from(bytes))
}

/// The `screencapture` argv for a single-window PNG grab. Pure so the flag
/// spelling is unit-testable without a GUI session.
fn grab_args(window_id: u32, path: &str) -> Vec<String> {
    vec![
        "-x".into(),
        "-o".into(),
        "-t".into(),
        "png".into(),
        format!("-l{window_id}"),
        path.into(),
    ]
}

/// Grab the whole screen as PNG. With no `-l<id>`, `screencapture` captures the
/// main display fullscreen; `-x` silences the shutter sound.
pub async fn grab_screen_png() -> anyhow::Result<Bytes> {
    let path = std::env::temp_dir().join(format!("hi-agent-screen-{}.png", uuid::Uuid::now_v7()));
    let path_str = path.to_string_lossy().into_owned();
    let status = Command::new("/usr/sbin/screencapture")
        .args(grab_screen_args(&path_str))
        .status()
        .await
        .context("spawning screencapture")?;
    anyhow::ensure!(status.success(), "screencapture exited {status}");
    let bytes = tokio::fs::read(&path).await.context("reading screencapture output")?;
    let _ = tokio::fs::remove_file(&path).await;
    Ok(Bytes::from(bytes))
}

/// The `screencapture` argv for a whole-screen PNG grab — like [`grab_args`] but
/// with no window target. Pure so the flags are unit-testable without a GUI.
fn grab_screen_args(path: &str) -> Vec<String> {
    vec!["-x".into(), "-t".into(), "png".into(), path.into()]
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn grab_args_target_the_window_and_path() {
        let args = grab_args(42, "/tmp/w.png");
        assert!(args.contains(&"-l42".to_string()), "window id glued to -l");
        assert!(args.contains(&"png".to_string()));
        assert_eq!(args.last().unwrap(), "/tmp/w.png");
    }

    #[test]
    fn grab_screen_args_have_no_window_target() {
        let args = grab_screen_args("/tmp/s.png");
        assert!(!args.iter().any(|a| a.starts_with("-l")), "whole-screen grab has no -l");
        assert!(args.contains(&"png".to_string()));
        assert_eq!(args.last().unwrap(), "/tmp/s.png");
    }
}
