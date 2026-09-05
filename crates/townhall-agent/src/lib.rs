#![forbid(unsafe_code)]

//! The untrusted proposer (M11, ADR-031) — the "Rig Agent Runtime" of the spec's
//! Figure 1, one hop before `bld-client`.
//!
//! Everything here lives OUTSIDE the BLD authority boundary. A [`Proposer`] is
//! handed a [`ProjectedContext`] — a read-only projection of the booking, the
//! menu of behaviours the projection published, the latest version, and the
//! non-authoritative request — and emits ONE [`ProposedAction`]. A driver submits
//! that action through `bld-client`'s public HTTP surface; the boundary disposes.
//!
//! Nothing in this crate names a domain, store, authority, council or payment
//! type. That is the point: the same [`Proposer`] seam carries a helpful LLM
//! proposer and a deterministic [`hostile::HostileProposer`], and the same
//! adversarial suite runs against both — so a safety claim is about the boundary,
//! never about a model being "nice enough" (spec §18, §19).
//!
//! # What a proposer may NOT decide (spec §18.1)
//!
//! A proposer chooses the next behaviour and fills non-authoritative constraints
//! (which venue, the purpose, the headcount from the request). It never sets the
//! price, the permission scope, the resource version, the payment status, an
//! effect/idempotency id, or whether an external effect succeeded. Those are read
//! from the projection or supplied by the boundary — which is exactly why
//! [`ProjectedContext`] carries only projected facts and [`ProposedAction`]
//! carries only a behaviour name plus its arguments.

pub mod hostile;
pub mod llm;
pub mod openai;

use async_trait::async_trait;

/// The read-only view a proposer is allowed to see. Deliberately a PROJECTION,
/// not a capability: it is what `bld-client`'s read returned, plus the
/// non-authoritative request and the venue candidates a browse surfaced. A
/// proposer cannot reach past this to the council, the store, or a signing key,
/// because it is never handed one.
#[derive(Clone, Debug)]
pub struct ProjectedContext {
    /// The human's natural-language request — non-authoritative intent, the only
    /// thing the model interprets.
    pub request: String,
    /// The booking's current state name, as the projection published it. `None`
    /// before the booking intent has been created.
    pub state: Option<String>,
    /// The behaviours the projection SAYS are available now — the closed menu a
    /// proposer must choose from. Empty before creation, or at a terminal state.
    pub available_behaviours: Vec<String>,
    /// The resource version the read observed, carried so the driver can send it
    /// as `If-Match`. A proposer may READ it; it never invents one.
    pub version: Option<u64>,
    /// Read-only venue candidates, from the permitted browse surface — never the
    /// council directly.
    pub venues: Vec<VenueOption>,
}

impl ProjectedContext {
    /// Whether the published menu offers this behaviour. A helpful proposer checks
    /// this before choosing; the driver checks it again, so an out-of-menu
    /// proposal is caught before it ever reaches the wire.
    #[must_use]
    pub fn offers(&self, behaviour: &str) -> bool {
        self.available_behaviours.iter().any(|b| b == behaviour)
    }
}

/// One venue candidate a proposer may choose among — an id pair and the facts a
/// choice turns on. Read-only: surfaced by the browse projection, never authored
/// by the proposer.
#[derive(Clone, Debug, PartialEq, Eq, serde::Serialize)]
pub struct VenueOption {
    pub venue_id: String,
    pub slot_id: String,
    /// The council's fee for this slot, in pence — read from the projection so a
    /// proposer can prefer an affordable venue. It is NOT a price the proposer
    /// sets; the boundary re-verifies the fee before any money moves.
    pub fee_pence: u64,
    pub accessible: bool,
    pub capacity: u16,
}

/// The one thing a proposer produces: a single step against the public surface.
///
/// It maps straight onto `bld-client` — [`Self::Create`] to the collection,
/// [`Self::Drive`] to a published behaviour — so a proposer can express nothing
/// the public API cannot already express. There is no variant for "call the
/// council", "set the fee", or "confirm the payment": those are not on the
/// surface, so a hostile proposer cannot even name them.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum ProposedAction {
    /// Create the booking intent with these (non-authoritative) requirements.
    Create { body: serde_json::Value },
    /// Drive a state-scoped behaviour by the NAME the projection published, with
    /// its JSON arguments. `if_match` is the version to send — normally the one
    /// the context carried; a hostile proposer may put a stale or invented value
    /// here, which the boundary's optimistic-concurrency check refuses.
    Drive {
        behaviour: String,
        body: serde_json::Value,
        if_match: Option<u64>,
    },
    /// The proposer has nothing more to do — the journey reached a state it was
    /// asked to reach, or no sensible next step exists.
    Done,
}

/// A proposer of typed BLD actions. The ONE method is all a proposer does; it
/// cannot reach past `bld-client` because it is handed a projection, not a
/// capability. Implemented by the helpful LLM proposer and by the deterministic
/// [`hostile::HostileProposer`] alike.
#[async_trait]
pub trait Proposer: Send + Sync {
    /// Choose the next action given the projected view. Pure intent: returning a
    /// malicious or malformed action is allowed — that is what the boundary (and
    /// the driver's pre-checks) exist to refuse.
    async fn propose(&self, context: &ProjectedContext) -> ProposedAction;
}
