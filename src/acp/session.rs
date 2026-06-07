//! Session-level wrappers around an ACP `session/*` lifecycle.

use std::sync::Arc;

use agent_client_protocol as acp;
use acp::schema::{
    self as acp_schema, CancelNotification, ContentBlock, PromptRequest, PromptResponse,
    SessionId, StopReason, TextContent,
};
use anyhow::anyhow;
use tokio::sync::{Mutex, mpsc};
use tokio::task::JoinHandle;

use crate::acp::process::AcpProcess;

/// A single ACP session, owning the [`AcpProcess`] that hosts it.
///
/// Dropping the session drops the process, which signals shutdown and tears the
/// child down — the per-session teardown path. There is at most one in-flight
/// prompt per session.
pub struct AcpSession {
    id: SessionId,
    /// The child process this session runs on. Owned exclusively, so dropping
    /// the session closes the process; prompts are driven on its connection.
    process: AcpProcess,
    /// Wrapped in `Arc<Mutex<...>>` so [`SessionRun`] can grab the receiver
    /// for the duration of a prompt without re-creating the channel on every
    /// call. There is at most one in-flight prompt per session.
    rx: Arc<Mutex<Option<mpsc::UnboundedReceiver<SessionUpdate>>>>,
    /// Optional system prompt to prepend to the first prompt. v0 has no
    /// dedicated system-prompt slot on `session/new` — we sneak it into the
    /// initial `PromptRequest` as a leading text block.
    pending_system_prompt: Arc<Mutex<Option<String>>>,
}

/// One streaming variant we surface to callers. `from_acp` collapses the
/// schema's richer enum into what hi-agent's reactor cares about; Step 4 will
/// extend [`ToolCall`].
#[derive(Debug, Clone)]
pub enum SessionUpdate {
    /// A chunk of agent text. Concatenate to reconstruct the message.
    Text(String),
    /// A chunk of agent internal reasoning. Routers may or may not care.
    Thought(String),
    /// A tool-call notification — opaque for Step 2. Step 4 expands this.
    ToolCall(ToolCallStub),
    /// An ACP event we did not specifically model. Carries the variant name
    /// so reactors can decide to log it; we never invent shape on the wire.
    Other(String),
}

#[derive(Debug, Clone)]
pub struct ToolCallStub {
    pub raw_variant: &'static str,
}

impl SessionUpdate {
    pub(crate) fn from_acp(u: &acp_schema::SessionUpdate) -> Vec<SessionUpdate> {
        use acp_schema::SessionUpdate as U;
        match u {
            U::AgentMessageChunk(chunk) => match content_block_text(&chunk.content) {
                Some(text) => vec![SessionUpdate::Text(text)],
                None => vec![SessionUpdate::Other(
                    "agent_message_chunk:nontext".to_string(),
                )],
            },
            U::AgentThoughtChunk(chunk) => match content_block_text(&chunk.content) {
                Some(text) => vec![SessionUpdate::Thought(text)],
                None => vec![SessionUpdate::Other(
                    "agent_thought_chunk:nontext".to_string(),
                )],
            },
            U::UserMessageChunk(_) => Vec::new(),
            U::ToolCall(_) => vec![SessionUpdate::ToolCall(ToolCallStub {
                raw_variant: "tool_call",
            })],
            U::ToolCallUpdate(_) => vec![SessionUpdate::ToolCall(ToolCallStub {
                raw_variant: "tool_call_update",
            })],
            _ => vec![SessionUpdate::Other("unmodelled".to_string())],
        }
    }
}

fn content_block_text(block: &ContentBlock) -> Option<String> {
    match block {
        ContentBlock::Text(t) => Some(t.text.clone()),
        _ => None,
    }
}

/// Result returned from [`SessionRun::wait`].
#[derive(Debug, Clone)]
pub struct PromptResult {
    pub stop_reason: StopReason,
    /// All text chunks concatenated, in the order they arrived. Provided as a
    /// convenience for callers that only want the final string.
    pub text: String,
}

/// Handle for a single in-flight prompt. Stream updates via [`next_update`]
/// or block to completion with [`wait`].
pub struct SessionRun {
    rx_slot: Arc<Mutex<Option<mpsc::UnboundedReceiver<SessionUpdate>>>>,
    rx: Option<mpsc::UnboundedReceiver<SessionUpdate>>,
    pending: Option<JoinHandle<anyhow::Result<PromptResponse>>>,
    /// Cached response once `pending` resolves. Held so `wait()` can return
    /// it after `next_update()` has drained the stream.
    response: Option<PromptResponse>,
    /// Text chunks observed so far. Buffered so `wait()` can return a
    /// completed assembly without forcing the caller to also pull updates.
    text_buf: String,
}

enum RaceOutcome {
    Stream(Option<SessionUpdate>),
    Done(Result<anyhow::Result<PromptResponse>, tokio::task::JoinError>),
}

impl SessionRun {
    /// Pull the next streamed [`SessionUpdate`]. Returns `None` when the
    /// prompt has finished (the response arrived and all queued updates have
    /// been drained).
    pub async fn next_update(&mut self) -> Option<SessionUpdate> {
        // If we already have the response cached, drain any leftover updates
        // synchronously, then end the stream. ACP delivers all session/update
        // notifications for a turn before the PromptResponse, so no further
        // arrivals can be expected after the response resolves.
        if self.response.is_some() {
            let rx = self.rx.as_mut()?;
            return match rx.try_recv() {
                Ok(u) => {
                    if let SessionUpdate::Text(t) = &u {
                        self.text_buf.push_str(t);
                    }
                    Some(u)
                }
                Err(_) => None,
            };
        }

        // Race the next inbound update against prompt completion. Split out
        // the two borrows explicitly so select!'s expansion doesn't trip
        // disjoint-borrow inference.
        let (rx, pending) = match (self.rx.as_mut(), self.pending.as_mut()) {
            (Some(rx), Some(p)) => (rx, p),
            _ => return None,
        };
        let outcome = tokio::select! {
            biased;
            recv = rx.recv() => RaceOutcome::Stream(recv),
            joined = pending => RaceOutcome::Done(joined),
        };

        match outcome {
            RaceOutcome::Stream(Some(u)) => {
                if let SessionUpdate::Text(t) = &u {
                    self.text_buf.push_str(t);
                }
                Some(u)
            }
            RaceOutcome::Stream(None) => None,
            RaceOutcome::Done(joined) => {
                self.pending = None;
                match joined {
                    Ok(Ok(resp)) => {
                        self.response = Some(resp);
                    }
                    Ok(Err(e)) => {
                        tracing::warn!(error = %e, "prompt resolved with error");
                    }
                    Err(join_err) => {
                        tracing::warn!(error = %join_err, "prompt task panicked");
                    }
                }
                // Drain anything that landed in the buffer while we raced.
                let rx = self.rx.as_mut()?;
                match rx.try_recv() {
                    Ok(u) => {
                        if let SessionUpdate::Text(t) = &u {
                            self.text_buf.push_str(t);
                        }
                        Some(u)
                    }
                    Err(_) => None,
                }
            }
        }
    }

    /// Drain the stream to completion and return the final response.
    /// Consumes the handle.
    pub async fn wait(mut self) -> anyhow::Result<PromptResult> {
        // Pump next_update until the stream ends — this also waits for the
        // prompt response if it hasn't arrived yet.
        while self.next_update().await.is_some() {}

        // Park the receiver back *first* so a subsequent prompt on the same
        // session can pick up where we left off. This must precede the
        // response check: a persistent session is reused turn after turn, and
        // a prompt that finished without a response (e.g. a transport hiccup)
        // would otherwise leave the rx slot empty forever, wedging every later
        // turn with "session already has an in-flight prompt".
        if let Some(rx) = self.rx.take() {
            let mut slot = self.rx_slot.lock().await;
            *slot = Some(rx);
        }

        let response = self
            .response
            .take()
            .ok_or_else(|| anyhow!("prompt finished without a response"))?;

        Ok(PromptResult {
            stop_reason: response.stop_reason,
            text: std::mem::take(&mut self.text_buf),
        })
    }
}

impl AcpSession {
    pub(crate) fn new(
        id: SessionId,
        process: AcpProcess,
        rx: mpsc::UnboundedReceiver<SessionUpdate>,
        system_prompt: Option<String>,
    ) -> Self {
        Self {
            id,
            process,
            rx: Arc::new(Mutex::new(Some(rx))),
            pending_system_prompt: Arc::new(Mutex::new(system_prompt)),
        }
    }

    pub fn id(&self) -> &SessionId {
        &self.id
    }

    /// Send a `session/prompt` and return a streaming handle.
    pub async fn prompt(&self, text: String) -> anyhow::Result<SessionRun> {
        let mut blocks: Vec<ContentBlock> = Vec::with_capacity(2);
        {
            let mut sp = self.pending_system_prompt.lock().await;
            if let Some(prefix) = sp.take() {
                blocks.push(ContentBlock::Text(TextContent::new(prefix)));
            }
        }
        blocks.push(ContentBlock::Text(TextContent::new(text)));
        self.send_blocks(blocks).await
    }

    /// Pre-send the system prompt on its own, so the backend processes the soul
    /// (and the upstream prompt cache populates) before the first real turn —
    /// taking it off that turn's critical path. Consumes the pending system prompt
    /// exactly as the first [`prompt`] would, so the first real turn then sends
    /// only its own content. Returns `None` when there is nothing to warm — no
    /// pending system prompt, e.g. it was already sent.
    pub async fn warm(&self) -> anyhow::Result<Option<SessionRun>> {
        let prefix = self.pending_system_prompt.lock().await.take();
        match prefix {
            Some(prefix) => {
                let blocks = vec![ContentBlock::Text(TextContent::new(prefix))];
                Ok(Some(self.send_blocks(blocks).await?))
            }
            None => Ok(None),
        }
    }

    /// Dispatch a `session/prompt` carrying `blocks` and wrap its stream in a
    /// [`SessionRun`]. Shared by [`prompt`] and [`warm`].
    async fn send_blocks(&self, blocks: Vec<ContentBlock>) -> anyhow::Result<SessionRun> {
        let rx = {
            let mut slot = self.rx.lock().await;
            slot.take()
                .ok_or_else(|| anyhow!("session already has an in-flight prompt"))?
        };

        let req = PromptRequest::new(self.id.clone(), blocks);
        let connection = self.process.connection().clone();
        let pending: JoinHandle<anyhow::Result<PromptResponse>> = tokio::spawn(async move {
            connection
                .send_request(req)
                .block_task()
                .await
                .map_err(|e| anyhow!("session/prompt failed: {e}"))
        });

        Ok(SessionRun {
            rx_slot: self.rx.clone(),
            rx: Some(rx),
            pending: Some(pending),
            response: None,
            text_buf: String::new(),
        })
    }

    /// Send `session/cancel`. The in-flight prompt will resolve with
    /// `StopReason::Cancelled` per ACP.
    pub async fn cancel(&self) -> anyhow::Result<()> {
        self.process
            .connection()
            .send_notification(CancelNotification::new(self.id.clone()))
            .map_err(|e| anyhow!("session/cancel failed: {e}"))?;
        Ok(())
    }
}
