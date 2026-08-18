#![forbid(unsafe_code)]

//! The deterministic BLD boundary kernel.
//!
//! The kernel decides whether a transition is **legal**. It does not perform it,
//! does not talk to anything external, and does not own state.
//!
//! # What changed at M4, and why
//!
//! The kernel used to run `resolve → execute → validate` in one call and assign
//! `*state = next` at the end. That worked while the capability was an
//! in-process fake. It cannot express what a real external effect requires:
//!
//! ```text
//! commit  →  call the provider  →  commit again
//! ```
//!
//! Two commits with a network round-trip between them do not fit a signature
//! that owns `&mut State` and returns once. So responsibilities separated
//! (ADR-013):
//!
//! ```text
//! Domain       legal meaning
//! Kernel       deterministic transition resolution   <- this crate
//! Repository   authoritative compare-and-set commit
//! Coordinator  external-effect choreography
//! Capability   external action
//! Verifier     provenance establishment
//! ```
//!
//! `execute` and `validate` left [`BoundaryDomain`] as part of that. Executing
//! an effect is a capability's job; establishing that a provider response is
//! genuine is a verifier's. Neither is domain policy, and keeping them here
//! forced the kernel to sit in the middle of a network call.

use async_trait::async_trait;

/// The three outcomes a boundary evaluation can have.
///
/// `Undefined` and `Denied` are **not** the same thing, and collapsing them is
/// the most common way a boundary quietly rots:
///
/// - `Undefined` — the behaviour does not exist in this state at all. Nothing
///   ran; no policy was even consulted. `Draft` has no `book`.
/// - `Denied(e)` — the behaviour exists here, but a deterministic guard refused
///   it, with a typed reason.
/// - `Committed(s)` — checks passed and the next state was committed.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum BoundaryOutcome<S, E> {
    Undefined,
    Denied(E),
    Committed(S),
}

/// The same trichotomy, before anything is persisted.
///
/// A `Ready` carries a *plan*, not a committed state — the repository performs
/// the compare-and-set.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Resolution<P, E> {
    Undefined,
    Denied(E),
    Ready(P),
}

impl<P, E> From<Result<P, E>> for Resolution<P, E> {
    fn from(value: Result<P, E>) -> Self {
        match value {
            Ok(plan) => Self::Ready(plan),
            Err(error) => Self::Denied(error),
        }
    }
}

impl<P, E> Resolution<P, E> {
    /// Whether this resolution would produce a transition.
    #[must_use]
    pub const fn is_ready(&self) -> bool {
        matches!(self, Self::Ready(_))
    }

    /// Whether the behaviour exists in this state at all.
    #[must_use]
    pub const fn is_undefined(&self) -> bool {
        matches!(self, Self::Undefined)
    }
}

/// What a legal transition will do.
///
/// The distinction is load-bearing, not descriptive. A `Local` transition can
/// be committed and forgotten. An `ExternalEffect` must have its intent
/// durably persisted **before** the capability is invoked (ADR-014), because a
/// crash between calling and committing otherwise leaves no record that an
/// external consequence may exist.
///
/// Modelling every transition as an effect would force `Draft → VenueSelected`
/// through a recovery protocol it does not need; modelling none of them that
/// way is how bookings get duplicated.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum TransitionPlan<S, E> {
    Local { next_state: S },
    ExternalEffect { next_state: S, effect: E },
}

impl<S, E> TransitionPlan<S, E> {
    /// The state this transition commits to.
    pub const fn next_state(&self) -> &S {
        match self {
            Self::Local { next_state } | Self::ExternalEffect { next_state, .. } => next_state,
        }
    }

    /// The intended external consequence, if there is one.
    pub const fn effect(&self) -> Option<&E> {
        match self {
            Self::Local { .. } => None,
            Self::ExternalEffect { effect, .. } => Some(effect),
        }
    }
}

/// A domain's legal transition graph.
///
/// Note what is absent: no `execute`, no `validate`, no `&mut` anything. The
/// domain decides *meaning*; it neither performs effects nor persists results.
#[async_trait]
pub trait BoundaryDomain: Send + Sync {
    type State: Clone + Send + Sync;
    type Proposal: Send;
    /// The intended external consequence an `ExternalEffect` carries.
    type Effect: Send + Sync;
    type Authority: Send + Sync;
    type Context: Send + Sync;
    type Error: Send;

    /// Classify a proposal against the current state.
    ///
    /// Whether a behaviour *exists* must depend on `(state, proposal)` alone.
    /// Authority and context decide whether an existing behaviour is permitted
    /// — they may turn `Ready` into `Denied`, never into `Undefined`.
    async fn resolve_proposal(
        &self,
        state: &Self::State,
        proposal: Self::Proposal,
        authority: &Self::Authority,
        context: &Self::Context,
    ) -> Resolution<TransitionPlan<Self::State, Self::Effect>, Self::Error>;
}

/// Deterministic transition resolution.
///
/// # Honestly: at B2 this is a passthrough
///
/// With one door and `execute`/`validate` gone, there is no sequencing left for
/// the kernel to enforce — it forwards to the domain and nothing more. It is
/// kept rather than deleted because B3 adds two more doors, and *then* it
/// becomes the typed dispatch point where "which provenance class is this
/// transition being driven by" lives:
///
/// ```text
/// resolve_proposal      what someone WANTS
/// resolve_fact          what is externally TRUE      (B3)
/// resolve_system_event  what the runtime KNOWS       (B3)
/// ```
///
/// Deleting it now and reintroducing it in B3 would churn the public API twice
/// for no gain. Saying it earns its keep today would be an overclaim.
#[derive(Clone, Copy, Debug, Default)]
pub struct Kernel;

impl Kernel {
    /// Classify a proposal. Returns a plan for the coordinator to commit — the
    /// kernel neither mutates state nor persists anything.
    pub async fn resolve_proposal<D: BoundaryDomain>(
        &self,
        domain: &D,
        state: &D::State,
        proposal: D::Proposal,
        authority: &D::Authority,
        context: &D::Context,
    ) -> Resolution<TransitionPlan<D::State, D::Effect>, D::Error> {
        domain
            .resolve_proposal(state, proposal, authority, context)
            .await
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[derive(Clone, Debug, PartialEq, Eq)]
    enum State {
        Start,
        Done,
        Reaching,
    }

    #[derive(Clone, Copy)]
    enum Proposal {
        Go,
        Reach,
        Impossible,
    }

    #[derive(Clone, Copy)]
    struct Authority {
        allowed: bool,
    }

    #[derive(Default)]
    struct Context;

    #[derive(Clone, Copy, Debug, PartialEq, Eq)]
    struct Effect;

    #[derive(Clone, Debug, PartialEq, Eq)]
    enum Error {
        Denied,
    }

    struct Domain;

    #[async_trait]
    impl BoundaryDomain for Domain {
        type State = State;
        type Proposal = Proposal;
        type Effect = Effect;
        type Authority = Authority;
        type Context = Context;
        type Error = Error;

        // One arm per (state, proposal) pair, deliberately - see the same note
        // on TownHallDomain::resolve_proposal. The match IS the topology.
        #[allow(clippy::match_same_arms)]
        async fn resolve_proposal(
            &self,
            state: &Self::State,
            proposal: Self::Proposal,
            authority: &Self::Authority,
            _context: &Self::Context,
        ) -> Resolution<TransitionPlan<Self::State, Self::Effect>, Self::Error> {
            match (state, proposal) {
                (State::Start, Proposal::Go) if authority.allowed => {
                    Resolution::Ready(TransitionPlan::Local {
                        next_state: State::Done,
                    })
                }
                (State::Start, Proposal::Go) => Resolution::Denied(Error::Denied),
                (State::Start, Proposal::Reach) if authority.allowed => {
                    Resolution::Ready(TransitionPlan::ExternalEffect {
                        next_state: State::Reaching,
                        effect: Effect,
                    })
                }
                (State::Start, Proposal::Reach) => Resolution::Denied(Error::Denied),
                _ => Resolution::Undefined,
            }
        }
    }

    async fn classify(
        state: State,
        proposal: Proposal,
        allowed: bool,
    ) -> Resolution<TransitionPlan<State, Effect>, Error> {
        Domain
            .resolve_proposal(&state, proposal, &Authority { allowed }, &Context)
            .await
    }

    /// A behaviour that does not exist here yields no plan at all. Nothing to
    /// commit, nothing to execute — the distinction from `Denied` is that no
    /// guard was even consulted.
    #[tokio::test]
    async fn undefined_yields_no_plan() {
        let got = classify(State::Start, Proposal::Impossible, true).await;
        assert!(got.is_undefined());
        assert!(!got.is_ready(), "Undefined must never carry a plan");
    }

    /// The behaviour exists but a guard refused it. Also no plan — but for a
    /// different, typed reason.
    #[tokio::test]
    async fn denied_yields_no_plan() {
        let got = classify(State::Start, Proposal::Go, false).await;
        assert_eq!(got, Resolution::Denied(Error::Denied));
        assert!(!got.is_ready(), "Denied must never carry a plan");
    }

    /// A local transition carries its next state and no effect. Committing it
    /// requires nothing external.
    #[tokio::test]
    async fn a_local_transition_carries_a_next_state_and_no_effect() {
        let Resolution::Ready(plan) = classify(State::Start, Proposal::Go, true).await else {
            panic!("expected Ready");
        };
        assert_eq!(*plan.next_state(), State::Done);
        assert_eq!(plan.effect(), None, "a local transition must reach nothing");
    }

    /// An external-effect transition carries both. The effect is what must be
    /// durably persisted before any capability is invoked (ADR-014).
    #[tokio::test]
    async fn an_external_transition_carries_an_effect_to_persist_first() {
        let Resolution::Ready(plan) = classify(State::Start, Proposal::Reach, true).await else {
            panic!("expected Ready");
        };
        assert_eq!(*plan.next_state(), State::Reaching);
        assert_eq!(plan.effect(), Some(&Effect));
    }

    /// The kernel does not own state. Classification is a pure question about a
    /// state value, so asking twice cannot change anything — which is what lets
    /// a coordinator reload and re-classify after losing a compare-and-set.
    #[tokio::test]
    async fn classification_does_not_mutate_and_is_repeatable() {
        let state = State::Start;
        let first = classify(state.clone(), Proposal::Go, true).await;
        let second = classify(state.clone(), Proposal::Go, true).await;
        assert_eq!(first, second);
        assert_eq!(state, State::Start, "the caller's state is untouched");
    }
}
