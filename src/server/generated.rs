//! GET /generated/views/<hash>.mjs — serve a compiled agent view module.
//!
//! These are the ESM modules [`crate::views::ViewCompiler`] writes under
//! `data_dir/generated/views/`. Unlike the embedded `/assets/*` bundles, they
//! are *runtime* artifacts on disk, so they live on the server (where
//! `AppState.data_dir` is in scope) rather than in the embed-only appearance
//! router. Content-addressed names make them immutable and safe to cache hard.

use std::sync::Arc;

use axum::body::Body;
use axum::extract::{Path, State};
use axum::http::header::{CACHE_CONTROL, CONTENT_TYPE};
use axum::http::{HeaderValue, StatusCode};
use axum::response::{IntoResponse, Response};

use crate::server::AppState;

/// A compiled view filename is `<lowercase-hex>.mjs` and nothing else. This is
/// the path-traversal guard: hex + a fixed suffix can encode no `/` or `..`, so
/// the name can never escape the views directory.
fn is_valid_module_name(name: &str) -> bool {
    match name.strip_suffix(".mjs") {
        Some(stem) => !stem.is_empty() && stem.bytes().all(|b| matches!(b, b'0'..=b'9' | b'a'..=b'f')),
        None => false,
    }
}

pub async fn generated_view(
    State(state): State<Arc<AppState>>,
    Path(file): Path<String>,
) -> Response {
    if !is_valid_module_name(&file) {
        return (StatusCode::NOT_FOUND, "not found\n").into_response();
    }

    let path = state.data_dir.join("generated").join("views").join(&file);
    let bytes = match tokio::fs::read(&path).await {
        Ok(bytes) => bytes,
        Err(_) => return (StatusCode::NOT_FOUND, "not found\n").into_response(),
    };

    let mut resp = Response::new(Body::from(bytes));
    resp.headers_mut().insert(
        CONTENT_TYPE,
        HeaderValue::from_static("application/javascript; charset=utf-8"),
    );
    // Content-addressed by source hash → immutable.
    resp.headers_mut().insert(
        CACHE_CONTROL,
        HeaderValue::from_static("public, max-age=31536000, immutable"),
    );
    resp
}

/// A hosted asset filename is `<lowercase-hex>.<ext>` for a known image type —
/// the same traversal guard as views (hex + a fixed suffix can encode no `/` or
/// `..`). Returns the `Content-Type` to serve it with, or `None` to 404.
fn asset_content_type(name: &str) -> Option<&'static str> {
    let (stem, ext) = name.rsplit_once('.')?;
    if stem.is_empty() || !stem.bytes().all(|b| matches!(b, b'0'..=b'9' | b'a'..=b'f')) {
        return None;
    }
    match ext {
        "png" => Some("image/png"),
        "jpg" | "jpeg" => Some("image/jpeg"),
        "gif" => Some("image/gif"),
        "webp" => Some("image/webp"),
        "svg" => Some("image/svg+xml"),
        _ => None,
    }
}

/// `GET /generated/assets/<hash>.<ext>` — serve an image [`crate::mcp`] downloaded
/// and stored under `data_dir/generated/assets/` for an agent view to `<img>`.
/// Content-addressed names make them immutable and safe to cache hard.
pub async fn generated_asset(
    State(state): State<Arc<AppState>>,
    Path(file): Path<String>,
) -> Response {
    let Some(content_type) = asset_content_type(&file) else {
        return (StatusCode::NOT_FOUND, "not found\n").into_response();
    };

    let path = state.data_dir.join("generated").join("assets").join(&file);
    let bytes = match tokio::fs::read(&path).await {
        Ok(bytes) => bytes,
        Err(_) => return (StatusCode::NOT_FOUND, "not found\n").into_response(),
    };

    let mut resp = Response::new(Body::from(bytes));
    resp.headers_mut()
        .insert(CONTENT_TYPE, HeaderValue::from_static(content_type));
    resp.headers_mut().insert(
        CACHE_CONTROL,
        HeaderValue::from_static("public, max-age=31536000, immutable"),
    );
    resp
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn asset_names_validated_and_typed() {
        assert_eq!(asset_content_type("0a1b2c.png"), Some("image/png"));
        assert_eq!(asset_content_type("deadbeef.jpg"), Some("image/jpeg"));
        assert_eq!(asset_content_type("ff.webp"), Some("image/webp"));
        assert_eq!(asset_content_type("../secret.png"), None, "no traversal");
        assert_eq!(asset_content_type("a/b.png"), None, "no separators");
        assert_eq!(asset_content_type("abc.exe"), None, "unknown ext");
        assert_eq!(asset_content_type("DEADBEEF.png"), None, "uppercase not produced by us");
        assert_eq!(asset_content_type(".png"), None, "empty stem");
    }

    #[test]
    fn rejects_traversal_and_non_module_names() {
        assert!(is_valid_module_name("0a1b2c3d4e5f6789.mjs"));
        assert!(!is_valid_module_name("../secret.mjs"), "no parent traversal");
        assert!(!is_valid_module_name("a/b.mjs"), "no path separators");
        assert!(!is_valid_module_name("abc.js"), "wrong suffix");
        assert!(!is_valid_module_name(".mjs"), "empty stem");
        assert!(!is_valid_module_name("DEADBEEF.mjs"), "uppercase not produced by us");
    }
}
