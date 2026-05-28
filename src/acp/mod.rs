//! ACP back-end — talks to `claude-code` (or any ACP-speaking agent) over stdio.
//!
//! One [`AcpProcess`] holds the child process and a long-lived JSON-RPC
//! connection. Many concurrent [`AcpSession`]s live inside it. Each session
//! owns one `mpsc` of streaming `SessionUpdate`s; the reactor pulls from it
//! while the prompt resolves in the background.

pub mod process;
pub mod session;

pub use process::{AcpProcess, SessionOpts};
pub use session::{AcpSession, PromptResult, SessionRun, SessionUpdate};
