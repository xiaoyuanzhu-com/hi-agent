//! Public types — the spec primitives plus journal/intent records.
//!
//! These signatures are the contract that the rest of the codebase (and the
//! subsequent implementation steps) build against. Treat them as stable.

use std::fmt;
use std::str::FromStr;

use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};
use thiserror::Error;
use uuid::Uuid;

// -----------------------------------------------------------------------------
// PeerId
// -----------------------------------------------------------------------------

/// A peer identifier carried by `X-HI-From` / `X-HI-To`, e.g. `alice@phone`.
#[derive(Debug, Clone, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(transparent)]
pub struct PeerId(pub String);

impl fmt::Display for PeerId {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&self.0)
    }
}

#[derive(Debug, Error)]
#[error("peer id may not be empty")]
pub struct PeerIdParseError;

impl FromStr for PeerId {
    type Err = PeerIdParseError;

    fn from_str(s: &str) -> Result<Self, Self::Err> {
        if s.is_empty() {
            Err(PeerIdParseError)
        } else {
            Ok(PeerId(s.to_owned()))
        }
    }
}

// -----------------------------------------------------------------------------
// Channel
// -----------------------------------------------------------------------------

/// The eight spec channels. Serialized lowercase.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Channel {
    Thought,
    Vision,
    Audio,
    Touch,
    Smell,
    Taste,
    Approval,
    Intent,
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
            Channel::Approval => "approval",
            Channel::Intent => "intent",
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
            "approval" => Ok(Channel::Approval),
            "intent" => Ok(Channel::Intent),
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
    pub from: PeerId,
    pub to: Option<PeerId>,
    pub body: String,
    pub ts: DateTime<Utc>,
}

// -----------------------------------------------------------------------------
// Stable IDs — UUIDv7, serialize as string
// -----------------------------------------------------------------------------

macro_rules! define_id {
    ($name:ident) => {
        #[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
        #[serde(transparent)]
        pub struct $name(pub Uuid);

        impl $name {
            pub fn new() -> Self {
                Self(Uuid::now_v7())
            }
        }

        impl Default for $name {
            fn default() -> Self {
                Self::new()
            }
        }

        impl fmt::Display for $name {
            fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
                self.0.fmt(f)
            }
        }

        impl FromStr for $name {
            type Err = uuid::Error;

            fn from_str(s: &str) -> Result<Self, Self::Err> {
                Uuid::parse_str(s).map(Self)
            }
        }
    };
}

define_id!(WorkerId);
define_id!(IntentId);
define_id!(ApprovalId);

// -----------------------------------------------------------------------------
// IntentTrigger
// -----------------------------------------------------------------------------

/// When a deferred intention should fire. Only `Absolute` is implemented in v0.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum IntentTrigger {
    Absolute { ts: DateTime<Utc> },
    // Cron and Relative are deferred to v0.1.
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Intent {
    pub id: IntentId,
    pub created: DateTime<Utc>,
    pub peer: PeerId,
    pub when: IntentTrigger,
    pub what: String,
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
        from: PeerId,
        body: String,
    },
    SignalOut {
        ts: DateTime<Utc>,
        channel: Channel,
        to: PeerId,
        body: String,
    },
    WorkerSpawn {
        ts: DateTime<Utc>,
        id: WorkerId,
        peer: PeerId,
        brief: String,
    },
    WorkerCancel {
        ts: DateTime<Utc>,
        id: WorkerId,
    },
    WorkerComplete {
        ts: DateTime<Utc>,
        id: WorkerId,
    },
    ApprovalRequest {
        ts: DateTime<Utc>,
        id: ApprovalId,
        peer: PeerId,
        action: String,
        summary: String,
        details: serde_json::Value,
    },
    ApprovalDecision {
        ts: DateTime<Utc>,
        id: ApprovalId,
        allow: bool,
        reason: Option<String>,
    },
    ApprovalExpired {
        ts: DateTime<Utc>,
        id: ApprovalId,
    },
    IntentSet {
        ts: DateTime<Utc>,
        id: IntentId,
        peer: PeerId,
        when: IntentTrigger,
        what: String,
    },
    IntentFired {
        ts: DateTime<Utc>,
        id: IntentId,
    },
    Note {
        ts: DateTime<Utc>,
        peer: Option<PeerId>,
        content: String,
    },
}
