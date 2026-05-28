//! Compile-time embed of the built SPA.
//!
//! The folder path is relative to the crate root. During development the
//! folder may be empty (just `.keep`), in which case `WebAssets::get` returns
//! `None` for everything and our handlers fall through to the dev placeholder.
//! In release builds, `pnpm build` populates `dist/` before `cargo build`.

use rust_embed::RustEmbed;

#[derive(RustEmbed)]
#[folder = "src/appearance/web/dist/"]
pub struct WebAssets;

/// Convenience wrapper around `WebAssets::get` so callers don't import the
/// `RustEmbed` trait themselves.
pub fn get(path: &str) -> Option<rust_embed::EmbeddedFile> {
    WebAssets::get(path)
}

/// Best-effort MIME guess from a path's extension. Kept small to avoid
/// pulling in an extra crate; covers the formats Vite produces.
pub fn content_type_for(path: &str) -> &'static str {
    let ext = path.rsplit('.').next().unwrap_or("").to_ascii_lowercase();
    match ext.as_str() {
        "html" | "htm" => "text/html; charset=utf-8",
        "js" | "mjs" => "application/javascript; charset=utf-8",
        "css" => "text/css; charset=utf-8",
        "json" => "application/json; charset=utf-8",
        "map" => "application/json; charset=utf-8",
        "svg" => "image/svg+xml",
        "png" => "image/png",
        "jpg" | "jpeg" => "image/jpeg",
        "gif" => "image/gif",
        "webp" => "image/webp",
        "avif" => "image/avif",
        "ico" => "image/x-icon",
        "woff" => "font/woff",
        "woff2" => "font/woff2",
        "ttf" => "font/ttf",
        "otf" => "font/otf",
        "txt" => "text/plain; charset=utf-8",
        "wasm" => "application/wasm",
        _ => "application/octet-stream",
    }
}
