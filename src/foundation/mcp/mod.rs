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
//! HTTP glue lives in `crate::foundation::server::mcp`. Tool calls are forwarded to the right
//! scene loop through the [`ToolRegistry`]; see [`crate::body::reactor::tools`].

use serde_json::{Value, json};

use base64::Engine as _;

use std::collections::HashMap;
use std::path::Path;
use std::sync::Mutex;

use bytes::Bytes;
use chrono::{DateTime, Utc};

use crate::mind::memory::people_vectors;
use crate::body::reactor::{SceneControl, ToolRegistry};
use crate::foundation::server::PartialMinute;
use crate::types::{Geometry, Region, Scene};

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
            tool(
                "look",
                "See the user's screen right now — returns a screenshot of the main display, plus \
                 its pixel size and the frontmost app. Use it to find where things are before you \
                 `act`, and again after acting to confirm what changed. The positions you pass to \
                 `act` are fractions of THIS image.",
                json!({ "type": "object", "properties": {} }),
            ),
            tool(
                "act",
                "Operate the user's screen like a human would: move, click, type, or press keys. \
                 Positions are normalized fractions of the screen read off the latest `look` — `x` \
                 is 0.0 (left) to 1.0 (right), `y` is 0.0 (top) to 1.0 (bottom). After you act, call \
                 `look` again to check it worked.",
                json!({
                    "type": "object",
                    "properties": {
                        "action": { "type": "string", "enum": ["click", "double_click", "right_click", "move", "drag", "type", "press"], "description": "What to do." },
                        "x": { "type": "number", "description": "Target x as a 0..1 fraction of screen width. For click/double_click/right_click/move, and the start of a drag." },
                        "y": { "type": "number", "description": "Target y as a 0..1 fraction of screen height." },
                        "x2": { "type": "number", "description": "Drag end x (0..1), for action=drag." },
                        "y2": { "type": "number", "description": "Drag end y (0..1), for action=drag." },
                        "text": { "type": "string", "description": "Text to type, for action=type (handles non-ASCII like a song title)." },
                        "key": { "type": "string", "description": "Key for action=press: return, tab, space, escape, delete, up, down, left, right, or a single character. For a chord like ⌘A use key=a with mods=[command]." },
                        "mods": { "type": "array", "items": { "type": "string", "enum": ["command", "shift", "option", "control"] }, "description": "Modifier keys held during a press." },
                    },
                    "required": ["action"],
                }),
            ),
            watch_tool(),
        ],
        // The reflection ("sleep") surface: a voice-less session that consolidates
        // the raw log into derived memory. One pass spans every recently-active
        // scene at once — the signals come grouped by scene, each group numbered
        // from 1 — so every scene-specific tool (`record_episode`, `keep_and_fade`,
        // `see`) names the scene it acts on.
        Some("reflection") => vec![
            tool(
                "record_episode",
                "File one coherent event as an episode. You are shown each scene's still-unconsolidated \
                 signals as its own numbered list, oldest first, under a `# Scene: <id>` header; `scene` is \
                 that id and `count` is how many signals from the TOP of THAT scene's list this one episode \
                 covers. Work one scene at a time, in order, front to back — each call consumes that many \
                 signals from the front of its scene, so the next `count` for the same scene starts after \
                 them. STOP early (just don't cover the last few) when the most recent signals are an event \
                 still in progress; they'll come back next time. `gist` is the consolidated event in your own \
                 prose. `title` is a short handle for this event (a few words) — it becomes the episode's \
                 directory name, so make it specific and human-readable (e.g. \"Lunch plan with Alice\", \
                 \"Kyoto flights booked\"). `subjects` are the `dimension/subject` refs this episode is about \
                 (e.g. `people/alice`, `projects/kyoto-trip`) — list every subject you'll want to update a \
                 facet for. The call returns the episode's ref; cite it when you update a facet.",
                json!({
                    "type": "object",
                    "properties": {
                        "scene": { "type": "string", "description": "The scene this episode belongs to — the id from its `# Scene: <id>` group header." },
                        "count": { "type": "integer", "minimum": 1, "description": "How many signals from the top of THAT scene's unconsolidated list this episode covers." },
                        "title": { "type": "string", "description": "A short, specific handle for this event (a few words); becomes the episode's directory name, e.g. \"Lunch plan with Alice\"." },
                        "gist": { "type": "string", "description": "The consolidated event, in prose — what happened, what mattered." },
                        "subjects": { "type": "array", "items": { "type": "string" }, "description": "The dimension/subject refs this episode touches, e.g. [\"people/alice\", \"projects/kyoto-trip\"]." },
                    },
                    "required": ["scene", "count", "title", "gist"],
                }),
            ),
            tool(
                "read_facet",
                "Read your current understanding of one subject before you rewrite it, so you fold new \
                 episodes into what you already know instead of starting blank. Returns the facet's \
                 current text, or a note that none exists yet.",
                json!({
                    "type": "object",
                    "properties": {
                        "dimension": { "type": "string", "description": "The subject's dimension, e.g. people, locations, projects, culture." },
                        "subject": { "type": "string", "description": "The subject's name, e.g. alice, kyoto-trip." },
                    },
                    "required": ["dimension", "subject"],
                }),
            ),
            tool(
                "update_facet",
                "Write your whole current understanding of one subject — regenerate the file, don't patch \
                 it: pass the complete text (old understanding folded together with the new), not just a \
                 delta. Every claim should cite the episode(s) it came from by their refs (the values \
                 record_episode returned). Dimensions are open-ended; reuse an existing dimension/subject \
                 when one fits rather than coining a near-duplicate.",
                json!({
                    "type": "object",
                    "properties": {
                        "dimension": { "type": "string", "description": "The subject's dimension, e.g. people, locations, projects, culture." },
                        "subject": { "type": "string", "description": "The subject's name, e.g. alice, kyoto-trip." },
                        "content": { "type": "string", "description": "The full regenerated understanding (markdown), every claim citing its source episode refs." },
                    },
                    "required": ["dimension", "subject", "content"],
                }),
            ),
            tool(
                "name_person",
                "Attach a name to a person you've recognized. Faces and voices are clustered \
                 automatically — a face shows as `⟨faces: <id>⟩`, a speaker as `⟨voice: <id>⟩`, \
                 where an opaque id like `ff32ce3w` is someone not yet named. When a signal tells \
                 you who an id is (e.g. the person says their name, or someone introduces them), \
                 call this with `id` = that id and `name` = the name (the `people/<name>` ref you'd \
                 use for their facet). It renames the whole cluster from the id to the name, so you \
                 recognize them by name next time. If the name already exists, the two are merged.",
                json!({
                    "type": "object",
                    "properties": {
                        "id": { "type": "string", "description": "The cluster's current key — a `⟨faces: …⟩` or `⟨voice: …⟩` id (e.g. ff32ce3w), or an existing name to re-key." },
                        "name": { "type": "string", "description": "The person's name to key them under (e.g. 赵力, alice)." },
                    },
                    "required": ["id", "name"],
                }),
            ),
            tool(
                "merge_people",
                "Collapse two clusters that are the same person into one — when you realize a face \
                 or voice id (or a name) actually refers to someone you already model, including \
                 across senses (a `⟨voice: …⟩` id and a `⟨faces: …⟩` id that are one source). Folds \
                 `from`'s face/voice gallery into `into` and drops `from`.",
                json!({
                    "type": "object",
                    "properties": {
                        "from": { "type": "string", "description": "The duplicate cluster's key (an id or name) to fold away." },
                        "into": { "type": "string", "description": "The cluster's key (an id or name) to keep." },
                    },
                    "required": ["from", "into"],
                }),
            ),
            tool(
                "keep_and_fade",
                "Let a cold day's media fade to the text, keeping only the moments worth keeping \
                 vivid. Use it on a day from the old-store list you're shown — one genuinely old and \
                 settled, heaviest first — when the raw bytes are vividness the words have outlived. \
                 `scene` is the scene that day belongs to (the `# Scene: <id>` group the old-store list \
                 appeared under). `channel` is `audio` or `vision`, `date` the `YYYY-MM-DD` day. `keep` is \
                 the spans to preserve, each `{start, end}` in RFC3339 — a vision keepsake is a still at \
                 `start`, an audio keepsake the clip `[start, end)`. Keep almost nothing: a frame or a few \
                 seconds, often none — pass `keep: []` to fade straight to text (which always remains). Keep \
                 only what the transcript can't carry (a face, a place, the sound of a voice), never someone \
                 merely talking. You can only fade a day already behind your consolidation; the tool \
                 refuses the rest.",
                json!({
                    "type": "object",
                    "properties": {
                        "scene": { "type": "string", "description": "The scene this day belongs to — the id from the `# Scene: <id>` group its old-store list appeared under." },
                        "channel": { "type": "string", "enum": ["audio", "vision"], "description": "Which sense's media to fade for this day." },
                        "date": { "type": "string", "description": "The day to fade, YYYY-MM-DD (UTC), from the old-store list." },
                        "keep": {
                            "type": "array",
                            "description": "Spans to keep vivid; omit or [] to fade straight to text. Each is one keepsake.",
                            "items": {
                                "type": "object",
                                "properties": {
                                    "start": { "type": "string", "description": "Span start, RFC3339 (the instant, for a vision still)." },
                                    "end": { "type": "string", "description": "Span end, RFC3339 (equal to start for a still; later for an audio clip)." },
                                },
                                "required": ["start", "end"],
                            },
                        },
                    },
                    "required": ["scene", "channel", "date"],
                }),
            ),
            see_tool(),
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
                 to evolve a view in place; op=dismiss takes one down. The screen is persistent \
                 state: whatever you've shown stays up — across page refreshes, other devices in \
                 the scene, even restarts — until you dismiss or replace it, so never re-show \
                 something that's already on screen. Re-showing an existing id raises it above \
                 the other views. For a trivial one-off you may pass raw `source` JSX instead of \
                 a ref.",
                json!({
                    "type": "object",
                    "properties": {
                        "op": { "type": "string", "enum": ["show", "replace", "dismiss"], "description": "show mounts; replace swaps the same id in place; dismiss removes it." },
                        "id": { "type": "string", "description": "A stable name for this on-screen slot, so replace/dismiss can target it. Omit to auto-generate." },
                        "ref": { "type": "string", "description": "A view ref a builder reported (e.g. `project/view`) — the usual way to show a built view. Omit for dismiss." },
                        "source": { "type": "string", "description": "Raw JSX (default-exported component) for a trivial inline view, when not using a ref. Omit for dismiss." },
                        "region": { "type": "string", "enum": ["center", "top", "bottom", "left", "right", "top_left", "top_right", "bottom_left", "bottom_right", "fill"], "description": "Optional: where on the stage to place this view. Omit to use the placement the builder chose — only set it to override, e.g. when arranging several views at once." },
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
            tool(
                "record_reflex",
                "Teach a quick-action reflex the user can later fire instantly, with no model in \
                 the loop. Use it when they ask you to remember filling a specific field with a \
                 specific value — e.g. \"on this signup page my ID number is 11010119…\". It stores \
                 the value and how to find the field, so a later invoke clicks that field and types \
                 it straight in. `name` is a short handle (e.g. \"fill my ID\"). `value` is exactly \
                 what to type. `label_contains` is text from the field's on-screen label (e.g. \"ID \
                 number\", \"身份证\"). Optionally narrow the situation with `app` (frontmost app, e.g. \
                 \"Safari\"), `title_contains` (a window-title substring), and `role` (the control's \
                 accessibility role; defaults to AXTextField). Keep the label/app specific enough \
                 that only the intended field can match — it fires only when exactly one field matches.",
                json!({
                    "type": "object",
                    "properties": {
                        "name": { "type": "string", "description": "Short handle for the reflex, e.g. \"fill my ID\"." },
                        "value": { "type": "string", "description": "Exactly the text to type into the field." },
                        "label_contains": { "type": "string", "description": "A substring of the target field's on-screen label, e.g. \"ID number\" or \"身份证\"." },
                        "app": { "type": "string", "description": "Optional: require this frontmost app (substring), e.g. \"Safari\"." },
                        "title_contains": { "type": "string", "description": "Optional: require this substring in the frontmost window title." },
                        "role": { "type": "string", "description": "Optional: the target control's accessibility role; defaults to AXTextField." },
                    },
                    "required": ["name", "value", "label_contains"],
                }),
            ),
            see_tool(),
            watch_tool(),
        ],
    }
}

fn tool(name: &str, description: &str, input_schema: Value) -> Value {
    json!({ "name": name, "description": description, "inputSchema": input_schema })
}

/// The `see` tool — understand a still the person handed in (or one surfaced in a
/// signal). Shared by the reactor (answer in conversation) and reflection (index a
/// day's photos). The bundle decides how: a native-vision model gets the raw image
/// to reason over; a text-only one gets the vision capability's description.
fn see_tool() -> Value {
    tool(
        "see",
        "Look at a still image and answer about it — a photo the person sent or held up to the \
         camera, surfaced to you as a signal like `📷 photo arrived ⟨ref: …⟩`. Pass that `ref`, and \
         optionally what you want to know. Reach for it the moment seeing the picture beats guessing: \
         read a label/menu/handwriting, identify a thing, check what's on a screen they photographed.",
        json!({
            "type": "object",
            "properties": {
                "ref": { "type": "string", "description": "The ⟨ref: …⟩ from the photo's signal, e.g. 2026-06-25/14/23-07.jpg." },
                "prompt": { "type": "string", "description": "Optional: what you want to know about the image (a question or focus). Omit to just look." },
                "scene": { "type": "string", "description": "Reflection only: the scene shown next to a `see` ref (its `# Scene: <id>` group). Pass it so the still resolves. Omit in conversation." },
            },
            "required": ["ref"],
        }),
    )
}

/// The `watch` tool — understand a short span of the *live* camera. Shared by the
/// reactor (in conversation) and workers (mid-task). Always polyfilled: the clip is
/// understood by the vision capability and the text handed back.
fn watch_tool() -> Value {
    tool(
        "watch",
        "Watch a few seconds of the live camera and tell what happened — for when motion or a \
         sequence matters, not a single frame (someone's action, a gesture, \"did you see that?\"). \
         It reads the camera streaming right now; say how far back with `span` (e.g. \"last 20s\"), or \
         omit it for the most recent stretch. Carry seconds, not minutes. Optionally say what to look \
         for with `prompt`.",
        json!({
            "type": "object",
            "properties": {
                "span": { "type": "string", "description": "How far back to look, e.g. \"last 20s\". Omit for the most recent stretch." },
                "prompt": { "type": "string", "description": "Optional: what to look for or assess (e.g. \"what's wrong with my serve?\")." },
            },
        }),
    )
}


/// Handle one parsed JSON-RPC message. `scene`/`role`/`worker_id` come from the
/// request headers; `registry` routes tool calls to the owning scene loop.
pub async fn handle(
    registry: &ToolRegistry,
    data_dir: &std::path::Path,
    video_partial: &Mutex<HashMap<Scene, PartialMinute>>,
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
                dispatch_tool(registry, data_dir, video_partial, scene.as_ref(), worker_id, name, &args).await,
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
    video_partial: &Mutex<HashMap<Scene, PartialMinute>>,
    scene: Option<&Scene>,
    worker_id: Option<u64>,
    name: &str,
    args: &Value,
) -> Value {
    let Some(scene) = scene else {
        return tool_error("missing X-HI-Scene header");
    };

    // Reflection tools are pure derived-memory IO over `data_dir`; they don't touch
    // the scene loop (no sink), so handle them before the sink lookup. The
    // consolidated reflection session spans every scene, so the scene-specific ones
    // (`record_episode`/`keep_and_fade`/`see`) take their scene from the args, not the
    // (sentinel) header — `see` falls back to the header for the live reactor surface.
    match name {
        "record_episode" => return reflection_record_episode(data_dir, args).await,
        "read_facet" => return reflection_read_facet(data_dir, args).await,
        "update_facet" => return reflection_update_facet(data_dir, args).await,
        "name_person" => return reflection_name_person(data_dir, args).await,
        "merge_people" => return reflection_merge_people(data_dir, args).await,
        "keep_and_fade" => return reflection_keep_and_fade(data_dir, args).await,
        "record_reflex" => return reflex_record(data_dir, args).await,
        "look" => return do_look().await,
        "act" => return do_act(args).await,
        "see" => return do_see(data_dir, scene, args).await,
        "watch" => return do_watch(scene, video_partial, args).await,
        _ => {}
    }

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
            // Inline `source` stays as a trivial-one-off escape hatch. The ref may
            // carry a `.geom.json` sidecar — the placement the builder chose.
            let (source, sidecar_geom) = match arg_opt("ref") {
                Some(r) if !r.trim().is_empty() => match resolve_view_ref(data_dir, &r).await {
                    Ok(resolved) => resolved,
                    Err(err) => return tool_error(&format!("show_view ref `{r}`: {err}")),
                },
                _ => (arg_str("source"), None),
            };
            // The mind may override where it goes (when arranging several at once);
            // otherwise the builder's declared geometry stands. Absent both = floor.
            let region_override = arg_opt("region").as_deref().and_then(parse_region);
            let geometry = match (sidecar_geom, region_override) {
                (Some(g), Some(region)) => Some(Geometry { region, ..g }),
                (Some(g), None) => Some(g),
                (None, Some(region)) => Some(Geometry { region, ..Default::default() }),
                (None, None) => None,
            };
            sink.show_view(arg_opt("id"), op, source, geometry).await.map(|()| "shown")
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

/// `look`: capture the screen so the calling session can see where to act. Returns
/// a text hint (size + frontmost app) and the screenshot as an image content block,
/// which `claude-agent-acp` forwards to the multimodal model. Errors when capture
/// is unavailable (non-macOS, or Screen Recording not granted).
async fn do_look() -> Value {
    let snap = match crate::body::capabilities::desktop_context::capture().await {
        Ok(s) => s,
        Err(e) => return tool_error(&format!("screen capture not available here: {e}")),
    };
    let Some(png) = snap.screenshot_png else {
        return tool_error("no screenshot — grant Screen Recording to the host app");
    };
    let mut hint = match png_dimensions(&png) {
        Some((w, h)) => format!("screenshot of the main display, {w}x{h} px"),
        None => "screenshot of the main display".to_string(),
    };
    if let Some(app) = &snap.frontmost_app {
        hint.push_str(&format!("; frontmost app: {app}"));
    }
    if let Some(title) = &snap.frontmost_window_title {
        hint.push_str(&format!("; front window: {title}"));
    }
    let b64 = base64::engine::general_purpose::STANDARD.encode(&png);
    json!({
        "content": [
            { "type": "text", "text": hint },
            { "type": "image", "data": b64, "mimeType": "image/png" },
        ],
        "isError": false,
    })
}

/// `act`: synthesize one input action on the host. Coordinates arrive as normalized
/// 0..1 fractions of the screen (what the model reasons about, looking at `look`'s
/// image) and are mapped to the main display's points here, so the pixel-vs-point
/// Retina detail never reaches the model.
async fn do_act(args: &Value) -> Value {
    use crate::body::capabilities::input::{self, Action, Point};
    let action = args.get("action").and_then(Value::as_str).unwrap_or_default();

    let act = match action {
        "type" => {
            let text = args.get("text").and_then(Value::as_str).unwrap_or_default();
            if text.is_empty() {
                return tool_error("act `type` requires non-empty `text`");
            }
            Action::Type(text.to_string())
        }
        "press" => {
            let Some(key) = parse_key(args.get("key").and_then(Value::as_str).unwrap_or_default())
            else {
                return tool_error(
                    "act `press` needs a valid `key`: return, tab, space, escape, delete, \
                     up/down/left/right, or a single character",
                );
            };
            Action::Press { key, mods: parse_mods(args.get("mods")) }
        }
        "click" | "double_click" | "right_click" | "move" | "drag" => {
            let (w, h) = match input::main_display_point_size() {
                Ok(s) => s,
                Err(e) => return tool_error(&format!("could not read display size: {e}")),
            };
            let pt = |xk: &str, yk: &str| -> Option<Point> {
                let x = args.get(xk).and_then(Value::as_f64)?;
                let y = args.get(yk).and_then(Value::as_f64)?;
                Some(Point { x: x.clamp(0.0, 1.0) * w, y: y.clamp(0.0, 1.0) * h })
            };
            let Some(from) = pt("x", "y") else {
                return tool_error("act requires `x` and `y` as 0..1 fractions of the screen");
            };
            match action {
                "click" => Action::Click(from),
                "double_click" => Action::DoubleClick(from),
                "right_click" => Action::RightClick(from),
                "move" => Action::MoveTo(from),
                "drag" => {
                    let Some(to) = pt("x2", "y2") else {
                        return tool_error("act `drag` requires `x2` and `y2` (the drag end, 0..1)");
                    };
                    Action::Drag { from, to }
                }
                _ => unreachable!(),
            }
        }
        other => return tool_error(&format!("unknown act action `{other}`")),
    };

    match input::perform(act).await {
        Ok(()) => tool_ok("acted"),
        Err(e) => tool_error(&e.to_string()),
    }
}

/// Read (width, height) from a PNG's IHDR header — big-endian, right after the
/// 8-byte signature. `None` if the bytes aren't a PNG we recognize.
fn png_dimensions(png: &[u8]) -> Option<(u32, u32)> {
    if png.len() < 24 || &png[12..16] != b"IHDR" {
        return None;
    }
    let w = u32::from_be_bytes(png[16..20].try_into().ok()?);
    let h = u32::from_be_bytes(png[20..24].try_into().ok()?);
    Some((w, h))
}

/// Map an `act` `key` string to a [`crate::body::capabilities::input::Key`]. Named keys
/// are case-insensitive; anything else is taken as a single character (so `a`, `/`,
/// `7` work). `None` for an empty or multi-character unknown name.
fn parse_key(s: &str) -> Option<crate::body::capabilities::input::Key> {
    use crate::body::capabilities::input::Key;
    Some(match s.to_ascii_lowercase().as_str() {
        "return" | "enter" => Key::Return,
        "tab" => Key::Tab,
        "space" => Key::Space,
        "escape" | "esc" => Key::Escape,
        "delete" | "backspace" => Key::Delete,
        "up" => Key::ArrowUp,
        "down" => Key::ArrowDown,
        "left" => Key::ArrowLeft,
        "right" => Key::ArrowRight,
        other => {
            let mut chars = other.chars();
            let c = chars.next()?;
            if chars.next().is_some() {
                return None;
            }
            Key::Char(c)
        }
    })
}

/// Map an `act` `mods` array to modifiers, accepting common aliases. Unknown
/// entries are dropped.
fn parse_mods(v: Option<&Value>) -> Vec<crate::body::capabilities::input::Modifier> {
    use crate::body::capabilities::input::Modifier;
    v.and_then(Value::as_array)
        .map(|arr| {
            arr.iter()
                .filter_map(|m| {
                    Some(match m.as_str()?.to_ascii_lowercase().as_str() {
                        "command" | "cmd" | "meta" => Modifier::Command,
                        "shift" => Modifier::Shift,
                        "option" | "alt" => Modifier::Option,
                        "control" | "ctrl" => Modifier::Control,
                        _ => return None,
                    })
                })
                .collect()
        })
        .unwrap_or_default()
}

/// The scene a reflection tool names explicitly in its args. The consolidated
/// reflection session spans every scene, so the scene-writing tools carry the scene
/// they act on as an argument rather than reading the session's (sentinel) header.
/// `None` (and the caller's error) when missing or blank.
fn arg_scene(args: &Value) -> Option<Scene> {
    args.get("scene")
        .and_then(Value::as_str)
        .map(str::trim)
        .filter(|s| !s.is_empty())
        .map(|s| Scene(s.to_string()))
}

/// `record_episode`: file the first `count` of the named scene's unconsolidated
/// signals as one episode (see [`crate::mind::memory::episodes::record_episode`]).
/// Returns the episode ref for the session to cite when it updates a facet.
async fn reflection_record_episode(data_dir: &std::path::Path, args: &Value) -> Value {
    let Some(scene) = arg_scene(args) else {
        return tool_error(
            "record_episode requires `scene` — the id from the episode's `# Scene: <id>` group header",
        );
    };
    let Some(count) = args.get("count").and_then(Value::as_u64) else {
        return tool_error("record_episode requires an integer `count` >= 1");
    };
    let gist = args.get("gist").and_then(Value::as_str).unwrap_or_default();
    if gist.trim().is_empty() {
        return tool_error("record_episode requires a non-empty `gist`");
    }
    let title = args.get("title").and_then(Value::as_str).unwrap_or_default();
    if title.trim().is_empty() {
        return tool_error("record_episode requires a non-empty `title`");
    }
    let subjects: Vec<String> = args
        .get("subjects")
        .and_then(Value::as_array)
        .map(|a| a.iter().filter_map(|v| v.as_str().map(str::to_owned)).collect())
        .unwrap_or_default();
    match crate::mind::memory::episodes::record_episode(data_dir, &scene, count as usize, title, gist, &subjects)
        .await
    {
        Ok(name) => tool_ok(&format!("recorded episode {name}")),
        Err(err) => tool_error(&err.to_string()),
    }
}

/// `read_facet`: return the current understanding of a subject, or a note that
/// none exists yet, so the session regenerates from the old rather than blank.
async fn reflection_read_facet(data_dir: &std::path::Path, args: &Value) -> Value {
    let dim = args.get("dimension").and_then(Value::as_str).unwrap_or_default();
    let subject = args.get("subject").and_then(Value::as_str).unwrap_or_default();
    if dim.trim().is_empty() || subject.trim().is_empty() {
        return tool_error("read_facet requires `dimension` and `subject`");
    }
    match crate::mind::memory::facets::read_facet(data_dir, dim, subject).await {
        Ok(Some(content)) => tool_ok(&content),
        Ok(None) => tool_ok("(no facet yet — this subject has no recorded understanding)"),
        Err(err) => tool_error(&err.to_string()),
    }
}

/// `update_facet`: write the whole regenerated understanding of a subject (see
/// [`crate::mind::memory::facets::update_facet`]). Returns the `<dim>/<subject>` ref.
async fn reflection_update_facet(data_dir: &std::path::Path, args: &Value) -> Value {
    let dim = args.get("dimension").and_then(Value::as_str).unwrap_or_default();
    let subject = args.get("subject").and_then(Value::as_str).unwrap_or_default();
    let content = args.get("content").and_then(Value::as_str).unwrap_or_default();
    if dim.trim().is_empty() || subject.trim().is_empty() {
        return tool_error("update_facet requires `dimension` and `subject`");
    }
    if content.trim().is_empty() {
        return tool_error("update_facet requires non-empty `content`");
    }
    match crate::mind::memory::facets::update_facet(data_dir, dim, subject, content).await {
        Ok(refname) => tool_ok(&format!("updated facet {refname}")),
        Err(err) => tool_error(&err.to_string()),
    }
}

/// `record_reflex`: teach a quick-action reflex (see [`crate::body::reflex`]). Stores the
/// fill value and how to find its field so a later invoke types it with no model in
/// the loop. The value itself is never echoed back in the ack.
async fn reflex_record(data_dir: &std::path::Path, args: &Value) -> Value {
    let name = args.get("name").and_then(Value::as_str).unwrap_or_default();
    let value = args.get("value").and_then(Value::as_str).unwrap_or_default();
    let label_contains = args.get("label_contains").and_then(Value::as_str).unwrap_or_default();
    if name.trim().is_empty() {
        return tool_error("record_reflex requires a non-empty `name`");
    }
    if value.trim().is_empty() {
        return tool_error("record_reflex requires a non-empty `value`");
    }
    if label_contains.trim().is_empty() {
        return tool_error("record_reflex requires a non-empty `label_contains`");
    }
    let opt = |k: &str| {
        args.get(k)
            .and_then(Value::as_str)
            .map(str::trim)
            .filter(|s| !s.is_empty())
            .map(str::to_owned)
    };
    let id = crate::body::reflex::id_for(name);
    if id.is_empty() {
        return tool_error("record_reflex `name` must contain a usable character");
    }
    let reflex = crate::body::reflex::Reflex {
        id,
        name: name.to_string(),
        trigger: crate::body::reflex::Trigger {
            app: opt("app"),
            title_contains: opt("title_contains"),
            role: opt("role"),
            label_contains: label_contains.to_string(),
        },
        value: value.to_string(),
    };
    match crate::body::reflex::save(data_dir, &reflex).await {
        Ok(id) => tool_ok(&format!("learned reflex '{name}' ({id})")),
        Err(err) => tool_error(&err.to_string()),
    }
}

/// `name_person`: rename a person's cluster (face or voice) from its `id` (or
/// current key) to a learned `name` — the structural side of "we now know who
/// this is". Merges if the name already exists. See [`people_vectors::rename`].
async fn reflection_name_person(data_dir: &std::path::Path, args: &Value) -> Value {
    let id = args.get("id").and_then(Value::as_str).unwrap_or_default();
    let name = args.get("name").and_then(Value::as_str).unwrap_or_default();
    if id.trim().is_empty() || name.trim().is_empty() {
        return tool_error("name_person requires `id` and `name`");
    }
    match people_vectors::rename(data_dir, id, name).await {
        Ok(()) => tool_ok(&format!("named {id} → people/{name}")),
        Err(err) => tool_error(&err.to_string()),
    }
}

/// `merge_people`: fold the `from` cluster into `into` (same person, two keys —
/// across senses too, e.g. a voice id into an already-named face). See
/// [`people_vectors::rename`].
async fn reflection_merge_people(data_dir: &std::path::Path, args: &Value) -> Value {
    let from = args.get("from").and_then(Value::as_str).unwrap_or_default();
    let into = args.get("into").and_then(Value::as_str).unwrap_or_default();
    if from.trim().is_empty() || into.trim().is_empty() {
        return tool_error("merge_people requires `from` and `into`");
    }
    match people_vectors::rename(data_dir, from, into).await {
        Ok(()) => tool_ok(&format!("merged people/{from} → people/{into}")),
        Err(err) => tool_error(&err.to_string()),
    }
}

/// `keep_and_fade`: let a cold consolidated day's media fade to text, keeping the
/// spans the mind chose (see [`crate::mind::memory::decay::keep_and_fade`]). The safety
/// gate lives in the tool, so an attempt on an un-consolidated day comes back as a
/// tool error the session can read, not a panic.
async fn reflection_keep_and_fade(data_dir: &std::path::Path, args: &Value) -> Value {
    let Some(scene) = arg_scene(args) else {
        return tool_error(
            "keep_and_fade requires `scene` — the id from the day's `# Scene: <id>` group header",
        );
    };
    let Some(channel) = args.get("channel").and_then(Value::as_str) else {
        return tool_error("keep_and_fade requires `channel` (audio|vision)");
    };
    let Ok(channel) = channel.parse::<crate::types::Channel>() else {
        return tool_error(&format!("keep_and_fade: unknown channel {channel:?}"));
    };
    let date = args.get("date").and_then(Value::as_str).unwrap_or_default();
    if date.trim().is_empty() {
        return tool_error("keep_and_fade requires `date` (YYYY-MM-DD)");
    }
    let mut spans = Vec::new();
    if let Some(arr) = args.get("keep").and_then(Value::as_array) {
        for (i, item) in arr.iter().enumerate() {
            let parse = |k: &str| {
                item.get(k)
                    .and_then(Value::as_str)
                    .and_then(|s| chrono::DateTime::parse_from_rfc3339(s).ok())
                    .map(|t| t.with_timezone(&chrono::Utc))
            };
            let (Some(start), Some(end)) = (parse("start"), parse("end")) else {
                return tool_error(&format!(
                    "keep_and_fade: keep[{i}] needs RFC3339 `start` and `end`"
                ));
            };
            spans.push(crate::mind::memory::decay::KeepSpan { start, end });
        }
    }
    match crate::mind::memory::decay::keep_and_fade(data_dir, &scene, channel, date, &spans).await {
        Ok(r) => tool_ok(&format!(
            "faded {} {date}: kept {} keepsake(s), freed {} bytes",
            channel.as_str(),
            r.kept,
            r.bytes_freed
        )),
        Err(err) => tool_error(&err.to_string()),
    }
}

/// `see`: understand a stored still. Resolves the `ref` (the `⟨ref: …⟩` from a
/// `📷 photo arrived` signal, or one surfaced to reflection) to its bytes, then hands
/// it to [`perceive_still`] — which the bundle routes either to the model's own eyes
/// (native vision) or through the vision capability (text-only model).
async fn do_see(data_dir: &Path, scene: &Scene, args: &Value) -> Value {
    let prompt = args.get("prompt").and_then(Value::as_str).unwrap_or_default();
    let Some(reff) = args.get("ref").and_then(Value::as_str).filter(|s| !s.trim().is_empty()) else {
        return tool_error(
            "see needs `ref` — the ⟨ref: …⟩ from the photo's signal, e.g. 2026-06-25/14/23-07.jpg",
        );
    };
    let Some((ts, rel, mime)) = parse_still_ref(reff) else {
        return tool_error(&format!(
            "see: malformed ref {reff:?} (expected <YYYY-MM-DD>/<HH>/<MM>-<SS>.<ext>)"
        ));
    };
    // The live reactor `see` resolves against its own scene (the header). The
    // consolidated reflection session has no single scene, so it names the scene the
    // ref belongs to in `scene` — honored here, header otherwise.
    let owned = arg_scene(args);
    let scene = owned.as_ref().unwrap_or(scene);
    let Some(path) =
        crate::mind::memory::media::resolve(data_dir, scene, crate::types::Channel::Vision, ts, &rel).await
    else {
        return tool_error(&format!("see: no media at {reff} (it may have faded)"));
    };
    let bytes = match tokio::fs::read(&path).await {
        Ok(b) => Bytes::from(b),
        Err(e) => return tool_error(&format!("see: reading {reff} failed: {e}")),
    };
    perceive_still(bytes, &mime, prompt).await
}

/// `watch`: understand a short span of the live camera. Reads the in-progress
/// (not-yet-flushed) minute from [`PartialMinute`] — the freshest source — optionally
/// trims it to the requested tail with ffmpeg, and hands the clip to
/// [`perceive_clip`]. Errors plainly when no camera is streaming, so the model can
/// ask the person to turn it on.
async fn do_watch(
    scene: &Scene,
    video_partial: &Mutex<HashMap<Scene, PartialMinute>>,
    args: &Value,
) -> Value {
    let prompt = args.get("prompt").and_then(Value::as_str).unwrap_or_default();
    let span = args.get("span").and_then(Value::as_str).unwrap_or_default();

    let Some((bytes, mime)) = partial_clip(video_partial, scene) else {
        return tool_error(
            "no live camera to watch — `watch` reads the camera streaming right now; ask the person \
             to turn it on, then try again.",
        );
    };

    // Trim to the requested tail when asked and ffmpeg can; on any trouble fall back
    // to the whole stretch (≤ ~1 min) rather than failing the look.
    let clip = match parse_last_secs(span) {
        Some(secs) => trim_tail(&bytes, &mime, secs).await.unwrap_or(bytes),
        None => bytes,
    };
    perceive_clip(clip, &mime, prompt).await
}

/// Understand a still per the current [`bundle`](crate::body::capabilities::bundle):
/// a native-vision model gets the raw image as a tool-result block to reason over; a
/// text-only model gets the vision capability's description as text.
async fn perceive_still(bytes: Bytes, mime: &str, prompt: &str) -> Value {
    use crate::body::capabilities::bundle::{self, Handling, Modality};
    match bundle::current().handling(Modality::Image) {
        Handling::Native => {
            let b64 = base64::engine::general_purpose::STANDARD.encode(&bytes);
            let hint = if prompt.trim().is_empty() {
                "the still you asked to see".to_string()
            } else {
                format!("the still you asked to see — you wanted to know: {prompt}")
            };
            json!({
                "content": [
                    { "type": "text", "text": hint },
                    { "type": "image", "data": b64, "mimeType": mime },
                ],
                "isError": false,
            })
        }
        Handling::Polyfill => {
            use crate::body::capabilities::vision::{self as vision_cap, VisualMedia};
            if !vision_cap::available() {
                return tool_error("can't see stills here — no vision provider configured (set VISION_PROVIDER)");
            }
            let q = if prompt.trim().is_empty() { "Describe what you see." } else { prompt };
            match vision_cap::understand(VisualMedia::image_bytes(bytes, mime.to_string()), q).await {
                Ok(text) => tool_ok(&text),
                Err(e) => tool_error(&format!("vision understanding failed: {e}")),
            }
        }
    }
}

/// Understand a short video clip. Always polyfilled — no model reached through the
/// adapter takes video — so the clip goes to the vision capability and the answer
/// comes back as text.
async fn perceive_clip(bytes: Bytes, mime: &str, prompt: &str) -> Value {
    use crate::body::capabilities::bundle::{self, Modality};
    use crate::body::capabilities::vision::{self as vision_cap, VisualMedia};
    // The bundle always polyfills video today — no adapter path carries video to the
    // model — so this is the only arm; consulting `handling` keeps the
    // native-vs-polyfill decision in one place for the day a native-video model lands.
    let _ = bundle::current().handling(Modality::Video);
    if !vision_cap::available() {
        return tool_error("can't watch video here — no vision provider configured (set VISION_PROVIDER)");
    }
    let q = if prompt.trim().is_empty() { "Describe what happens in this clip." } else { prompt };
    match vision_cap::understand(VisualMedia::video_bytes(bytes, mime.to_string()), q).await {
        Ok(text) => tool_ok(&text),
        Err(e) => tool_error(&format!("video understanding failed: {e}")),
    }
}

/// Concatenate a scene's in-progress minute (`init` + `buf`) into one
/// independently-decodable clip, with its container mime. `None` when no camera is
/// streaming for the scene.
fn partial_clip(map: &Mutex<HashMap<Scene, PartialMinute>>, scene: &Scene) -> Option<(Bytes, String)> {
    let guard = map.lock().unwrap();
    let p = guard.get(scene)?;
    let mut v = Vec::with_capacity(p.init.len() + p.buf.len());
    v.extend_from_slice(&p.init);
    v.extend_from_slice(&p.buf);
    Some((Bytes::from(v), p.mime.clone()))
}

/// Trim the last `secs` seconds out of an in-memory clip via ffmpeg. Writes the
/// bytes to a temp input file (ffmpeg needs a seekable input for `-sseof`), clips,
/// and cleans up. `Err` (no ffmpeg, undecodable) lets the caller send the whole clip.
async fn trim_tail(bytes: &Bytes, mime: &str, secs: f64) -> anyhow::Result<Bytes> {
    let ext = if mime.contains("mp4") { "mp4" } else { "webm" };
    let tmp = std::env::temp_dir().join(format!("hi-watch-{}.{ext}", uuid::Uuid::now_v7()));
    tokio::fs::write(&tmp, bytes).await?;
    let res = crate::foundation::vendors::ffmpeg_frame::clip_video(&tmp, -secs, secs).await;
    let _ = tokio::fs::remove_file(&tmp).await;
    res
}

/// Pull a tail length out of a `watch` span like "last 20s" / "20 seconds" → 20.0.
/// `None` (no number) means "the whole recent stretch".
fn parse_last_secs(span: &str) -> Option<f64> {
    let digits: String = span
        .chars()
        .skip_while(|c| !c.is_ascii_digit())
        .take_while(|c| c.is_ascii_digit() || *c == '.')
        .collect();
    digits.parse::<f64>().ok().filter(|n| *n > 0.0)
}

/// Map a still image extension to its MIME, for the native image content block.
fn ext_to_mime(ext: &str) -> String {
    match ext.to_ascii_lowercase().as_str() {
        "jpg" | "jpeg" => "image/jpeg",
        "png" => "image/png",
        "webp" => "image/webp",
        "gif" => "image/gif",
        _ => "application/octet-stream",
    }
    .to_string()
}

/// Parse a still `ref` — `<YYYY-MM-DD>/<HH>/<MM>-<SS>.<ext>` — into the timestamp and
/// channel-day-relative path [`crate::mind::memory::media::resolve`] wants, plus the
/// MIME. `None` if the shape doesn't match.
fn parse_still_ref(reff: &str) -> Option<(DateTime<Utc>, String, String)> {
    let (date, rel) = reff.split_once('/')?; // "2026-06-25", "14/23-07.jpg"
    let (hh, file) = rel.split_once('/')?; // "14", "23-07.jpg"
    let (stem, ext) = file.rsplit_once('.')?; // "23-07", "jpg"
    let (mm, ss) = stem.split_once('-')?; // "23", "07"
    // Reuse the proven RFC3339 parse (see keep_and_fade) rather than NaiveDate
    // helpers — a malformed part fails the parse and yields `None`.
    let ts = DateTime::parse_from_rfc3339(&format!("{date}T{hh}:{mm}:{ss}Z"))
        .ok()?
        .with_timezone(&Utc);
    Some((ts, rel.to_string(), ext_to_mime(ext)))
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

/// A view ref is a relative path under the views tree, naming the view's source file
/// minus the `.jsx` — e.g. `badminton-top10/leader` → `views/badminton-top10/
/// leader.jsx`. Each `/`-separated segment is a slug (letters, digits, `-`, `_`) —
/// no dots, no empty segments — so the ref stays inside the views tree and can't
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

/// Parse a `region` tool argument into a [`Region`]; unknown strings yield `None`.
fn parse_region(s: &str) -> Option<Region> {
    Some(match s {
        "center" => Region::Center,
        "top" => Region::Top,
        "bottom" => Region::Bottom,
        "left" => Region::Left,
        "right" => Region::Right,
        "top_left" => Region::TopLeft,
        "top_right" => Region::TopRight,
        "bottom_left" => Region::BottomLeft,
        "bottom_right" => Region::BottomRight,
        "fill" => Region::Fill,
        _ => return None,
    })
}

/// Resolve a view ref to its stored JSX source (and the builder's declared
/// placement, if any), read from the views tree. The agent passes only the tiny
/// ref through `show_view`; this reads the component back, plus an optional
/// `<ref>.geom.json` sidecar the builder wrote next to it. A missing or
/// unparseable sidecar is not an error — it just means the floor layout.
async fn resolve_view_ref(
    data_dir: &std::path::Path,
    view_ref: &str,
) -> Result<(String, Option<Geometry>), String> {
    let view_ref = view_ref.trim();
    if !valid_view_ref(view_ref) {
        return Err(format!("invalid ref `{view_ref}` (names and `/` only, no dots)"));
    }
    let views = data_dir.join("views");
    let source = tokio::fs::read_to_string(views.join(format!("{view_ref}.jsx")))
        .await
        .map_err(|e| format!("no such view ({e})"))?;
    let geometry = match tokio::fs::read(views.join(format!("{view_ref}.geom.json"))).await {
        Ok(bytes) => serde_json::from_slice::<Geometry>(&bytes).ok(),
        Err(_) => None,
    };
    Ok((source, geometry))
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
    async fn resolve_reads_views_source() {
        let dir = tempfile::tempdir().unwrap();
        let proj = dir.path().join("views").join("deck");
        tokio::fs::create_dir_all(&proj).await.unwrap();
        tokio::fs::write(proj.join("leader.jsx"), "export default () => 1").await.unwrap();
        let (source, geometry) = resolve_view_ref(dir.path(), "deck/leader").await.unwrap();
        assert_eq!(source, "export default () => 1");
        // No sidecar written → floor layout.
        assert!(geometry.is_none());
    }

    #[tokio::test]
    async fn resolve_reads_geometry_sidecar() {
        let dir = tempfile::tempdir().unwrap();
        let proj = dir.path().join("views").join("deck");
        tokio::fs::create_dir_all(&proj).await.unwrap();
        tokio::fs::write(proj.join("leader.jsx"), "export default () => 1").await.unwrap();
        tokio::fs::write(
            proj.join("leader.geom.json"),
            r#"{"region":"right","size":"wide"}"#,
        )
        .await
        .unwrap();
        let (_, geometry) = resolve_view_ref(dir.path(), "deck/leader").await.unwrap();
        let g = geometry.expect("sidecar geometry");
        assert_eq!(g.region, Region::Right);
        assert_eq!(g.size, crate::types::SizeClass::Wide);
        assert!(!g.owns_captions); // defaulted field absent from the sidecar
    }

    #[test]
    fn parse_region_reads_names_and_rejects_garbage() {
        assert_eq!(parse_region("center"), Some(Region::Center));
        assert_eq!(parse_region("bottom_left"), Some(Region::BottomLeft));
        assert_eq!(parse_region("fill"), Some(Region::Fill));
        assert_eq!(parse_region("middle"), None);
        assert_eq!(parse_region(""), None);
    }

    #[tokio::test]
    async fn resolve_rejects_bad_refs() {
        let dir = tempfile::tempdir().unwrap();
        assert!(resolve_view_ref(dir.path(), "../secret").await.is_err(), "traversal");
        assert!(resolve_view_ref(dir.path(), "missing/view").await.is_err(), "no file");
    }
}

#[cfg(test)]
mod name_tests {
    use super::*;

    #[tokio::test]
    async fn name_person_renames_a_cluster_to_the_name() {
        let dir = tempfile::tempdir().unwrap();
        // A clustered-but-unnamed face id with a gallery.
        people_vectors::enroll(dir.path(), "ff32ce3w", people_vectors::Modality::Face, &[1.0, 0.0], b"m", "jpg")
            .await
            .unwrap();
        let r = reflection_name_person(
            dir.path(),
            &json!({ "id": "ff32ce3w", "name": "赵力" }),
        )
        .await;
        assert_eq!(r["isError"], false);
        let got = people_vectors::nearest(dir.path(), people_vectors::Modality::Face, &[1.0, 0.0], 1)
            .await
            .unwrap();
        assert_eq!(got[0].subject, "赵力");
    }

    #[tokio::test]
    async fn name_person_renames_a_voice_cluster_too() {
        let dir = tempfile::tempdir().unwrap();
        // A clustered-but-unnamed voice id with a gallery.
        people_vectors::enroll(dir.path(), "ab12cd34", people_vectors::Modality::Voice, &[1.0, 0.0], b"m", "wav")
            .await
            .unwrap();
        let r = reflection_name_person(
            dir.path(),
            &json!({ "id": "ab12cd34", "name": "赵力" }),
        )
        .await;
        assert_eq!(r["isError"], false);
        let got = people_vectors::nearest(dir.path(), people_vectors::Modality::Voice, &[1.0, 0.0], 1)
            .await
            .unwrap();
        assert_eq!(got[0].subject, "赵力");
    }

    #[tokio::test]
    async fn merge_people_ties_a_voice_id_to_a_named_face() {
        let dir = tempfile::tempdir().unwrap();
        // 赵力 is already known by face; their voice is still a separate opaque id.
        people_vectors::enroll(dir.path(), "赵力", people_vectors::Modality::Face, &[1.0, 0.0], b"m", "jpg")
            .await
            .unwrap();
        people_vectors::enroll(dir.path(), "ab12cd34", people_vectors::Modality::Voice, &[0.0, 1.0], b"m", "wav")
            .await
            .unwrap();
        let r = reflection_merge_people(
            dir.path(),
            &json!({ "from": "ab12cd34", "into": "赵力" }),
        )
        .await;
        assert_eq!(r["isError"], false);
        // 赵力 is now recognized by BOTH senses — the cross-modal bind.
        let face = people_vectors::nearest(dir.path(), people_vectors::Modality::Face, &[1.0, 0.0], 1)
            .await
            .unwrap();
        let voice = people_vectors::nearest(dir.path(), people_vectors::Modality::Voice, &[0.0, 1.0], 1)
            .await
            .unwrap();
        assert_eq!(face[0].subject, "赵力");
        assert_eq!(voice[0].subject, "赵力");
    }

    #[tokio::test]
    async fn name_person_rejects_blank_args() {
        let dir = tempfile::tempdir().unwrap();
        assert_eq!(reflection_name_person(dir.path(), &json!({ "id": "x" })).await["isError"], true);
        assert_eq!(reflection_name_person(dir.path(), &json!({ "name": "y" })).await["isError"], true);
    }
}

#[cfg(test)]
mod screen_tool_tests {
    use super::*;
    use crate::body::capabilities::input::{Key, Modifier};

    #[test]
    fn png_dimensions_reads_ihdr() {
        let mut png = vec![0x89, b'P', b'N', b'G', 0x0D, 0x0A, 0x1A, 0x0A];
        png.extend_from_slice(&[0, 0, 0, 13]); // IHDR chunk length
        png.extend_from_slice(b"IHDR");
        png.extend_from_slice(&256u32.to_be_bytes());
        png.extend_from_slice(&128u32.to_be_bytes());
        assert_eq!(png_dimensions(&png), Some((256, 128)));
        assert_eq!(png_dimensions(b"not a png at all"), None);
        assert_eq!(png_dimensions(b"short"), None);
    }

    #[test]
    fn parse_key_handles_names_and_single_chars() {
        assert_eq!(parse_key("return"), Some(Key::Return));
        assert_eq!(parse_key("ENTER"), Some(Key::Return));
        assert_eq!(parse_key("esc"), Some(Key::Escape));
        assert_eq!(parse_key("a"), Some(Key::Char('a')));
        assert_eq!(parse_key("/"), Some(Key::Char('/')));
        assert_eq!(parse_key("f1"), None);
        assert_eq!(parse_key(""), None);
    }

    #[test]
    fn parse_mods_maps_aliases_and_drops_unknown() {
        let v = json!(["cmd", "Shift", "alt", "ctrl", "bogus"]);
        assert_eq!(
            parse_mods(Some(&v)),
            vec![Modifier::Command, Modifier::Shift, Modifier::Option, Modifier::Control]
        );
        assert_eq!(parse_mods(None), Vec::<Modifier>::new());
    }
}

#[cfg(test)]
mod vision_tool_tests {
    use super::*;

    #[test]
    fn parses_a_well_formed_still_ref() {
        let (ts, rel, mime) = parse_still_ref("2026-06-25/14/23-07.jpg").unwrap();
        assert_eq!(rel, "14/23-07.jpg");
        assert_eq!(mime, "image/jpeg");
        assert_eq!(ts.to_rfc3339(), "2026-06-25T14:23:07+00:00");
    }

    #[test]
    fn rejects_malformed_still_refs() {
        assert!(parse_still_ref("not-a-ref").is_none());
        assert!(parse_still_ref("2026-06-25/14/23.jpg").is_none(), "minute file, not a one-off still");
        assert!(parse_still_ref("2026-06-25/14/23-07").is_none(), "no extension");
    }

    #[test]
    fn ext_to_mime_covers_common_stills() {
        assert_eq!(ext_to_mime("JPG"), "image/jpeg");
        assert_eq!(ext_to_mime("png"), "image/png");
        assert_eq!(ext_to_mime("bin"), "application/octet-stream");
    }

    #[test]
    fn parse_last_secs_pulls_a_tail_length() {
        assert_eq!(parse_last_secs("last 20s"), Some(20.0));
        assert_eq!(parse_last_secs("30 seconds"), Some(30.0));
        assert_eq!(parse_last_secs("what just happened"), None);
        assert_eq!(parse_last_secs(""), None);
    }
}
