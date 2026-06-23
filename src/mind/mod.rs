//! mind — what the agent knows and remembers.
//!
//! The agent-grown store, peer to `identity` (who it is) and the factory-sealed
//! `body`/`foundation`. Two parts:
//! - `memory` — the lossless raw signal journal plus the derived episodes/facets
//!   and the recency projection; the agent's autobiographical + semantic record.
//! - `views` — agent-authored views (learned, presentational memory) and the
//!   compiler that turns their source into served ESM.
//!
//! Reads are the worker grepping the tree; writes flow through these stores'
//! APIs. The provenance-tagged, seed-shadowing write *port* described in
//! `docs/arch.md` is future work — for now this module is the faculty's home and
//! namespace, not yet a gate.

pub mod memory;
pub mod views;
