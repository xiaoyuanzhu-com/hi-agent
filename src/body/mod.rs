//! body — the always-on apparatus the developer builds so the agent can perceive
//! and act, plus the loops that keep it running.
//!
//! Factory-sealed code, peer to the agent-grown `identity`/`mind` and the engine
//! `foundation`. Holds:
//! - `capabilities` — the senses and actions (STT/TTS, vision, input, screen, …),
//!   over the vendor adapters in `crate::foundation::vendors`.
//! - `reactor` — the per-scene loops: turn-taking, the pulse and reflection clocks,
//!   the loader/assembler, the output sequencer, and delegated workers.
//! - `reflex` — taught teach-and-fire quick actions that run with no model in the
//!   loop (the cerebellum fast-path).
//! - `presence` — who/what is attached to a scene right now.
//! - `gesture` — the desktop attention gestures (come-and-see, press-and-hold-⌘).
//!
//! Whether a piece runs continuously (a sense, a clock) or is invoked on demand
//! (a tool) is wiring, not a sub-module boundary — see docs/arch.md.

pub mod capabilities;
pub mod gesture;
pub mod presence;
pub mod reactor;
pub mod reflex;
