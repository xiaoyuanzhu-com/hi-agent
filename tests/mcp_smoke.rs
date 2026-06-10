//! Protocol smoke test for the `/mcp` tool endpoint.
//!
//! Builds the axum router via [`hi_agent::server::build`] and exercises the
//! hand-rolled MCP "Streamable HTTP" surface directly: the initialize handshake,
//! role-gated `tools/list`, the `202` for notifications, the `405` for the GET
//! SSE stream we decline, and a `tools/call` whose scene has no live loop.

use hi_agent::memory::Memory;
use hi_agent::server::{self, ServerSeams};
use serde_json::{Value, json};
use tempfile::tempdir;
use tokio::net::TcpListener;

async fn spawn_server() -> (String, tempfile::TempDir, ServerSeams) {
    let dir = tempdir().expect("tempdir");
    let memory = Memory::open(dir.path()).await.expect("memory");
    let observatory =
        hi_agent::observatory::Observatory::new(None, hi_agent::reactor::swap_budget_chars());
    let (router, seams) = server::build(
        memory,
        dir.path().to_path_buf(),
        observatory,
        hi_agent::acp::AcpTap::new(),
        hi_agent::reactor::ToolRegistry::new(),
        hi_agent::reactor::InterruptRegistry::new(),
    );

    let listener = TcpListener::bind("127.0.0.1:0").await.expect("bind");
    let addr = listener.local_addr().expect("local_addr");
    tokio::spawn(async move {
        let _ = axum::serve(listener, router).await;
    });
    (format!("http://{addr}"), dir, seams)
}

async fn post_mcp(client: &reqwest::Client, base: &str, role: &str, msg: Value) -> reqwest::Response {
    client
        .post(format!("{base}/mcp"))
        .header("X-HI-Scene", "alice@phone")
        .header("X-HI-Role", role)
        .header("Content-Type", "application/json")
        .body(msg.to_string())
        .send()
        .await
        .expect("send POST /mcp")
}

fn tool_names(list: &Value) -> Vec<String> {
    list["result"]["tools"]
        .as_array()
        .expect("tools array")
        .iter()
        .map(|t| t["name"].as_str().unwrap_or_default().to_string())
        .collect()
}

#[tokio::test]
async fn initialize_returns_server_info() {
    let (base, _dir, _seams) = spawn_server().await;
    let client = reqwest::Client::new();
    let resp = post_mcp(
        &client,
        &base,
        "reactor",
        json!({ "jsonrpc": "2.0", "id": 1, "method": "initialize",
                "params": { "protocolVersion": "2025-06-18" } }),
    )
    .await;
    assert_eq!(resp.status(), reqwest::StatusCode::OK);
    let body: Value = resp.json().await.expect("json");
    assert_eq!(body["result"]["serverInfo"]["name"], "hi-agent");
    assert_eq!(body["result"]["protocolVersion"], "2025-06-18");
    assert!(body["result"]["capabilities"]["tools"].is_object());
}

#[tokio::test]
async fn tools_list_is_role_gated() {
    let (base, _dir, _seams) = spawn_server().await;
    let client = reqwest::Client::new();

    let reactor = post_mcp(
        &client,
        &base,
        "reactor",
        json!({ "jsonrpc": "2.0", "id": 2, "method": "tools/list" }),
    )
    .await
    .json::<Value>()
    .await
    .expect("json");
    let names = tool_names(&reactor);
    assert!(names.contains(&"delegate".to_string()), "got {names:?}");
    assert!(names.contains(&"alarm".to_string()), "got {names:?}");
    assert!(!names.contains(&"ask".to_string()), "reactor must not see ask");

    let worker = post_mcp(
        &client,
        &base,
        "worker",
        json!({ "jsonrpc": "2.0", "id": 3, "method": "tools/list" }),
    )
    .await
    .json::<Value>()
    .await
    .expect("json");
    let names = tool_names(&worker);
    assert_eq!(names, vec!["ask".to_string()]);
}

#[tokio::test]
async fn notification_is_accepted_without_body() {
    let (base, _dir, _seams) = spawn_server().await;
    let client = reqwest::Client::new();
    // No `id` ⇒ a notification ⇒ 202 Accepted, empty body.
    let resp = post_mcp(
        &client,
        &base,
        "reactor",
        json!({ "jsonrpc": "2.0", "method": "notifications/initialized" }),
    )
    .await;
    assert_eq!(resp.status(), reqwest::StatusCode::ACCEPTED);
    assert!(resp.bytes().await.expect("body").is_empty());
}

#[tokio::test]
async fn get_declines_sse_stream() {
    let (base, _dir, _seams) = spawn_server().await;
    let resp = reqwest::Client::new()
        .get(format!("{base}/mcp"))
        .send()
        .await
        .expect("GET /mcp");
    assert_eq!(resp.status(), reqwest::StatusCode::METHOD_NOT_ALLOWED);
}

#[tokio::test]
async fn tool_call_for_unknown_scene_is_a_tool_error() {
    // No reactor loop is registered (server::build doesn't start one), so a
    // delegate call resolves to a tool error rather than a transport failure —
    // the JSON-RPC envelope still succeeds.
    let (base, _dir, _seams) = spawn_server().await;
    let client = reqwest::Client::new();
    let body: Value = post_mcp(
        &client,
        &base,
        "reactor",
        json!({ "jsonrpc": "2.0", "id": 4, "method": "tools/call",
                "params": { "name": "delegate", "arguments": { "task": "look something up" } } }),
    )
    .await
    .json()
    .await
    .expect("json");
    assert_eq!(body["result"]["isError"], true);
}
