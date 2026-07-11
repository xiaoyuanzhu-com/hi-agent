//! A process-wide shutdown signal: one `trigger`, many observers.
//!
//! Cloneable and cheap. Once triggered it stays triggered (level-triggered), so
//! a late observer still sees it. Two ways to observe it:
//! - [`Shutdown::is_triggered`] — a synchronous level check, for hot paths that
//!   must not spawn fresh work mid-shutdown (e.g. the reactor's retry loop, before
//!   it would restart an ACP session into a dying process group).
//! - [`Shutdown::cancelled`] — an await point for `select!` arms, so an idle loop
//!   wakes and winds down promptly instead of holding the process open.
//!
//! Built on tokio's `Notify` plus an `AtomicBool` rather than pulling in
//! `CancellationToken`, and ordered so it is lost-wakeup-safe: a `trigger` that
//! races a task about to await is caught either by the flag check before the wait
//! or by the notification after it (see [`Shutdown::cancelled`]).

use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};

use tokio::sync::Notify;

#[derive(Clone, Default)]
pub struct Shutdown {
    inner: Arc<Inner>,
}

#[derive(Default)]
struct Inner {
    triggered: AtomicBool,
    notify: Notify,
}

impl Shutdown {
    pub fn new() -> Self {
        Self::default()
    }

    /// Enter the triggered state and wake everyone awaiting [`Shutdown::cancelled`].
    /// Idempotent — later calls are no-ops beyond a redundant wake.
    pub fn trigger(&self) {
        self.inner.triggered.store(true, Ordering::SeqCst);
        self.inner.notify.notify_waiters();
    }

    /// Whether [`Shutdown::trigger`] has fired. Cheap; safe to poll in a tight loop.
    pub fn is_triggered(&self) -> bool {
        self.inner.triggered.load(Ordering::SeqCst)
    }

    /// Resolve once the signal is (or becomes) triggered. Returns immediately if
    /// already triggered; otherwise parks until the next `trigger`. Cancel-safe,
    /// so it composes as a `select!` arm.
    pub async fn cancelled(&self) {
        // Register the waiter *before* the flag check. `notify_waiters` only wakes
        // waiters already registered when it runs, so enabling first closes the
        // race: if `trigger` lands after this `enable`, our waiter is woken; if it
        // landed before, the store it published (SeqCst) is visible to the load
        // below and we return without waiting.
        let notified = self.inner.notify.notified();
        tokio::pin!(notified);
        notified.as_mut().enable();
        if self.is_triggered() {
            return;
        }
        notified.await;
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::time::Duration;

    #[tokio::test]
    async fn cancelled_returns_immediately_when_already_triggered() {
        let s = Shutdown::new();
        s.trigger();
        assert!(s.is_triggered());
        // Would hang if it parked instead of observing the flag.
        tokio::time::timeout(Duration::from_secs(1), s.cancelled())
            .await
            .expect("cancelled should return at once when already triggered");
    }

    #[tokio::test]
    async fn cancelled_wakes_on_later_trigger() {
        let s = Shutdown::new();
        let waiter = s.clone();
        let h = tokio::spawn(async move { waiter.cancelled().await });
        // Give the task a moment to park on `cancelled`, then trigger.
        tokio::time::sleep(Duration::from_millis(20)).await;
        assert!(!s.is_triggered());
        s.trigger();
        tokio::time::timeout(Duration::from_secs(1), h)
            .await
            .expect("waiter should wake on trigger")
            .expect("waiter task should not panic");
    }

    #[tokio::test]
    async fn trigger_is_idempotent() {
        let s = Shutdown::new();
        s.trigger();
        s.trigger();
        assert!(s.is_triggered());
    }
}
