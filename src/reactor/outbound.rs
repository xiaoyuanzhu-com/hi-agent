//! The reactor's outbound vocabulary — continuous channel signals, transport-free.
//!
//! The reactor is the mind; it must stay aligned to the human-channel model and
//! know nothing about whichever wire happens to carry it. So instead of building
//! HTTP-shaped events, it emits [`OutboundSignal`]s: "said this text", "this span
//! of speech", "show this surface". A transport adapter (today the HTTP server)
//! binds these to a wire — assigns `Content-Type`, frames one utterance into one
//! response, closes the body at an utterance boundary. Swap HTTP for WebSocket
//! and only the adapter changes; this vocabulary and the reactor are untouched.
//!
//! What is deliberately *absent* here is the tell: no `mime`/`Content-Type`, no
//! HTTP response framing, no body-close semantics. The one integer that remains,
//! `turn`, is the reactor's own cognition-turn id (it already tags journal and
//! logs); the adapter reuses it to keep one utterance's audio frames bound to one
//! response, but the reactor does not reason about responses.

use bytes::Bytes;

use crate::types::{Scene, SurfaceEnvelope, ViewEnvelope};

/// One continuous outbound signal on a channel, addressed to a scene. The
/// reactor's entire output surface in human-channel terms.
#[derive(Debug, Clone)]
pub enum OutboundSignal {
    /// A chunk of agent text on the /thought channel. Concatenate a scene's
    /// chunks between [`TextEnd`](OutboundSignal::TextEnd)s to get one utterance.
    Text { scene: Scene, chunk: String },
    /// The boundary that ends one continuous /thought utterance.
    TextEnd { scene: Scene },
    /// A span of synthesized speech begins; `codec` names the audio format
    /// (e.g. `audio/mpeg`). `turn` correlates this span's frames so the adapter
    /// can hold one response open for exactly one utterance.
    AudioBegin { scene: Scene, turn: u64, codec: String },
    /// One frame of synthesized speech within the open span.
    AudioFrame { scene: Scene, turn: u64, bytes: Bytes },
    /// The span of speech ends (synthesis finished, or the turn was cut short).
    AudioEnd { scene: Scene, turn: u64 },
    /// A rich-content surface to show on the /surface channel.
    Surface { scene: Scene, envelope: SurfaceEnvelope },
    /// An agent-authored view module to mount on the /view channel. `envelope`
    /// carries the compiled module URL; the binder broadcasts it to GET
    /// /api/out/view subscribers.
    View { scene: Scene, envelope: ViewEnvelope },
}
