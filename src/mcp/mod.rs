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
        Some("worker") => vec![
            tool(
                "ask",
                "Raise a non-blocking question for the agent about an ambiguity in your task. \
                 You do NOT wait for an answer — note your best assumption and keep working; \
                 the agent sees the question and may steer you next time it speaks.",
                json!({
                    "type": "object",
                    "properties": { "question": { "type": "string", "description": "The question to surface." } },
                    "required": ["question"],
                }),
            ),
        ],
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
                "Put a view on the screen. Normally you show a view a builder made for you: \
                 delegate the build, then pass the `ref` it reported back (like `project/view`) here. \
                 Interleave show_view and say calls in the order you want them experienced (say, \
                 then show) so each view lands as you speak to it. Reuse an `id` with op=replace \
                 to evolve a view in place; op=dismiss takes one down. For a trivial one-off you \
                 may pass raw `source` JSX instead of a ref.",
                json!({
                    "type": "object",
                    "properties": {
                        "op": { "type": "string", "enum": ["show", "replace", "dismiss"], "description": "show mounts; replace swaps the same id in place; dismiss removes it." },
                        "id": { "type": "string", "description": "A stable name for this on-screen slot, so replace/dismiss can target it. Omit to auto-generate." },
                        "ref": { "type": "string", "description": "A view ref a builder reported (e.g. `project/view`) — the usual way to show a built view. Omit for dismiss." },
                        "source": { "type": "string", "description": "Raw JSX (default-exported component) for a trivial inline view, when not using a ref. Omit for dismiss." },
                    },
                    "required": ["op"],
                }),
            ),
            tool(
                "delegate",
                "Hand a heavy or long-running task (research, multi-step tool use, writing and \
                 running code) to a background working session, so you stay free to keep talking. \
                 It runs with your tools and memory but no voice; it reports back when done or if \
                 it gets stuck, and you'll see that as a new signal to fold into what you say next. \
                 To refine or build on what a worker just did, pass its `worker` id to continue \
                 the SAME session — it keeps all its context (the files it wrote, the data it \
                 gathered) and you avoid two workers clobbering the same work. The id of each \
                 running worker is shown in your 'Working sessions' status.",
                json!({
                    "type": "object",
                    "properties": {
                        "task": { "type": "string", "description": "A self-contained description of the work, with everything the worker needs to start." },
                        "worker": { "type": "integer", "description": "Optional: the id of an existing working session to continue (from your 'Working sessions' status). Omit to spawn a fresh worker; set it to follow up on or refine that worker's own work." },
                    },
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
            // A view is normally shown by ref (one a worker built); resolve it to
            // source HERE, server-side, so the JSX never enters the mind's context.
            // Inline `source` stays as a trivial-one-off escape hatch.
            let source = match arg_opt("ref") {
                Some(r) if !r.trim().is_empty() => match resolve_view_ref(data_dir, &r).await {
                    Ok(src) => src,
                    Err(err) => return tool_error(&format!("show_view ref `{r}`: {err}")),
                },
                _ => arg_str("source"),
            };
            sink.show_view(arg_opt("id"), op, source).await.map(|()| "shown")
        }
        "delegate" => {
            let task = arg_str("task");
            if task.trim().is_empty() {
                return tool_error("delegate requires a non-empty `task`");
            }
            let worker = args.get("worker").and_then(Value::as_u64);
            sink.send(SceneControl::Delegate { task, worker }).await.map(|()| "delegated to a working session")
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

/// A view ref is a relative path under the workspace, naming the view's source file
/// minus the `.jsx` — e.g. `badminton-top10/leader` → `workspace/badminton-top10/
/// leader.jsx`. Each `/`-separated segment is a slug (letters, digits, `-`, `_`) —
/// no dots, no empty segments — so the ref stays inside the workspace and can't
/// traverse out. The build sub-agent writes `<ref>.jsx` with its own file tools (no
/// MCP tool needed); this reads it back server-side, so the JSX never enters the
/// mind's context.
fn valid_view_ref(view_ref: &str) -> bool {
    !view_ref.is_empty()
        && view_ref.len() <= 128
        && view_ref.split('/').all(|seg| {
            !seg.is_empty()
                && seg.bytes().all(|b| matches!(b, b'A'..=b'Z' | b'a'..=b'z' | b'0'..=b'9' | b'-' | b'_'))
        })
}

/// Resolve a view ref to its stored JSX source, read from the workspace. The agent
/// passes only the tiny ref through `show_view`; this reads the component back.
async fn resolve_view_ref(data_dir: &std::path::Path, view_ref: &str) -> Result<String, String> {
    let view_ref = view_ref.trim();
    if !valid_view_ref(view_ref) {
        return Err(format!("invalid ref `{view_ref}` (names and `/` only, no dots)"));
    }
    let path = data_dir.join("workspace").join(format!("{view_ref}.jsx"));
    tokio::fs::read_to_string(&path).await.map_err(|e| format!("no such view ({e})"))
}

#[cfg(test)]
mod view_store_tests {
    use super::*;

    #[test]
    fn ref_validation_allows_nested_slugs_blocks_traversal() {
        assert!(valid_view_ref("badminton-top10"));
        assert!(valid_view_ref("badminton-top10/leader"));
        assert!(valid_view_ref("a/b/c_2"));
        assert!(!valid_view_ref(""), "empty");
        assert!(!valid_view_ref("../etc/passwd"), "dots blocked");
        assert!(!valid_view_ref("a//b"), "empty segment");
        assert!(!valid_view_ref("dot.name"), "dot blocked");
        assert!(!valid_view_ref("/abs"), "leading slash → empty segment");
        assert!(!valid_view_ref(&"x".repeat(129)), "too long");
    }

    #[tokio::test]
    async fn resolve_reads_workspace_source() {
        let dir = tempfile::tempdir().unwrap();
        let proj = dir.path().join("workspace").join("deck");
        tokio::fs::create_dir_all(&proj).await.unwrap();
        tokio::fs::write(proj.join("leader.jsx"), "export default () => 1").await.unwrap();
        assert_eq!(
            resolve_view_ref(dir.path(), "deck/leader").await.unwrap(),
            "export default () => 1"
        );
    }

    #[tokio::test]
    async fn resolve_rejects_bad_refs() {
        let dir = tempfile::tempdir().unwrap();
        assert!(resolve_view_ref(dir.path(), "../secret").await.is_err(), "traversal");
        assert!(resolve_view_ref(dir.path(), "missing/view").await.is_err(), "no file");
    }
}
