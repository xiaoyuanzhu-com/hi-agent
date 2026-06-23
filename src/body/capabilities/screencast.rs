//! Window-capture capability — find an application's on-screen windows and grab
//! one as an image, on the machine this process runs on. The outbound visual twin
//! of the inbound camera ([`crate::server::vision`]): where that ingests a camera
//! the user points at the agent, this captures the screen the agent is driving —
//! the building block for casting an app window into a view, and a frame source
//! for a look→act loop.
//!
//! Like [`super::desktop_context`], the "vendor" is the operating system, selected
//! at compile time (`cfg(target_os)`) — no `init_from_env`, nothing to configure.
//! A still frame is the irreducible primitive; a smooth live stream (continuous
//! encode + transport) lands with the cast-to-view wiring and a capture vendor
//! built for video (ScreenCaptureKit).
//!
//! Capturing pixels needs the **Screen Recording** grant; without it the grab
//! returns a desktop-only image (the target window's pixels come back blank).
//!
//! **No caller wires this in yet.** The future caller is a `screen_out` broadcast
//! + `GET /api/out/screen` endpoint feeding a `<video>` cast view; additive later.

use bytes::Bytes;

/// One on-screen window: the OS window id to capture, plus the owning app and the
/// window's title for picking the right one.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct WindowRef {
    /// The CoreGraphics window number, passed to the capture as `-l<id>`.
    pub id: u32,
    /// The owning application's name, e.g. `QQ音乐`.
    pub app: String,
    /// The window's title, e.g. `蜗牛与黄鹂鸟 - 周杰伦`. May be empty.
    pub title: String,
}

/// Keep the windows whose owning app matches `app` (case-insensitive substring),
/// or all of them when `app` is `None`. Pure so it's unit-testable off-macOS; the
/// vendor enumerates, this selects.
pub fn filter_by_app(windows: Vec<WindowRef>, app: Option<&str>) -> Vec<WindowRef> {
    match app {
        None => windows,
        Some(query) => {
            let needle = query.to_lowercase();
            windows
                .into_iter()
                .filter(|w| w.app.to_lowercase().contains(&needle))
                .collect()
        }
    }
}

/// Whether this build can capture windows on the current platform. Compile-time,
/// not a permission check — a macOS build still needs the Screen Recording grant
/// for the captured pixels to be real.
pub fn available() -> bool {
    cfg!(target_os = "macos")
}

/// List on-screen windows, optionally filtered to one app (e.g. `Some("QQ音乐")`).
pub async fn list_windows(app: Option<&str>) -> anyhow::Result<Vec<WindowRef>> {
    #[cfg(target_os = "macos")]
    {
        let all = crate::vendors::macos_screencast::list_windows().await?;
        Ok(filter_by_app(all, app))
    }
    #[cfg(not(target_os = "macos"))]
    {
        let _ = app;
        anyhow::bail!("window capture is not supported on this platform")
    }
}

/// Grab a single window (by its [`WindowRef::id`]) as PNG bytes.
pub async fn grab_window_png(window_id: u32) -> anyhow::Result<Bytes> {
    #[cfg(target_os = "macos")]
    {
        crate::vendors::macos_screencast::grab_window_png(window_id).await
    }
    #[cfg(not(target_os = "macos"))]
    {
        let _ = window_id;
        anyhow::bail!("window capture is not supported on this platform")
    }
}

/// Grab the **whole current screen** (the main display) as PNG bytes — the user's
/// working context, not one window. The frame behind the "come and see this"
/// gesture (see [`crate::body::gesture`]): the user double-taps Command and is handed
/// this back as a file.
pub async fn grab_screen_png() -> anyhow::Result<Bytes> {
    #[cfg(target_os = "macos")]
    {
        crate::vendors::macos_screencast::grab_screen_png().await
    }
    #[cfg(not(target_os = "macos"))]
    {
        anyhow::bail!("screen capture is not supported on this platform")
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn win(id: u32, app: &str, title: &str) -> WindowRef {
        WindowRef { id, app: app.to_string(), title: title.to_string() }
    }

    #[test]
    fn filter_none_keeps_everything() {
        let all = vec![win(1, "Safari", "a"), win(2, "QQ音乐", "b")];
        assert_eq!(filter_by_app(all.clone(), None), all);
    }

    #[test]
    fn filter_matches_app_case_insensitively() {
        let all = vec![win(1, "Safari", "a"), win(2, "QQ音乐", "蜗牛"), win(3, "Google Chrome", "c")];
        let got = filter_by_app(all, Some("qq音乐"));
        assert_eq!(got, vec![win(2, "QQ音乐", "蜗牛")]);
    }

    #[test]
    fn filter_substring_matches_partial_app_name() {
        let all = vec![win(1, "Google Chrome", "a"), win(2, "Safari", "b")];
        let got = filter_by_app(all, Some("chrome"));
        assert_eq!(got, vec![win(1, "Google Chrome", "a")]);
    }

    #[test]
    fn available_matches_platform() {
        assert_eq!(available(), cfg!(target_os = "macos"));
    }
}
