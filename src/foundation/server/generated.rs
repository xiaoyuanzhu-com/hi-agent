//! `GET /views/<path>` — serve a file from the agent's view workshop on disk (where
//! `AppState.data_dir` is in scope, unlike the embed-only appearance router):
//! compiled view modules ([`crate::mind::views::ViewCompiler`] writes them under
//! `_compiled/`), images a build sub-agent downloaded, and anything else it
//! authored. Single-user and trusted, so served whole, guarded only against `..`.

use std::sync::Arc;

use axum::body::Body;
use axum::extract::{Path, State};
use axum::http::header::{CACHE_CONTROL, CONTENT_TYPE};
use axum::http::{HeaderValue, StatusCode};
use axum::response::{IntoResponse, Response};

use crate::foundation::server::AppState;

/// The only traversal guard for the views tree: reject any empty or `..` segment, so
/// the joined path can't climb out of the views root. Dotfiles are allowed (harmless;
/// compiled modules live under the non-dotfile `_compiled/`).
fn safe_views_path(path: &str) -> bool {
    !path.is_empty() && path.split('/').all(|seg| !seg.is_empty() && seg != "..")
}

/// Best-effort `Content-Type` by extension for view-workshop files.
fn views_content_type(path: &str) -> &'static str {
    match path.rsplit('.').next().unwrap_or("") {
        "mjs" | "js" => "application/javascript; charset=utf-8",
        "json" => "application/json; charset=utf-8",
        "css" => "text/css; charset=utf-8",
        "png" => "image/png",
        "jpg" | "jpeg" => "image/jpeg",
        "gif" => "image/gif",
        "webp" => "image/webp",
        "svg" => "image/svg+xml",
        _ => "text/plain; charset=utf-8",
    }
}

/// `GET /views/<path>` — serve a file from the agent's view workshop: a compiled view
/// module from `_compiled/`, an image, or any artifact a build sub-agent wrote.
/// The views tree is single-user and trusted, so it's served whole; the only guard is
/// against `..` traversal out of the root.
pub async fn views_file(
    State(state): State<Arc<AppState>>,
    Path(path): Path<String>,
) -> Response {
    if !safe_views_path(&path) {
        return (StatusCode::NOT_FOUND, "not found\n").into_response();
    }

    let full = state.data_dir.join("views").join(&path);
    let bytes = match tokio::fs::read(&full).await {
        Ok(bytes) => bytes,
        Err(_) => return (StatusCode::NOT_FOUND, "not found\n").into_response(),
    };

    let mut resp = Response::new(Body::from(bytes));
    resp.headers_mut()
        .insert(CONTENT_TYPE, HeaderValue::from_static(views_content_type(&path)));
    // Compiled modules under _compiled/ are content-addressed → immutable; source files
    // change in place, so they must not be cached.
    let cache = if path.starts_with("_compiled/") {
        "public, max-age=31536000, immutable"
    } else {
        "no-store"
    };
    resp.headers_mut().insert(CACHE_CONTROL, HeaderValue::from_static(cache));
    resp
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn views_path_blocks_traversal() {
        assert!(safe_views_path("_compiled/0a1b.mjs"));
        assert!(safe_views_path("badminton-top10/leader.jsx"));
        assert!(!safe_views_path("../secret"), "no parent traversal");
        assert!(!safe_views_path("a/../b"), "no mid-path traversal");
        assert!(!safe_views_path("a//b"), "no empty segment");
        assert!(!safe_views_path(""), "empty");
    }

    #[test]
    fn views_content_types() {
        assert_eq!(views_content_type("x.mjs"), "application/javascript; charset=utf-8");
        assert_eq!(views_content_type("a/b/photo.png"), "image/png");
        assert_eq!(views_content_type("v.jsx"), "text/plain; charset=utf-8");
    }
}
