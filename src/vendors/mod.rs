//! Vendor layer — the concrete implementations that call third-party APIs.
//!
//! Each module here integrates one vendor for one capability: it owns the
//! wire protocol (HTTP/websocket, auth, request/response structs) and exposes
//! **stateless free functions** that take their config as an explicit `&Config`
//! parameter, so they stay pure and unit-testable without touching any global.
//! The matching `crate::capabilities` module selects and dispatches to them.

pub mod doubao_image_gen;
pub mod doubao_video_gen;
pub mod doubao_vision;
#[cfg(target_os = "macos")]
pub mod macos_desktop_context;
pub mod volcengine_stt;
pub mod volcengine_tts;
