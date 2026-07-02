//! Vendor layer — the concrete implementations that call third-party APIs.
//!
//! Each module here integrates one vendor for one capability: it owns the
//! wire protocol (HTTP/websocket, auth, request/response structs) and exposes
//! **stateless free functions** that take their config as an explicit `&Config`
//! parameter, so they stay pure and unit-testable without touching any global.
//! The matching `crate::body::capabilities` module selects and dispatches to them.

pub mod campplus;
pub mod doubao_image_gen;
pub mod doubao_video_gen;
pub mod doubao_vision;
pub mod ffmpeg;
pub mod ffmpeg_frame;
pub mod insightface_face;
#[cfg(target_os = "macos")]
pub mod macos_accessibility;
#[cfg(target_os = "macos")]
pub mod macos_account;
#[cfg(target_os = "macos")]
pub mod macos_audio_capture;
#[cfg(target_os = "macos")]
pub mod macos_desktop_context;
#[cfg(target_os = "macos")]
pub mod macos_hotkey;
#[cfg(target_os = "macos")]
pub mod macos_input;
#[cfg(target_os = "macos")]
pub mod macos_popover;
#[cfg(target_os = "macos")]
pub mod macos_screencast;
#[cfg(target_os = "macos")]
pub mod macos_tray;
#[cfg(target_os = "macos")]
pub mod macos_window;
pub mod volcengine_stt;
pub mod volcengine_tts;

/// Rebase `endpoint`'s host (and port) onto `base`'s, keeping the endpoint's
/// scheme and path. songguo is a transparent proxy fronting these vendors at
/// their native paths, so routing a call through it is just a host swap — the
/// wire (path, body, headers) is identical. Returns `endpoint` unchanged if
/// either URL won't parse.
pub(crate) fn rebase_host(endpoint: &str, base: &str) -> String {
    match (reqwest::Url::parse(endpoint), reqwest::Url::parse(base)) {
        (Ok(mut ep), Ok(b)) => {
            let _ = ep.set_host(b.host_str());
            let _ = ep.set_port(b.port());
            ep.to_string()
        }
        _ => endpoint.to_string(),
    }
}

#[cfg(test)]
mod tests {
    #[test]
    fn rebase_host_swaps_host_keeps_scheme_and_path() {
        // Doubao HTTP: host swapped, /api/plan/v3 path kept.
        assert_eq!(
            super::rebase_host(
                "https://ark.cn-beijing.volces.com/api/plan/v3",
                "https://songguo.xiaoyuanzhu.com"
            ),
            "https://songguo.xiaoyuanzhu.com/api/plan/v3"
        );
        // Speech wss: scheme + native path kept, only host swapped.
        assert_eq!(
            super::rebase_host(
                "wss://openspeech.bytedance.com/api/v3/sauc/bigmodel_async",
                "https://songguo.xiaoyuanzhu.com"
            ),
            "wss://songguo.xiaoyuanzhu.com/api/v3/sauc/bigmodel_async"
        );
        // Unparseable endpoint → returned unchanged.
        assert_eq!(super::rebase_host("not a url", "https://x.example"), "not a url");
    }
}
