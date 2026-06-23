//! ACP back-end — talks to `claude-code` (or any ACP-speaking agent) over stdio.
//!
//! One [`AcpProcess`] holds a child process and a long-lived JSON-RPC
//! connection, hosting exactly **one** [`AcpSession`]. The session owns its
//! process (drop tears it down) and one `mpsc` of streaming `SessionUpdate`s;
//! the reactor pulls from it while the prompt resolves in the background.
//! Isolation and concurrency come from running a process per session, so there
//! is no `session_id` demux.

pub mod process;
pub mod session;
pub mod tap;

pub use process::{AcpProcess, ProcessRegistry, SessionOpts};
pub use session::{AcpSession, PromptResult, SessionRun, SessionUpdate};
pub use tap::{AcpTap, Dir, RawFrame};
