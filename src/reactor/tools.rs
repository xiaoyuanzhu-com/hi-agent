//! The bridge between the MCP tool server and each scene's reactor loop.
//!
//! The mind (and its workers) express side-effects as MCP tool calls over the
//! `/mcp` HTTP endpoint (see [`crate::mcp`]). Those calls arrive on a different
//! task than the per-scene loop, so they cannot touch the loop's private state
//! directly. Instead each scene registers a [`ToolSink`] — a control-channel
//! sender — into a shared [`ToolRegistry`] keyed by scene. The MCP handler looks
//! the sink up by the call's `X-HI-Scene` header and forwards a [`SceneControl`]
//! the loop applies on its own turn, so worker-registry and alarm state stay
//! owned by the loop with no locking.

use std::collections::HashMap;
use std::sync::Arc;

use tokio::sync::{Mutex, mpsc};

use crate::types::Scene;

/// One command the MCP tool server routes to a scene's reactor loop. Delegate and
/// alarm are pure side-effects applied without a spoken turn; a worker `ask`
/// becomes a question report the loop folds into its next turn (fix-forward — the
/// worker never waits on an answer).
#[derive(Debug)]
pub enum SceneControl {
    /// Spawn a working session for `task` (the `delegate` tool).
    Delegate { task: String },
    /// Schedule a self-wake after `delay` (e.g. `30s`, `20m`, `1h`) carrying
    /// `note` (the `alarm` tool). The delay is parsed loop-side; an unparseable
    /// one is dropped.
    Alarm { delay: String, note: String },
    /// A working session raised a non-blocking question (the `ask` tool); `id`
    /// names the worker so the loop can attribute it to its task.
    WorkerAsk { id: u64, question: String },
}

/// Per-scene handle the MCP handler dispatches to. Cheap to clone; holds only the
/// control-channel sender.
#[derive(Clone)]
pub struct ToolSink {
    pub(super) control: mpsc::Sender<SceneControl>,
}

impl ToolSink {
    /// Forward one control command to the scene loop. Returns an error only if
    /// the loop is gone (channel closed).
    pub async fn send(&self, control: SceneControl) -> anyhow::Result<()> {
        self.control
            .send(control)
            .await
            .map_err(|_| anyhow::anyhow!("scene loop gone; control dropped"))
    }
}

/// Shared scene→sink table. Created once in `lib.rs`, shared (cloneable handle)
/// between the HTTP front's `/mcp` handler and the reactor that registers sinks.
#[derive(Clone, Default)]
pub struct ToolRegistry {
    inner: Arc<Mutex<HashMap<Scene, ToolSink>>>,
}

impl ToolRegistry {
    pub fn new() -> Self {
        Self::default()
    }

    /// Register (or replace) a scene's sink. Called when the per-scene loop is
    /// created, before its session opens and can issue any tool call.
    pub async fn register(&self, scene: Scene, sink: ToolSink) {
        self.inner.lock().await.insert(scene, sink);
    }

    /// Look a scene's sink up by its `X-HI-Scene` header. `None` if no loop is
    /// registered for it (e.g. a stale or unknown scene).
    pub async fn get(&self, scene: &Scene) -> Option<ToolSink> {
        self.inner.lock().await.get(scene).cloned()
    }
}
