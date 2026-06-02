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
    Thought,
    Vision,
    Audio,
    Touch,
    Smell,
    Taste,
}

impl Channel {
    pub fn as_str(self) -> &'static str {
        match self {
            Channel::Thought => "thought",
            Channel::Vision => "vision",
            Channel::Audio => "audio",
            Channel::Touch => "touch",
            Channel::Smell => "smell",
            Channel::Taste => "taste",
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
            "thought" => Ok(Channel::Thought),
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
// SurfaceEnvelope — outbound rich-content block for the UI overlay
// -----------------------------------------------------------------------------

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum SurfaceOp {
    Show,
    Dismiss,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum SurfaceMode {
    Card,
    Full,
}

/// One rich-content event delivered to the browser over GET /surface. `html` is
/// agent-authored and rendered inside a sandboxed iframe; `mode` chooses the
/// overlay presentation. For `op = dismiss` only `id` is meaningful.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SurfaceEnvelope {
    pub id: String,
    pub op: SurfaceOp,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub mode: Option<SurfaceMode>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub html: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub ttl_ms: Option<u64>,
}
