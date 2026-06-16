//! Uniform per-channel I/O logging.
//!
//! Every signal that enters or leaves the agent through a channel is logged
//! here at INFO under `target = "channel"`, in one consistent shape, so the
//! whole conversation — text in, text out, spoken audio, views — is visible
//! as a single filterable stream (`RUST_LOG=channel=info`). This complements
//! the journal (which persists `SignalIn`/`SignalOut`): the journal is the
//! durable record, these logs are the live tap for debugging what's flowing.

use std::borrow::Cow;

use crate::types::{Channel, Scene};

/// Longest body logged inline; longer text is clipped so a big reply or a
/// view's HTML doesn't flood one line. Clipped on a char boundary.
const MAX_BODY: usize = 2000;

/// A signal arriving in a scene on `channel` (user → agent).
pub fn inbound(channel: Channel, scene: &Scene, body: &str) {
    tracing::info!(
        target: "channel",
        dir = "in",
        channel = %channel,
        scene = %scene,
        body = %clip(body),
        "channel in",
    );
}

/// A signal the agent emits into a scene on `channel` (agent → user).
pub fn outbound(channel: Channel, scene: &Scene, body: &str) {
    tracing::info!(
        target: "channel",
        dir = "out",
        channel = %channel,
        scene = %scene,
        body = %clip(body),
        "channel out",
    );
}

fn clip(s: &str) -> Cow<'_, str> {
    if s.len() <= MAX_BODY {
        return Cow::Borrowed(s);
    }
    let mut end = MAX_BODY;
    while !s.is_char_boundary(end) {
        end -= 1;
    }
    Cow::Owned(format!("{}…", &s[..end]))
}
