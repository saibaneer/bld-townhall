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
use bld_types::{BoundedString as BoundedDetail, EffectAttempt};

/// What a whole turn through the boundary amounted to.
///
/// [`Resolution`] and [`FactResolution`] are what a *door* answers, before
/// anything is persisted. This is what a coordinator answers after the whole
/// sequence — classify, maybe reach outside, maybe commit — has run.
///
/// `Undefined` and `Denied` are **not** the same thing, and collapsing them is
/// the most common way a boundary quietly rots:
///
/// - `Undefined` — the behaviour does not exist in this state at all. Nothing
///   ran; no policy was even consulted. `Draft` has no `book`.
/// - `Denied(e)` — the behaviour exists here, but a deterministic guard refused
///   it, with a typed reason.
/// - `Committed(s)` — checks passed and the next state was committed.
/// - `Converged` — authoritative state already reflected the evidence, so there
///   was nothing to commit. Success, not breakage: recovery re-applies facts by
///   design.
/// - `Unresolved` — an effect is in flight and its outcome is not yet knowable.
///
/// `Unresolved` is the one that carries weight. A coordinator that folded it
/// into `Denied` would return a booking to a re-proposable state while the
/// provider held a live one — the failure M4 exists to prevent. Timeout is
/// neither success nor failure, and it has to be sayable.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum BoundaryOutcome<S, E> {
    Undefined,
    Denied(E),
    Committed(S),
    Converged,
    Unresolved,
}

impl<S, E> BoundaryOutcome<S, E> {
    /// The committed state, if the turn committed one.
    pub const fn committed(&self) -> Option<&S> {
        match self {
            Self::Committed(state) => Some(state),
            _ => None,
        }
    }

    /// Whether an external effect is still outstanding. The caller must not
    /// treat this as either success or failure.
    #[must_use]
    pub const fn is_unresolved(&self) -> bool {
        matches!(self, Self::Unresolved)
    }
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

/// Evidence whose provenance a verifier has established.
///
/// `Verified<T>` answers exactly one question: did this claim pass its
/// provenance verifier — did it genuinely come from where it says it did,
/// intact? It does **not** say the claim is consistent with any resource. The
/// domain still binds every consequential field against the persisted canonical
/// plan (ADR-012); a field-perfect claim with the wrong provenance never gets
/// this far, and a well-provenanced claim about the wrong effect is refused by
/// the binding.
///
/// # What the type actually guarantees
///
/// - **No `Serialize`, no `Deserialize`.** Deserialising verified evidence from
///   a wire format is precisely the forgery the type exists to prevent.
/// - The untrusted half cannot *name* it: `agent-runtime` and `bld-client` may
///   not depend on this crate, so no proposer-facing transport can carry one.
///
/// # What it does not guarantee
///
/// Unforgeability. Any code inside the trusted half can construct one. The
/// constructor is named [`Verified::assert_verified`] so every call site greps
/// as an audit point — the guarantee is vocabulary separation plus the crate
/// graph, and claiming more would be an overclaim.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Verified<T> {
    inner: T,
}

impl<T> Verified<T> {
    /// Assert that `inner` passed its provenance verifier.
    ///
    /// Every call to this is a claim someone can audit. Grep for it.
    #[must_use]
    pub fn assert_verified(inner: T) -> Self {
        Self { inner }
    }

    #[must_use]
    pub fn get(&self) -> &T {
        &self.inner
    }

    #[must_use]
    pub fn into_inner(self) -> T {
        self.inner
    }
}

/// The fact door's four outcomes.
///
/// Three are [`Resolution`]'s. The fourth exists because recovery re-applies
/// the same fact **by design**: a reconciler that lost a compare-and-set
/// reloads and asks again, and "authoritative state already reflects this
/// fact" is success, not breakage. Without `Converged`, healthy convergence is
/// indistinguishable from a refused transition and a reconciler reads its own
/// success as an error.
///
/// `Converged` is deliberately **not** added to the proposal door: for intent,
/// a silent no-op hides mistakes — `Book` when already booked is `Undefined`,
/// never "quietly fine" (ADR-012).
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum FactResolution<P, E> {
    Undefined,
    Denied(E),
    Ready(P),
    Converged,
}

impl<P, E> FactResolution<P, E> {
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

    /// Whether authoritative state already reflects the fact.
    #[must_use]
    pub const fn is_converged(&self) -> bool {
        matches!(self, Self::Converged)
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
    /// Externally verified reality, as domain vocabulary. Lives in the domain
    /// crate, not here — the kernel must not know what a booking is (ADR-001).
    type ProviderFact: Send;
    /// A deterministic runtime fact. Neither intent nor external truth: the
    /// provider cannot tell us our own retry budget is exhausted.
    type SystemEvent: Send;
    /// What the coordinator must supply for fact binding — canonically, the
    /// persisted effect intent. Deliberately a different type from `Context`:
    /// the fact door must bind against the persisted plan, and a context that
    /// cannot even name capability-loaded facts makes that structural.
    type FactContext: Send + Sync;
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

    /// Classify a verified provider fact against the current state.
    ///
    /// No authority parameter, deliberately: a fact is admitted by its
    /// verifier, not authorised by a principal, and recovery must run with a
    /// helpful model, a hostile model, or no model at all (ADR-012). The
    /// `principal` a fact must match comes from the persisted canonical plan —
    /// which is why the plan is persisted.
    async fn resolve_fact(
        &self,
        state: &Self::State,
        fact: Verified<Self::ProviderFact>,
        context: &Self::FactContext,
    ) -> FactResolution<TransitionPlan<Self::State, Self::Effect>, Self::Error>;

    /// Classify a deterministic runtime fact against the current state.
    ///
    /// No context at all: the only binding a system event needs is "is this
    /// the effect this state is waiting on", and the state carries that
    /// identity. Nothing but state and event is what lets this door run with
    /// no provider reachable and no model present.
    async fn resolve_system_event(
        &self,
        state: &Self::State,
        event: Self::SystemEvent,
    ) -> Resolution<TransitionPlan<Self::State, Self::Effect>, Self::Error>;
}

/// Deterministic transition resolution — the three provenance doors, in one
/// named place.
///
/// ```text
/// resolve_proposal      what someone WANTS       (intent)
/// resolve_fact          what is externally TRUE  (verified provider fact)
/// resolve_system_event  what the runtime KNOWS   (deterministic runtime fact)
/// ```
///
/// # Honestly: each method still forwards to the domain
///
/// B2's version of this comment promised the kernel would "stop being a
/// passthrough" at B3. The accurate statement is narrower: it stops being a
/// *single-door* passthrough. No method here adds logic — the value is that
/// every way state can legally change is visible in this one type, which makes
/// "these are the only three doors" auditable rather than asserted. The
/// forbidden move — a proposer driving a fact-shaped transition — is absent
/// from the *type system*: `resolve_fact` demands `Verified<ProviderFact>`,
/// which proposer-facing transport cannot construct or even name.
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

    /// Classify a verified provider fact. Returns a plan or `Converged` — the
    /// kernel neither mutates state nor persists anything.
    pub async fn resolve_fact<D: BoundaryDomain>(
        &self,
        domain: &D,
        state: &D::State,
        fact: Verified<D::ProviderFact>,
        context: &D::FactContext,
    ) -> FactResolution<TransitionPlan<D::State, D::Effect>, D::Error> {
        domain.resolve_fact(state, fact, context).await
    }

    /// Classify a deterministic runtime fact.
    pub async fn resolve_system_event<D: BoundaryDomain>(
        &self,
        domain: &D,
        state: &D::State,
        event: D::SystemEvent,
    ) -> Resolution<TransitionPlan<D::State, D::Effect>, D::Error> {
        domain.resolve_system_event(state, event).await
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

    /// One fact, carrying the identity it claims to answer.
    #[derive(Clone, Copy, Debug, PartialEq, Eq)]
    struct Arrived {
        effect_id: u8,
    }

    #[derive(Clone, Copy)]
    enum Event {
        GaveUp,
    }

    /// What the coordinator supplies for binding: which effect is in flight.
    struct FactContext {
        in_flight: Option<u8>,
    }

    #[derive(Clone, Debug, PartialEq, Eq)]
    enum Error {
        Denied,
        WrongEffect,
    }

    struct Domain;

    #[async_trait]
    impl BoundaryDomain for Domain {
        type State = State;
        type Proposal = Proposal;
        type Effect = Effect;
        type Authority = Authority;
        type Context = Context;
        type ProviderFact = Arrived;
        type SystemEvent = Event;
        type FactContext = FactContext;
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

        // The four outcomes, minimally: a fact answers `Reaching` if it names
        // the in-flight effect; `Done` already reflects any arrival; `Start`
        // has no fact-shaped behaviour at all.
        async fn resolve_fact(
            &self,
            state: &Self::State,
            fact: Verified<Self::ProviderFact>,
            context: &Self::FactContext,
        ) -> FactResolution<TransitionPlan<Self::State, Self::Effect>, Self::Error> {
            match state {
                State::Start => FactResolution::Undefined,
                State::Done => FactResolution::Converged,
                State::Reaching => match context.in_flight {
                    Some(id) if id == fact.get().effect_id => {
                        FactResolution::Ready(TransitionPlan::Local {
                            next_state: State::Done,
                        })
                    }
                    _ => FactResolution::Denied(Error::WrongEffect),
                },
            }
        }

        async fn resolve_system_event(
            &self,
            state: &Self::State,
            event: Self::SystemEvent,
        ) -> Resolution<TransitionPlan<Self::State, Self::Effect>, Self::Error> {
            let Event::GaveUp = event;
            match state {
                State::Reaching => Resolution::Ready(TransitionPlan::Local {
                    next_state: State::Start,
                }),
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

    /// The fact door has a fourth outcome the proposal door must not have:
    /// a state that already reflects the fact is convergence, not breakage.
    /// This is what lets a reconciler re-apply a fact after losing a CAS.
    #[tokio::test]
    async fn a_fact_the_state_already_reflects_converges() {
        let got = Kernel
            .resolve_fact(
                &Domain,
                &State::Done,
                Verified::assert_verified(Arrived { effect_id: 7 }),
                &FactContext { in_flight: None },
            )
            .await;
        assert!(got.is_converged());
        assert!(!got.is_ready(), "Converged must never carry a plan");
    }

    /// A fact where no fact-shaped behaviour exists is Undefined — exactly the
    /// proposal door's distinction, preserved across doors.
    #[tokio::test]
    async fn a_fact_with_no_edge_here_is_undefined() {
        let got = Kernel
            .resolve_fact(
                &Domain,
                &State::Start,
                Verified::assert_verified(Arrived { effect_id: 7 }),
                &FactContext { in_flight: None },
            )
            .await;
        assert!(got.is_undefined());
    }

    /// A fact that fails its binding is Denied with a typed reason — the
    /// behaviour exists, the evidence does not fit.
    #[tokio::test]
    async fn a_fact_naming_the_wrong_effect_is_denied() {
        let got = Kernel
            .resolve_fact(
                &Domain,
                &State::Reaching,
                Verified::assert_verified(Arrived { effect_id: 9 }),
                &FactContext { in_flight: Some(7) },
            )
            .await;
        assert_eq!(got, FactResolution::Denied(Error::WrongEffect));
    }

    /// A bound fact at the waiting state yields the transition.
    #[tokio::test]
    async fn a_bound_fact_at_the_waiting_state_yields_a_plan() {
        let got = Kernel
            .resolve_fact(
                &Domain,
                &State::Reaching,
                Verified::assert_verified(Arrived { effect_id: 7 }),
                &FactContext { in_flight: Some(7) },
            )
            .await;
        let FactResolution::Ready(plan) = got else {
            panic!("expected Ready");
        };
        assert_eq!(*plan.next_state(), State::Done);
    }

    /// The system-event door: reachable only where something is in flight.
    #[tokio::test]
    async fn a_system_event_moves_only_an_in_flight_state() {
        let moved = Kernel
            .resolve_system_event(&Domain, &State::Reaching, Event::GaveUp)
            .await;
        assert!(moved.is_ready());

        let nowhere = Kernel
            .resolve_system_event(&Domain, &State::Done, Event::GaveUp)
            .await;
        assert!(nowhere.is_undefined());
    }

    /// The load-bearing negative: `Verified<T>` implements neither `Serialize`
    /// nor `Deserialize`, and this fails to COMPILE if either impl ever appears.
    ///
    /// The plan called for a `trybuild` compile-fail test here. This is the
    /// same assertion with a cheaper mechanism: `assert_not_impl_any!` errors
    /// at compile time on the exact property (the impl exists), where trybuild
    /// pins a full stderr transcript that drifts with compiler versions and can
    /// silently start passing for an unrelated compilation error.
    /// `DeserializeOwned` sidesteps the lifetime parameter that makes
    /// `Deserialize<'de>` awkward to name in a static assertion.
    ///
    /// A `Serialize` impl would let verified evidence leak outward through any
    /// generic sink; a `Deserialize` impl would let it be minted from JSON,
    /// which is the forgery ADR-012 exists to prevent.
    #[test]
    fn verified_evidence_cannot_cross_a_wire() {
        static_assertions::assert_not_impl_any!(
            Verified<Arrived>: serde::Serialize, serde::de::DeserializeOwned
        );
        static_assertions::assert_not_impl_any!(
            Verified<String>: serde::Serialize, serde::de::DeserializeOwned
        );
    }

    /// `Verified<T>` hands its inner value out but never absorbs one from a
    /// wire format — construction is `assert_verified`, greppably, or nothing.
    #[test]
    fn verified_exposes_but_never_absorbs() {
        let fact = Verified::assert_verified(Arrived { effect_id: 7 });
        assert_eq!(fact.get().effect_id, 7);
        assert_eq!(fact.into_inner().effect_id, 7);
    }
}

/// Why an external attempt produced no usable answer.
///
/// **One variant, deliberately.** An earlier draft of slice C gave this a
/// `Refused` variant and had the coordinator turn it into verified evidence
/// directly — the coordinator asserting provenance, which is exactly what
/// ADR-012 forbids, and which would let an adapter-level refusal (a malformed
/// request, a rejected header) become ADR-016's much stronger claim that the
/// provider permanently closed an effect.
///
/// So an authoritative refusal is a provider **response**, not a transport
/// error. It travels in [`Capability::Raw`] and passes through a [`Verifier`]
/// like any other response. This type is for ambiguity only.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Unknown {
    detail: BoundedDetail,
}

impl Unknown {
    /// The attempt produced neither success nor failure: a timeout, a dropped
    /// connection, a 5xx.
    ///
    /// **Not failure.** A coordinator that treated this as failure would return
    /// a resource to a re-proposable state while the provider held a live
    /// effect — the failure M4 exists to prevent.
    #[must_use]
    pub fn new(detail: BoundedDetail) -> Self {
        Self { detail }
    }

    #[must_use]
    pub const fn detail(&self) -> &BoundedDetail {
        &self.detail
    }
}

impl core::fmt::Display for Unknown {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        write!(f, "the outcome is unknown: {}", self.detail.as_str())
    }
}

impl core::error::Error for Unknown {}

/// Performs one external consequence, and reports what the provider said.
///
/// The capability receives the **canonical plan** the boundary derived, never
/// model instructions — and an [`EffectAttempt`] alongside it, because the plan
/// deliberately does not carry an identity (the repository owns effect identity,
/// since it holds the uniqueness key).
///
/// # Why an attempt and not just an identity
///
/// A provider that enforces a deadline must bind the deadline the *durable
/// intent* holds, not one the adapter arrived at independently. Passing only an
/// identity leaves the adapter to source the deadline itself, and there is no
/// correct way to do that from inside a capability — see [`EffectAttempt`] for
/// the failure this prevents.
///
/// Everything else the provider must be handed back travels in the **plan**, for
/// the same reason and by the same rule: a value issued during one turn and spent
/// during a later one has to wait somewhere durable, and the persisted canonical
/// plan is the only honest place. An adapter that re-fetches instead gets a
/// *currently valid* answer about facts the plan no longer reflects.
///
/// `Raw` is whatever the provider actually returned, unexamined. Establishing
/// that it genuinely came from the provider, and what it means, is a
/// [`Verifier`]'s job — not this trait's, and emphatically not the
/// coordinator's.
#[async_trait]
pub trait Capability<E>: Send + Sync {
    /// The provider's unexamined response. Carries success *and* authoritative
    /// refusal, because both are things the provider said.
    type Raw: Send;

    /// Attempt the effect. Returns [`Unknown`] only when the attempt produced
    /// no answer at all.
    async fn execute(&self, effect: &E, attempt: &EffectAttempt) -> Result<Self::Raw, Unknown>;
}

/// Establishes that a raw provider response is genuine, and what fact it
/// carries.
///
/// # The refusal burden is heavier than the success burden
///
/// For a success the verifier must establish provenance and integrity: this
/// response really came from the provider, intact.
///
/// For a **refusal** it must additionally establish that the provider
/// *permanently closed this exact effect identity*. A terminal rejection is
/// acted on irreversibly — the resource returns to a re-proposable state and the
/// intent is finalised — so anything short of permanent closure must not become
/// one:
///
/// ```text
/// authenticated, well-formed, durably closing this identity  -> the rejection fact
/// authenticated, well-formed, but validation / authorization /
///     rate limit / "try again"                               -> VerificationError::Unknown
/// unauthenticated, malformed, unattributable                 -> VerificationError::Rejected
/// ```
///
/// Without that distinction the provenance bypass simply relocates from the
/// coordinator into this trait's implementations, and an ordinary 4xx becomes a
/// durable tombstone.
pub trait Verifier<R, F>: Send + Sync {
    /// # Errors
    /// [`VerificationError`] when no fact can be established.
    fn verify(&self, raw: R) -> Result<Verified<F>, VerificationError>;
}

/// Why no fact could be established from a raw response.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum VerificationError {
    /// The response is genuine but establishes nothing terminal. Treated exactly
    /// as [`Unknown`]: the effect stays in flight.
    Unknown(BoundedDetail),
    /// The response could not be attributed to the provider, or was malformed.
    /// Also not a conclusion — a boundary that cannot read an answer has not
    /// received one.
    Rejected(BoundedDetail),
}

impl core::fmt::Display for VerificationError {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        match self {
            Self::Unknown(detail) => {
                write!(f, "nothing terminal was established: {}", detail.as_str())
            }
            Self::Rejected(detail) => {
                write!(f, "the response could not be trusted: {}", detail.as_str())
            }
        }
    }
}

impl core::error::Error for VerificationError {}
