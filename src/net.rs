//! Shared HTTP download policy for the managed provisioners.
//!
//! Every first-run / package-time download — the Node runtime, the recognition
//! models, the static ffmpeg — goes through one client configured with a **read
//! (idle) timeout** rather than a total timeout. A large model on a slow link
//! must be free to take as long as it needs *while bytes keep arriving*; but a
//! connection that stalls mid-stream must fail fast instead of hanging the whole
//! provision forever (which it did before this existed — a `make dmg` once hung
//! on a half-finished model). [`with_retries`] then lets a transient stall
//! self-heal without re-running the build.

use std::future::Future;
use std::time::Duration;

/// Time to establish the TCP/TLS connection before giving up.
const CONNECT_TIMEOUT: Duration = Duration::from_secs(20);
/// Max time a download may go idle (no bytes received) before it is treated as
/// stalled. `read_timeout` resets on every successful read, so this bounds
/// *stalls*, not total duration — a slow-but-progressing download is never cut off.
const READ_TIMEOUT: Duration = Duration::from_secs(30);
/// How many times a download is attempted before the error propagates.
const MAX_ATTEMPTS: u32 = 3;

/// A reqwest client carrying the shared connect + read (idle) timeouts. Callers
/// that issue several requests should build one and reuse it (clones share a
/// connection pool).
pub fn http_client() -> reqwest::Client {
    reqwest::Client::builder()
        .connect_timeout(CONNECT_TIMEOUT)
        .read_timeout(READ_TIMEOUT)
        .build()
        .expect("reqwest client with static timeouts is infallible to build")
}

/// Run an async download `op` up to [`MAX_ATTEMPTS`] times with linear backoff,
/// returning the first success or the last error. Each attempt re-runs `op` from
/// scratch — downloads are not resumed — so `op` must (re)create its own temp
/// output. Our `download_verify`s satisfy this: they `File::create` the temp,
/// which truncates any partial bytes a previous attempt left behind. `label`
/// only tags the warning log.
pub async fn with_retries<T, F, Fut>(label: &str, mut op: F) -> anyhow::Result<T>
where
    F: FnMut() -> Fut,
    Fut: Future<Output = anyhow::Result<T>>,
{
    let mut last: Option<anyhow::Error> = None;
    for attempt in 1..=MAX_ATTEMPTS {
        match op().await {
            Ok(v) => return Ok(v),
            Err(e) => {
                tracing::warn!(label, attempt, max = MAX_ATTEMPTS, error = %e, "download attempt failed");
                last = Some(e);
                if attempt < MAX_ATTEMPTS {
                    tokio::time::sleep(Duration::from_secs(2 * attempt as u64)).await;
                }
            }
        }
    }
    Err(last.expect("the loop runs at least once"))
}
