//! Local Anthropic-compatible reverse proxy. Injects the real upstream key so
//! it never lands in any on-disk claude/adapter config.

use std::sync::Arc;

use anyhow::Context;
use axum::body::Body;
use axum::extract::State;
use axum::http::{HeaderName, StatusCode};
use axum::response::Response;
use axum::routing::any;
use axum::Router;

/// Headers we never forward upstream (hop-by-hop or auth we replace).
const STRIP_REQUEST_HEADERS: &[&str] = &["host", "authorization", "x-api-key", "content-length"];

#[derive(Clone)]
struct ProxyState {
    client: reqwest::Client,
    upstream_base_url: Arc<str>,
    upstream_key: Arc<str>,
}

/// A running local proxy. Drop to stop serving.
pub struct LlmProxy {
    port: u16,
    _server: tokio::task::JoinHandle<()>,
}

impl LlmProxy {
    /// Bind on 127.0.0.1:0 and start serving. `upstream_base_url` is the origin
    /// (no trailing `/v1/...`); `upstream_key` is the real credential.
    pub async fn start(upstream_base_url: String, upstream_key: String) -> anyhow::Result<Self> {
        let state = ProxyState {
            client: reqwest::Client::new(),
            upstream_base_url: Arc::from(upstream_base_url.trim_end_matches('/')),
            upstream_key: Arc::from(upstream_key),
        };
        let app = Router::new()
            .route("/{*path}", any(forward))
            .with_state(state);

        let listener = tokio::net::TcpListener::bind("127.0.0.1:0")
            .await
            .context("binding llm proxy")?;
        let port = listener.local_addr()?.port();
        let server = tokio::spawn(async move {
            if let Err(e) = axum::serve(listener, app).await {
                tracing::error!(error = %e, "llm proxy server exited");
            }
        });
        tracing::info!(port, "llm proxy listening");
        Ok(Self { port, _server: server })
    }

    pub fn port(&self) -> u16 {
        self.port
    }
}

/// Forward any request to the upstream, replacing auth with the real key and
/// streaming the response body straight back.
async fn forward(
    State(state): State<ProxyState>,
    req: axum::extract::Request,
) -> Result<Response, StatusCode> {
    let method = req.method().clone();
    let path_q = req
        .uri()
        .path_and_query()
        .map(|pq| pq.as_str().to_string())
        .unwrap_or_else(|| "/".to_string());
    let in_headers = req.headers().clone();

    let body_bytes = axum::body::to_bytes(req.into_body(), usize::MAX)
        .await
        .map_err(|_| StatusCode::BAD_REQUEST)?;

    let url = format!("{}{}", state.upstream_base_url, path_q);
    let mut out = state.client.request(method, &url).body(body_bytes);

    for (name, value) in in_headers.iter() {
        if STRIP_REQUEST_HEADERS.contains(&name.as_str()) {
            continue;
        }
        out = out.header(name, value);
    }
    out = out.header("x-api-key", state.upstream_key.as_ref());

    let upstream_resp = out.send().await.map_err(|e| {
        tracing::warn!(error = %e, "upstream request failed");
        StatusCode::BAD_GATEWAY
    })?;

    let status = upstream_resp.status();
    tracing::debug!(status = %status, path = %path_q, "proxy forwarded to upstream");
    let mut builder = Response::builder().status(status);
    for (name, value) in upstream_resp.headers().iter() {
        // content-length will not match a streamed body; let axum recompute.
        if name.as_str() == "content-length" || name.as_str() == "transfer-encoding" {
            continue;
        }
        if let Ok(hn) = HeaderName::from_bytes(name.as_str().as_bytes()) {
            builder = builder.header(hn, value);
        }
    }
    let stream = upstream_resp.bytes_stream();
    builder
        .body(Body::from_stream(stream))
        .map_err(|_| StatusCode::INTERNAL_SERVER_ERROR)
}