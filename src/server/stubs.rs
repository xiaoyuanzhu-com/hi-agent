//! 501 stubs for channels not implemented in v0.

use axum::http::StatusCode;
use axum::response::IntoResponse;

pub async fn post_vision() -> impl IntoResponse {
    (
        StatusCode::NOT_IMPLEMENTED,
        "vision channel is not implemented in v0\n",
    )
}

pub async fn post_touch() -> impl IntoResponse {
    (
        StatusCode::NOT_IMPLEMENTED,
        "touch channel is not implemented in v0\n",
    )
}

pub async fn post_smell() -> impl IntoResponse {
    (
        StatusCode::NOT_IMPLEMENTED,
        "smell channel is not implemented in v0\n",
    )
}

pub async fn post_taste() -> impl IntoResponse {
    (
        StatusCode::NOT_IMPLEMENTED,
        "taste channel is not implemented in v0\n",
    )
}
