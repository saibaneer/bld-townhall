#![forbid(unsafe_code)]

//! Conversation routing: the layer that decides WHICH move to make, reaching a
//! booking only through the gateway's socket.
//!
//! Spec §3.2 marks the orchestrator "may mutate authoritative booking state?
//! No", and the crate graph holds it to that: no `townhall-service`, no
//! `townhall-store`, no `bld-kernel`, no `sqlx` (see `tests/boundary.rs`). The
//! dispatcher is deterministic; the one probabilistic seat is the [`Proposer`]
//! port, whose M6 occupant is a strict grammar and whose M11 occupant is a
//! model — the seat's shape does not change between them.
//!
//! # The ordering that is the contract
//!
//! Channel controls answer from ports before the proposer is consulted or the
//! wire is touched (§15.1, Appendix B). STOP gates the convergence follow-up
//! *turn*, not just its message. Every proposal path reloads through the wire —
//! session memory holds ids and nothing else, so a stale-version bug has
//! nothing to be built from.

pub mod dispatcher;
pub mod journey;
pub mod ports;
pub mod scripted;
pub mod suppression;

pub use dispatcher::Dispatcher;
pub use ports::{
    BookingRequest, BookingWire, CandidateSummary, CredentialSource, GatewayFactory, NoLedgerYet,
    PrincipalDirectory, ProjectedContext, Proposed, Proposer, Request, UsageBalance, WireFactory,
};
pub use scripted::ScriptedProposer;
pub use suppression::FileSuppression;
