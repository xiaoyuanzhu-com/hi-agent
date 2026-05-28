//! ACP back-end — talks to `claude-code` (or any ACP-speaking agent) over stdio.
//!
//! The unit of life here is a single [`AcpProcess`]: one child process held for
//! the lifetime of hi-agent, hosting many concurrent [`AcpSession`]s. Step 2
//! intentionally stays just below the reactor — anything that routes signals
//! to specific peers or workers belongs in Step 3/Step 5.
//!
//! Streaming model: `session/update` notifications fan out by `SessionId`
//! through a routing table held inside the process driver. Each session owns
//! one `mpsc` receiver and pulls updates from it via [`SessionRun::next_update`]
//! while the prompt request resolves in the background.

pub mod process;
pub mod session;

pub use process::{
    AcpProcess, ApprovalBridge, ApprovalOutcome as AcpApprovalOutcome, McpServerCfg, SessionOpts,
};
pub use session::{AcpSession, PromptResult, SessionRun, SessionUpdate};
