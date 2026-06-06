//! Minimal MCP server — the tool carrier between the mind and the reactor module.
//!
//! The reactor session (and its workers) reach this over the ACP `mcp_servers`
//! attachment as an HTTP MCP endpoint (`/mcp`). It speaks just enough of the MCP
//! "Streamable HTTP" transport to serve tools: a JSON-RPC *request* gets a single
//! `application/json` response, a *notification* gets `202 Accepted`, and the GET
//! SSE stream is declined (`405`) since we never push server-initiated messages.
//! No session ids — each ACP session opens its own MCP connection and identifies
//! its scene/role/worker on every call via headers, so the transport stays
//! stateless here.
//!
//! This module is transport-free: it turns a parsed JSON-RPC message plus the
//! routing identity (scene/role/worker id from headers) into an [`McpReply`]. The
//! HTTP glue lives in `crate::server::mcp`. Tool calls are forwarded to the right
//! scene loop through the [`ToolRegistry`]; see [`crate::reactor::tools`].

use serde_json::{Value, json};

use crate::reactor::{SceneControl, ToolRegistry};
use crate::types::Scene;

/// MCP protocol version we advertise when the client doesn't pin one. We echo the
/// client's requested version when present, so this is only the fallback.
const PROTOCOL_VERSION: &str = "2025-06-18";

/// What the HTTP layer should send back. `Json` is a JSON-RPC response body;
/// `Accepted` is the empty 202 for notifications/responses.
pub enum McpReply {
    Json(Value),
    Accepted,
}

/// The two tool surfaces, selected by the `X-HI-Role` header. A reactor session
/// drives output and delegation; a worker can only raise a question.
fn tools_for_role(role: Option<&str>) -> Vec<Value> {
    match role {
        Some("worker") => vec![tool(
            "ask",
            "Raise a non-blocking question for the agent about an ambiguity in your task. \
             You do NOT wait for an answer — note your best assumption and keep working; \
             the agent sees the question and may steer you next time it speaks.",
            json!({
                "type": "object",
                "properties": { "question": { "type": "string", "description": "The question to surface." } },
                "required": ["question"],
            }),
        )],
        // Default to the reactor surface (the soul describes these).
        _ => vec![
            tool(
                "say",
                "Speak to the person. Everything you want said aloud goes through this tool — \
                 plain text you write is NOT spoken. Call it with one natural chunk at a time; \
                 several calls in a turn are spoken in order. To stay silent, don't call it at all.",
                json!({
                    "type": "object",
                    "properties": { "text": { "type": "string", "description": "What to say, as natural spoken language (no markdown)." } },
                    "required": ["text"],
                }),
            ),
            tool(
                "show_view",
                "Put a view on the screen — a small React component you write as JSX. Interleave \
                 show_view and say calls in the order you want them experienced (say, then show, \
                 then say) so each view lands as you speak to it. Reuse an `id` with op=replace to \
                 evolve a view in place; op=dismiss takes one down.",
                json!({
                    "type": "object",
                    "properties": {
                        "op": { "type": "string", "enum": ["show", "replace", "dismiss"], "description": "show mounts; replace swaps the same id in place; dismiss removes it." },
                        "id": { "type": "string", "description": "A stable name for this view, so replace/dismiss can target it. Omit to auto-generate." },
                        "source": { "type": "string", "description": "The view's JSX (default-exported component). Omit for dismiss." },
                    },
                    "required": ["op"],
                }),
            ),
            tool(
                "add_asset",
                "Host an image so a view can show it. Pass the `url` of a real image you found \
                 (search the web with your own tools first); the server downloads it and returns a \
                 same-origin path like `/generated/assets/<hash>.png`. Put THAT path in an `<img \
                 src>` inside show_view — never hotlink the original URL (it can fail CORS, be \
                 hotlink-blocked, or 404). Use this whenever a view wants a real photo, poster, or \
                 picture; this is how you get one onto the screen.",
                json!({
                    "type": "object",
                    "properties": { "url": { "type": "string", "description": "Direct URL of the source image (jpg/png/webp/gif/svg)." } },
                    "required": ["url"],
                }),
            ),
            tool(
                "delegate",
                "Hand a heavy or long-running task (research, multi-step tool use, writing and \
                 running code) to a background working session, so you stay free to keep talking. \
                 It runs with your tools and memory but no voice; it reports back when done or if \
                 it gets stuck, and you'll see that as a new signal to fold into what you say next.",
                json!({
                    "type": "object",
                    "properties": { "task": { "type": "string", "description": "A self-contained description of the work, with everything the worker needs to start." } },
                    "required": ["task"],
                }),
            ),
            tool(
                "alarm",
                "Set yourself to come back to something after a delay — a reminder you promised, \
                 checking back if they've gone quiet, any time-based follow-up. When it fires you're \
                 woken with the note as a new signal even if nothing else happened; decide then.",
                json!({
                    "type": "object",
                    "properties": {
                        "delay": { "type": "string", "description": "How long to wait: seconds, or a number with an s/m/h suffix like 30s, 20m, 1h." },
                        "note": { "type": "string", "description": "A short note to your future self about what to revisit." },
                    },
                    "required": ["delay", "note"],
                }),
            ),
        ],
    }
}

fn tool(name: &str, description: &str, input_schema: Value) -> Value {
    json!({ "name": name, "description": description, "inputSchema": input_schema })
}

/// Handle one parsed JSON-RPC message. `scene`/`role`/`worker_id` come from the
/// request headers; `registry` routes tool calls to the owning scene loop.
pub async fn handle(
    registry: &ToolRegistry,
    data_dir: &std::path::Path,
    scene: Option<Scene>,
    role: Option<&str>,
    worker_id: Option<u64>,
    msg: &Value,
) -> McpReply {
    let method = msg.get("method").and_then(Value::as_str).unwrap_or_default();
    let id = msg.get("id").cloned();

    // No id ⇒ a notification (e.g. notifications/initialized) ⇒ just 202.
    let Some(id) = id else {
        return McpReply::Accepted;
    };

    match method {
        "initialize" => {
            let requested = msg
                .get("params")
                .and_then(|p| p.get("protocolVersion"))
                .and_then(Value::as_str)
                .unwrap_or(PROTOCOL_VERSION);
            McpReply::Json(result(
                id,
                json!({
                    "protocolVersion": requested,
                    "capabilities": { "tools": {} },
                    "serverInfo": { "name": "hi-agent", "version": env!("CARGO_PKG_VERSION") },
                }),
            ))
        }
        "tools/list" => McpReply::Json(result(id, json!({ "tools": tools_for_role(role) }))),
        "tools/call" => {
            let params = msg.get("params").cloned().unwrap_or_else(|| json!({}));
            let name = params.get("name").and_then(Value::as_str).unwrap_or_default();
            let args = params.get("arguments").cloned().unwrap_or_else(|| json!({}));
            McpReply::Json(result(
                id,
                dispatch_tool(registry, data_dir, scene.as_ref(), worker_id, name, &args).await,
            ))
        }
        // ping is a no-op request the client may send.
        "ping" => McpReply::Json(result(id, json!({}))),
        other => McpReply::Json(error(id, -32601, &format!("method not found: {other}"))),
    }
}

/// Run one tool call, returning the MCP `tools/call` result shape (a content list
/// with an `isError` flag). Tools are fire-and-forget: we forward the call to the
/// scene (its loop for side-effects, its sequencer for output) and ack
/// immediately, never blocking on playback or on the worker a delegate spawns.
async fn dispatch_tool(
    registry: &ToolRegistry,
    data_dir: &std::path::Path,
    scene: Option<&Scene>,
    worker_id: Option<u64>,
    name: &str,
    args: &Value,
) -> Value {
    let Some(scene) = scene else {
        return tool_error("missing X-HI-Scene header");
    };
    let Some(sink) = registry.get(scene).await else {
        return tool_error(&format!("no active scene loop for {}", scene.0));
    };

    let arg_str =
        |key: &str| args.get(key).and_then(Value::as_str).unwrap_or_default().to_string();
    let arg_opt = |key: &str| args.get(key).and_then(Value::as_str).map(str::to_owned);

    let outcome = match name {
        "say" => {
            let text = arg_str("text");
            if text.trim().is_empty() {
                return tool_error("say requires non-empty `text`");
            }
            sink.say(text).await.map(|()| "spoken")
        }
        "show_view" => {
            let op = args.get("op").and_then(Value::as_str).unwrap_or("show").to_string();
            sink.show_view(arg_opt("id"), op, arg_str("source")).await.map(|()| "shown")
        }
        "delegate" => {
            let task = arg_str("task");
            if task.trim().is_empty() {
                return tool_error("delegate requires a non-empty `task`");
            }
            sink.send(SceneControl::Delegate { task }).await.map(|()| "delegated to a working session")
        }
        "alarm" => {
            let delay = arg_str("delay");
            if delay.trim().is_empty() {
                return tool_error("alarm requires a `delay`");
            }
            sink.send(SceneControl::Alarm { delay, note: arg_str("note") }).await.map(|()| "alarm scheduled")
        }
        "ask" => {
            let Some(id) = worker_id else {
                return tool_error("ask is only available to working sessions");
            };
            sink.send(SceneControl::WorkerAsk { id, question: arg_str("question") }).await.map(|()| "question noted")
        }
        "add_asset" => {
            let url = arg_str("url");
            if url.trim().is_empty() {
                return tool_error("add_asset requires a non-empty `url`");
            }
            return match ingest_asset(data_dir, &url).await {
                Ok(asset_url) => tool_ok(&asset_url),
                Err(err) => tool_error(&err),
            };
        }
        other => return tool_error(&format!("unknown tool: {other}")),
    };

    match outcome {
        Ok(ack) => tool_ok(ack),
        Err(err) => tool_error(&err.to_string()),
    }
}

fn result(id: Value, result: Value) -> Value {
    json!({ "jsonrpc": "2.0", "id": id, "result": result })
}

fn error(id: Value, code: i64, message: &str) -> Value {
    json!({ "jsonrpc": "2.0", "id": id, "error": { "code": code, "message": message } })
}

fn tool_ok(text: &str) -> Value {
    json!({ "content": [{ "type": "text", "text": text }], "isError": false })
}

fn tool_error(text: &str) -> Value {
    json!({ "content": [{ "type": "text", "text": text }], "isError": true })
}

/// Download a remote image and store it content-addressed under
/// `data_dir/generated/assets/<hash>.<ext>`, returning the same-origin path the
/// page serves it from (`GET /generated/assets/...`). The fetch happens here,
/// server-side, so the view never hotlinks (no CORS, no broken box) and the agent
/// needn't know where `data_dir` lives on disk. The 64-bit name is a cache key —
/// identical bytes are written at most once — not a security boundary.
async fn ingest_asset(data_dir: &std::path::Path, url: &str) -> Result<String, String> {
    use std::hash::Hasher;

    let resp = reqwest::Client::new()
        .get(url)
        // A real UA: some hosts 403 the default reqwest agent for hotlink checks.
        .header(reqwest::header::USER_AGENT, "Mozilla/5.0 (hi-agent asset fetch)")
        .send()
        .await
        .map_err(|e| format!("fetch failed: {e}"))?;
    if !resp.status().is_success() {
        return Err(format!("fetch returned HTTP {}", resp.status()));
    }
    let content_type = resp
        .headers()
        .get(reqwest::header::CONTENT_TYPE)
        .and_then(|v| v.to_str().ok())
        .unwrap_or("")
        .to_owned();
    let ext = ext_for_content_type(&content_type)
        .or_else(|| ext_from_url(url))
        .ok_or_else(|| format!("not a supported image (content-type: {content_type:?})"))?;

    let bytes = resp.bytes().await.map_err(|e| format!("reading body failed: {e}"))?;
    if bytes.is_empty() {
        return Err("fetched image was empty".into());
    }

    let mut h = std::collections::hash_map::DefaultHasher::new();
    h.write(&bytes);
    let name = format!("{:016x}.{ext}", h.finish());

    let dir = data_dir.join("generated").join("assets");
    tokio::fs::create_dir_all(&dir)
        .await
        .map_err(|e| format!("creating assets dir: {e}"))?;
    let path = dir.join(&name);
    if tokio::fs::metadata(&path).await.is_err() {
        tokio::fs::write(&path, &bytes)
            .await
            .map_err(|e| format!("writing asset: {e}"))?;
    }
    Ok(format!("/generated/assets/{name}"))
}

/// Map an HTTP `Content-Type` (which may carry params) to our stored extension.
fn ext_for_content_type(content_type: &str) -> Option<&'static str> {
    match content_type.split(';').next().unwrap_or("").trim() {
        "image/png" => Some("png"),
        "image/jpeg" => Some("jpg"),
        "image/gif" => Some("gif"),
        "image/webp" => Some("webp"),
        "image/svg+xml" => Some("svg"),
        _ => None,
    }
}

/// Fallback when the server sends no usable `Content-Type`: sniff the URL path.
fn ext_from_url(url: &str) -> Option<&'static str> {
    let path = url.split(['?', '#']).next().unwrap_or(url).to_ascii_lowercase();
    if path.ends_with(".png") {
        Some("png")
    } else if path.ends_with(".jpg") || path.ends_with(".jpeg") {
        Some("jpg")
    } else if path.ends_with(".gif") {
        Some("gif")
    } else if path.ends_with(".webp") {
        Some("webp")
    } else if path.ends_with(".svg") {
        Some("svg")
    } else {
        None
    }
}
