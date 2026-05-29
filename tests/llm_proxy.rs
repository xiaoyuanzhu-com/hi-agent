//! Proxy forwards to a mock upstream, injecting the real key, streaming body back.

use std::net::SocketAddr;

use axum::routing::post;
use axum::{Router, extract::State};
use hi_agent::llm_proxy::LlmProxy;

/// Minimal mock upstream: asserts the injected key, echoes a canned SSE body.
async fn spawn_mock_upstream() -> (SocketAddr, tokio::task::JoinHandle<()>) {
    let app = Router::new().route(
        "/v1/messages",
        post(|State(()): State<()>, headers: axum::http::HeaderMap, body: String| async move {
            assert_eq!(headers.get("x-api-key").unwrap(), "REAL-KEY");
            assert!(body.contains("hello"));
            axum::response::Response::builder()
                .status(200)
                .header("content-type", "text/event-stream")
                .body(axum::body::Body::from("data: {\"type\":\"ok\"}\n\n"))
                .unwrap()
        }),
    ).with_state(());
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    let handle = tokio::spawn(async move {
        axum::serve(listener, app).await.unwrap();
    });
    (addr, handle)
}

#[tokio::test]
async fn forwards_injects_key_and_streams_body() {
    let (upstream_addr, _up) = spawn_mock_upstream().await;
    let upstream = format!("http://{upstream_addr}");

    let proxy = LlmProxy::start(upstream, "REAL-KEY".to_string()).await.unwrap();
    let port = proxy.port();

    let client = reqwest::Client::new();
    let resp = client
        .post(format!("http://127.0.0.1:{port}/v1/messages"))
        .header("x-api-key", "hi-agent-proxy") // placeholder; proxy overwrites
        .header("content-type", "application/json")
        .body(r#"{"messages":[{"role":"user","content":"hello"}]}"#)
        .send()
        .await
        .unwrap();

    assert_eq!(resp.status(), 200);
    let text = resp.text().await.unwrap();
    assert!(text.contains("\"type\":\"ok\""));
}
