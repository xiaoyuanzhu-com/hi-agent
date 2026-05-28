//! Heartbeat — the agent's "alive" loop.
//!
//! Ticks once per second. On each tick:
//! 1. Asks the intent store for due intents (now >= trigger.ts for Absolute).
//! 2. For each due intent: injects a synthetic /intent signal into the peer's
//!    routing path via the reactor, journals IntentFired, removes the intent
//!    from the active set.
//!
//! Per impl.md § Aliveness — Heartbeat: routing decides whether/how to act
//! on the synthetic signal. We never emit directly; the router catches the
//! "already-happened" case and stays silent.

use std::time::Duration;

use chrono::Utc;
use tokio::time::{MissedTickBehavior, interval};

use crate::memory::Memory;
use crate::reactor::Reactor;
use crate::types::{Channel, JournalEntry};

const TICK: Duration = Duration::from_secs(1);

/// Spawn the heartbeat task. Runs for the lifetime of the process.
pub fn start(memory: Memory, reactor: Reactor) {
    tokio::spawn(async move {
        let mut tick = interval(TICK);
        // Never burst-fire backlogged ticks after a stall.
        tick.set_missed_tick_behavior(MissedTickBehavior::Skip);
        loop {
            tick.tick().await;
            if let Err(e) = run_tick(&memory, &reactor).await {
                tracing::warn!(error = ?e, "heartbeat tick failed");
            }
        }
    });
}

async fn run_tick(memory: &Memory, reactor: &Reactor) -> anyhow::Result<()> {
    let now = Utc::now();
    let due = memory.intents.due_intents(now).await;
    if due.is_empty() {
        return Ok(());
    }

    for intent in due {
        tracing::info!(
            id = %intent.id,
            peer = %intent.peer,
            what = %intent.what,
            "intent due; injecting synthetic signal",
        );

        // Inject via the reactor — it journals SignalIn and routes through
        // the peer's queue. The router decides whether/how to emit.
        if let Err(e) = reactor
            .inject_synthetic_signal(intent.peer.clone(), Channel::Intent, intent.what.clone())
            .await
        {
            tracing::warn!(id = %intent.id, error = ?e, "failed to inject synthetic signal");
            // Do NOT remove on failure — retry next tick.
            continue;
        }

        if let Err(e) = memory
            .journal
            .append(JournalEntry::IntentFired {
                ts: Utc::now(),
                id: intent.id,
            })
            .await
        {
            tracing::warn!(id = %intent.id, error = ?e, "failed to journal IntentFired");
        }
        if let Err(e) = memory.intents.remove(&intent.id).await {
            tracing::warn!(id = %intent.id, error = ?e, "failed to remove fired intent");
        }
    }

    Ok(())
}
