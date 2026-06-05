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
                dispatch_tool(registry, scene.as_ref(), worker_id, name, &args).await,
            ))
        }
        // ping is a no-op request the client may send.
        "ping" => McpReply::Json(result(id, json!({}))),
        other => McpReply::Json(error(id, -32601, &format!("method not found: {other}"))),
    }
}

/// Run one tool call, returning the MCP `tools/call` result shape (a content list
/// with an `isError` flag). Tools are fire-and-forget: we forward the command to
/// the scene loop and ack immediately, never blocking on playback or on the
/// worker the delegate spawns.
async fn dispatch_tool(
    registry: &ToolRegistry,
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

    let arg_str = |key: &str| args.get(key).and_then(Value::as_str).unwrap_or_default().to_string();

    let (control, ack) = match name {
        "delegate" => {
            let task = arg_str("task");
            if task.trim().is_empty() {
                return tool_error("delegate requires a non-empty `task`");
            }
            (SceneControl::Delegate { task }, "delegated to a working session")
        }
        "alarm" => {
            let delay = arg_str("delay");
            let note = arg_str("note");
            if delay.trim().is_empty() {
                return tool_error("alarm requires a `delay`");
            }
            (SceneControl::Alarm { delay, note }, "alarm scheduled")
        }
        "ask" => {
            let question = arg_str("question");
            let Some(id) = worker_id else {
                return tool_error("ask is only available to working sessions");
            };
            (SceneControl::WorkerAsk { id, question }, "question noted")
        }
        other => return tool_error(&format!("unknown tool: {other}")),
    };

    match sink.send(control).await {
        Ok(()) => tool_ok(ack),
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
