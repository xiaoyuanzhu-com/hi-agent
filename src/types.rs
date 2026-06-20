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
    /// Handed artifacts — a file the user gives the agent (a contract, a passport
    /// scan), received by reference through an upload carrier. NOT a sense: the
    /// agent doesn't *perceive* a file, it is *handed* one; the bytes are kept
    /// verbatim and the signal says who handed over what.
    File,
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
            Channel::File => "file",
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
            "file" => Ok(Channel::File),
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
// Origin — which mind produced a signal
// -----------------------------------------------------------------------------

/// Mechanical provenance: which mind produced a signal. NOT the speaker's
/// identity (that stays soft, inferred from content). Inbound human signals are
/// `Human`, the reactor's own articulation is `Reactor`, and delegated workers
/// (once they journal) are `Worker`.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Origin {
    Human,
    Reactor,
    Worker,
}

// -----------------------------------------------------------------------------
// Media — the multimodal payload a signal carries (audio bytes, image, …)
// -----------------------------------------------------------------------------

/// A signal's media payload. The bytes live inside the signal's channel-day
/// folder on a wall-clock grid; this records the path (relative to that folder)
/// plus enough metadata that a reader needn't open the bytes to know what they
/// are. The signal's `body` stays the text surface (an STT transcript, a
/// caption).
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Media {
    /// Path relative to the signal's channel-date folder, e.g. `09/16-45.mp3`
    /// (a one-off) or `output/09/11.mp3` (a streamed output minute).
    pub file: String,
    pub mime: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub duration_ms: Option<u64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub width: Option<u32>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub height: Option<u32>,
}

// -----------------------------------------------------------------------------
// JournalEntry — the discriminated union written to each scene's day-log
// -----------------------------------------------------------------------------

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum JournalEntry {
    SignalIn {
        /// Stable, time-sortable id (uuidv7): the cursor + citation key, and the
        /// stem of any co-located media blob (`audio-<id>.mp3`).
        id: String,
        ts: DateTime<Utc>,
        channel: Channel,
        #[serde(alias = "from")]
        scene: Scene,
        body: String,
        /// Named stream within the scene this signal arrived on, or absent for
        /// the default stream.
        #[serde(default, skip_serializing_if = "Option::is_none")]
        stream: Option<String>,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        media: Option<Media>,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        origin: Option<Origin>,
    },
    SignalOut {
        id: String,
        ts: DateTime<Utc>,
        channel: Channel,
        #[serde(alias = "to")]
        scene: Scene,
        body: String,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        media: Option<Media>,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        origin: Option<Origin>,
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
/// points at the compiled ESM module (`/views/_compiled/<hash>.mjs`) the client
/// dynamically imports and mounts under `id` in the view slot. For
/// `op = dismiss` only `id` is meaningful. A view persists until the agent
/// dismisses (or replaces) it — there is no auto-expiry; lifetime is the
/// reactor's decision.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ViewEnvelope {
    pub id: String,
    pub op: ViewOp,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub module_url: Option<String>,
}
