//! Public types — spec primitives plus journal records.

use std::fmt;
use std::str::FromStr;

use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};
use thiserror::Error;

// -----------------------------------------------------------------------------
// Scene
// -----------------------------------------------------------------------------

/// The situation a signal belongs to, carried by `X-HI-Scene` — a 1:1, a group,
/// or being alone doing something, e.g. `alice@phone`. It is the unit of context
/// isolation (one reactor session / journal slice / subprocess per scene); the
/// human who spoke is soft, inferred content, not part of this key.
#[derive(Debug, Clone, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(transparent)]
pub struct Scene(pub String);

impl fmt::Display for Scene {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&self.0)
    }
}

#[derive(Debug, Error)]
#[error("scene id may not be empty")]
pub struct SceneParseError;

impl FromStr for Scene {
    type Err = SceneParseError;

    fn from_str(s: &str) -> Result<Self, Self::Err> {
        if s.is_empty() {
            Err(SceneParseError)
        } else {
            Ok(Scene(s.to_owned()))
        }
    }
}

// -----------------------------------------------------------------------------
// Channel — the six spec channels
// -----------------------------------------------------------------------------

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Channel {
    /// The text channel — typed input and the agent's worded replies. `alias`
    /// keeps journals written before the thought→text rename loadable.
    #[serde(alias = "thought")]
    Text,
    Vision,
    Audio,
    Touch,
    Smell,
    Taste,
}

impl Channel {
    pub fn as_str(self) -> &'static str {
        match self {
            Channel::Text => "text",
            Channel::Vision => "vision",
            Channel::Audio => "audio",
            Channel::Touch => "touch",
            Channel::Smell => "smell",
            Channel::Taste => "taste",
        }
    }

    /// The channel's textual form for a prompt/journal line, suffixed with a
    /// `#stream` label when the signal came from a named stream within the scene
    /// (`audio#webcam`). The default stream (`None`) renders bare (`audio`), so
    /// single-stream output stays identical. The `#` notation lives only here.
    pub fn with_stream(self, stream: Option<&str>) -> String {
        match stream {
            Some(s) => format!("{}#{s}", self.as_str()),
            None => self.as_str().to_owned(),
        }
    }
}

impl fmt::Display for Channel {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(self.as_str())
    }
}

#[derive(Debug, Error)]
#[error("unknown channel: {0}")]
pub struct ChannelParseError(pub String);

impl FromStr for Channel {
    type Err = ChannelParseError;

    fn from_str(s: &str) -> Result<Self, Self::Err> {
        match s {
            "text" | "thought" => Ok(Channel::Text),
            "vision" => Ok(Channel::Vision),
            "audio" => Ok(Channel::Audio),
            "touch" => Ok(Channel::Touch),
            "smell" => Ok(Channel::Smell),
            "taste" => Ok(Channel::Taste),
            other => Err(ChannelParseError(other.to_owned())),
        }
    }
}

// -----------------------------------------------------------------------------
// Signal — one utterance on one channel
// -----------------------------------------------------------------------------

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Signal {
    pub channel: Channel,
    pub scene: Scene,
    pub body: String,
    /// The named stream this signal arrived on within the scene (`webcam`,
    /// `headset`), or `None` for the scene's default stream. Carried so the
    /// reactor can tell concurrent sources of one channel apart.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub stream: Option<String>,
    pub ts: DateTime<Utc>,
}

// -----------------------------------------------------------------------------
// JournalEntry — the discriminated union written to journal.jsonl
// -----------------------------------------------------------------------------

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum JournalEntry {
    SignalIn {
        ts: DateTime<Utc>,
        channel: Channel,
        // `alias = "from"` keeps journals written before the X-HI-Scene rename
        // (which stored the sender as `from`) loadable on cold start.
        #[serde(alias = "from")]
        scene: Scene,
        body: String,
        /// Named stream within the scene this signal arrived on, or absent for
        /// the default stream. Old journals (no key) load as `None`, and
        /// default-stream entries omit the key entirely, so existing lines stay
        /// byte-identical — no migration.
        #[serde(default, skip_serializing_if = "Option::is_none")]
        stream: Option<String>,
        /// Stable file reference for non-text bodies (audio bytes, future
        /// images). `body` stays the text representation (e.g. STT transcript).
        #[serde(default)]
        media_path: Option<String>,
    },
    SignalOut {
        ts: DateTime<Utc>,
        channel: Channel,
        // `alias = "to"` keeps pre-rename journals (which stored the recipient
        // as `to`) loadable.
        #[serde(alias = "to")]
        scene: Scene,
        body: String,
        /// For outbound audio: where the rendered bytes live.
        #[serde(default)]
        media_path: Option<String>,
    },
}

// -----------------------------------------------------------------------------
// ViewEnvelope — outbound agent-authored view module for the UI view slot
// -----------------------------------------------------------------------------

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ViewOp {
    /// Mount a new view under `id`.
    Show,
    /// Swap the module mounted under an existing `id` in place. Reusing the id
    /// is the continuity lever — the client keeps the slot, so a `motion`-tagged
    /// element animates rather than popping.
    Replace,
    /// Remove the view mounted under `id`.
    Dismiss,
}

/// One view event delivered to the browser over GET /api/out/view. `module_url`
/// points at the compiled ESM module (`/generated/views/<hash>.mjs`) the client
/// dynamically imports and mounts under `id` in the view slot. For
/// `op = dismiss` only `id` is meaningful.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ViewEnvelope {
    pub id: String,
    pub op: ViewOp,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub module_url: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub ttl_ms: Option<u64>,
}
