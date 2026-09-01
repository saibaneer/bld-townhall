#![forbid(unsafe_code)]

//! The coordinator: the component that finally *sequences* the boundary.
//!
//! Everything below this crate classifies. Nothing ordered it. Before slice C2
//! the correct order — load, classify, persist the intent, call the provider,
//! classify the evidence, commit the outcome — was a convention the tests
//! happened to follow. A caller could resolve a `Ready(ExternalEffect)` and
//! never persist the intent, call a capability before the intent was durable, or
//! commit an outcome without consulting `resolve_fact` at all.
//!
//! ```text
//! Phase A   persist the intent, commit the in-flight state      repository
//! Phase B   call the provider                                   capability
//! Phase C   verify, classify the evidence, commit the outcome   verifier + domain + repository
//! ```
//!
//! # What is structural here, and what is not
//!
//! Two properties hold by construction:
//!
//! - **No transaction is open during Phase B.** Every repository operation takes
//!   and releases its own transaction and returns *committed* state. Nothing
//!   hands a transaction out, so there is no way to hold one across a network
//!   call — which under `BEGIN IMMEDIATE` would block every unrelated booking
//!   for the busy timeout (ADR-015).
//! - **The outcome is never the coordinator's opinion.** Phase C asks
//!   `resolve_fact`. This crate cannot decide `Booked`; it can only carry a
//!   classification to the repository.
//!
//! One property is narrower than it first looks, and worth stating precisely:
//! the capability *cannot* be called before Phase A **on this path**, because the
//! effect identity it needs is minted by `prepare_effect` and does not exist
//! until that commits. That makes the correct order the natural one here. It is
//! not a global guarantee — the repository and the capability are both public,
//! and a different caller could misuse them.

use async_trait::async_trait;
use bld_kernel::{
    BoundaryOutcome, Capability, FactResolution, Kernel, Resolution, TransitionPlan, Unknown,
    Verified, Verifier,
};
use bld_types::{BookingId, EffectAttempt, EffectIntentId, SlotId, VenueId};
use std::sync::Arc;
use thiserror::Error;
use townhall_domain::{
    Booking, BookingAggregate, BookingContext, BookingEffect, BookingError, BookingProposal,
    FactContext, ObservedAvailability, OperationKind, SystemEvent, TownHallDomain,
    VerifiedAuthority, VerifiedProviderFact,
};
use townhall_store::{
    BookingRepository, ClaimedEffect, FinalizeEffect, HandoffEffect, PrepareEffect, StoreError,
    TransitionAudit, derive_effect_intent_id,
};

pub mod fake;

/// Where authoritative availability comes from.
///
/// Loading it is a coordinator responsibility, not a transition (ADR-013): the
/// domain binds facts it is *given* against what the user chose, and a transition
/// that fetched its own inputs would be reaching outside the boundary to decide
/// whether it is allowed.
///
/// The coordinator asks for the facts of whatever venue the booking currently
/// names, without knowing which proposals need them. That is deliberate — a
/// coordinator that branched on "does `VerifySlot` need availability?" would hold
/// a copy of the topology, and topology lives in one place.
///
/// Answers three ways (ADR-021): an ANSWER (with or without facts — an
/// unverifiable or wrong-slot response is not a weaker fact, it is no fact,
/// and stays `Answered(None)` only when the provider genuinely answered), or
/// `Unavailable` — the provider could not be asked at all, which the domain
/// refuses as its own error and the wire maps to 503 rather than 422. Failing
/// towards refusal remains the only safe direction for context three guards
/// read; the three-way shape only keeps the REASON honest.
#[async_trait]
pub trait AvailabilitySource: Send + Sync {
    async fn read(&self, venue: &VenueId, slot: &SlotId) -> ObservedAvailability;
}

/// The browse catalogue (spec §10's `GET /venues` search, served through the
/// council's own list). BROWSE-ONLY by contract: nothing returned here is
/// evidence — every guard consumes the per-slot verified answer instead.
#[async_trait]
pub trait CatalogueSource: Send + Sync {
    /// The full list, or `None` when the provider cannot be asked (503).
    async fn venues(&self) -> Option<Vec<VenueSummary>>;
}

/// One browse row. Deliberately not `VenueFacts`: same fields, different
/// authority — a type the guards cannot consume by construction.
#[derive(Clone, Debug)]
pub struct VenueSummary {
    pub venue_id: String,
    pub slot_id: String,
    pub fee_pence: i64,
    pub capacity: u64,
    pub accessible: bool,
    pub available: bool,
}

/// Browse filters for [`CatalogueSource`] results — applied by the facade, so
/// the wire's query parameters never reach the provider.
#[derive(Clone, Copy, Debug, Default)]
pub struct VenueFilters {
    pub attendees: Option<u16>,
    pub accessible: Option<bool>,
    pub max_fee_pence: Option<i64>,
}

/// Asks a provider what became of an effect it may or may not have seen.
///
/// [`Capability`] *causes* an effect; this only asks about one, and that
/// distinction is the whole of reconciliation. They are separate traits because a
/// reconciler must be able to ask without any possibility of causing: a single
/// trait with a flag would put "do not actually book anything" in a parameter,
/// which is not where a guarantee belongs.
///
/// `Raw` is a type parameter rather than an associated type so a coordinator can
/// require `EffectResolver<C::Raw>` and reuse the capability's verifier. Two
/// independently-declared raw types that happen to coincide would be the same
/// fact in two places, and a reconciler would need a second verifier bound to
/// say so.
#[async_trait]
pub trait EffectResolver<Raw>: Send + Sync {
    async fn resolve(
        &self,
        attempt: &EffectAttempt,
        kind: OperationKind,
    ) -> Result<Resolved<Raw>, Unknown>;
}

/// What one ask produced, for the pursuit decision (ADR-020).
///
/// `NotYetVisible` is the one reply that can authorize a resend, so its bar is
/// stated at the trait: an implementation may only produce it after **(1)** the
/// reply's signature verified against the pinned council key and **(2)** the
/// reply names exactly the asked attempt's identity. A signed not-yet for
/// identity A must never authorize resending identity B. Everything that fails
/// either check — bad or missing signature, wrong identity, protocol conflict,
/// "unavailable", garbage — is `Err(Unknown)`: an unusable reply that drives
/// nothing.
///
/// Deliberately not a `VerifiedProviderFact`: "not yet" is a pursuit signal,
/// not a fact about the world, and it never enters the fact door.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Resolved<Raw> {
    /// An answer to hand to the verifier and, if it passes, the fact door.
    Answer(Raw),
    /// The council's authenticated, identity-bound "I know this attempt and
    /// nothing is settled".
    NotYetVisible,
}

#[derive(Debug, Error)]
pub enum ServiceError {
    #[error(transparent)]
    Store(#[from] StoreError),
    /// The compare-and-set was lost more times than the attempt budget allows.
    ///
    /// Not a failure of the effect: the intent is durable and reconciliation owns
    /// anything unfinished. The caller is told the *current* truth, never a
    /// fabricated success.
    #[error("gave up re-classifying after {attempts} attempts under contention")]
    Contended { attempts: u32 },
    /// The caller's expected version is not the current one — either at the
    /// pre-classification comparison, or because the CAS/replay proved the
    /// world moved mid-turn. Carries the CURRENT version so a 412 always ships
    /// the fresh `ETag` (ADR-021). Produced only by [`Coordinator::propose_at`];
    /// the versionless `propose` keeps M4's contracts.
    #[error("expected version is stale; the current version is {current}")]
    PreconditionFailed { current: u64 },
    /// The domain produced a plan this path cannot carry out.
    ///
    /// Unreachable through the current transition graph, and refused rather than
    /// asserted: a coordinator that panicked here would turn a classification bug
    /// into a downed process, and the whole point of the boundary is that a wrong
    /// answer is still an answer.
    #[error("the classification cannot be carried out here: {reason}")]
    UnexpectedPlan { reason: &'static str },
}

/// The turn's answer, as a caller sees it.
pub type Turn = BoundaryOutcome<BookingAggregate, BookingError>;

/// How long Phase B may hold an intent while it calls out.
const PHASE_B_LEASE_MS: i64 = 30_000;

/// How soon after an inconclusive call the reconciler may ask again.
const RETRY_CADENCE_MS: i64 = 5_000;

/// How soon after ESCALATION the reconciler may ask again: hours, not seconds.
/// Giving up changes the cadence, never the asking — the council is pull-only,
/// so an exit of "a late fact settles it" is only real if lookups keep
/// happening (ADR-019 §3).
const ESCALATED_CADENCE_MS: i64 = townhall_store::MAX_CADENCE_MS;

/// One validated configuration for every pursuit knob, shared by the
/// coordinator and the reconciler so configured behaviour can never diverge
/// from persisted behaviour (ADR-021). Values are validated against the
/// store's own clamps at construction; the single sanctioned zero is
/// `reclassify_attempts`, the deterministic-429 test seam.
#[derive(Clone, Copy, Debug)]
pub struct PursuitConfig {
    pub retry_cadence_ms: i64,
    pub escalated_cadence_ms: i64,
    pub lease_ms: i64,
    pub attempt_budget: u32,
    pub reclassify_attempts: u32,
}

impl Default for PursuitConfig {
    fn default() -> Self {
        Self {
            retry_cadence_ms: RETRY_CADENCE_MS,
            escalated_cadence_ms: ESCALATED_CADENCE_MS,
            lease_ms: PHASE_B_LEASE_MS,
            attempt_budget: ATTEMPT_BUDGET,
            reclassify_attempts: 3,
        }
    }
}

impl PursuitConfig {
    /// Refuse a configuration the store would silently clamp or the protocol
    /// would silently break: cadences and the lease must be positive and
    /// within the store's maxima; the budget must exist.
    ///
    /// # Errors
    /// A human-readable description of the first violated bound.
    pub fn validated(self) -> Result<Self, String> {
        if self.retry_cadence_ms <= 0 || self.retry_cadence_ms > townhall_store::MAX_CADENCE_MS {
            return Err(format!(
                "retry_cadence_ms must be in 1..={}",
                townhall_store::MAX_CADENCE_MS
            ));
        }
        if self.escalated_cadence_ms <= 0
            || self.escalated_cadence_ms > townhall_store::MAX_CADENCE_MS
        {
            return Err(format!(
                "escalated_cadence_ms must be in 1..={}",
                townhall_store::MAX_CADENCE_MS
            ));
        }
        if self.lease_ms <= 0 || self.lease_ms > townhall_store::MAX_LEASE_MS {
            return Err(format!(
                "lease_ms must be in 1..={}",
                townhall_store::MAX_LEASE_MS
            ));
        }
        if self.attempt_budget == 0 {
            return Err("attempt_budget must be at least 1".to_owned());
        }
        Ok(self)
    }
}

/// How many started calls may accumulate before a turn escalates instead of
/// asking again. Conservative — a call that died mid-flight still counts —
/// and cheap to be conservative about, because under ADR-019 an early
/// escalation costs a longer cadence, not a stranding.
const ATTEMPT_BUDGET: u32 = 5;

/// Sequences the three phases around one proposal.
pub struct Coordinator<R, C, V, A> {
    repository: Arc<R>,
    capability: Arc<C>,
    verifier: Arc<V>,
    availability: Arc<A>,
    kernel: Kernel,
    domain: TownHallDomain,
    /// The denial log, when one is wired. One field serving all three doors, so
    /// "wire one door and call ADR-017 done" — the failure review predicted —
    /// is not expressible: either every refusal records, or none do.
    denials: Option<Arc<townhall_store::denials::DenialLog>>,
    /// Every pursuit knob, in one validated place — cadences, the lease, the
    /// escalation budget, and the Phase C re-classification budget (ADR-021).
    ///
    /// Correctness does not depend on any of these numbers. What makes the
    /// bounds safe is that the intent is durable and reconciliation owns
    /// anything unfinished, so exhausting a budget reports the current state
    /// rather than inventing an outcome.
    config: PursuitConfig,
}

impl<R, C, V, A> Coordinator<R, C, V, A>
where
    R: BookingRepository,
    C: Capability<BookingEffect>,
    V: Verifier<C::Raw, VerifiedProviderFact>,
    A: AvailabilitySource,
{
    pub fn new(
        repository: Arc<R>,
        capability: Arc<C>,
        verifier: Arc<V>,
        availability: Arc<A>,
    ) -> Self {
        Self {
            repository,
            capability,
            verifier,
            availability,
            kernel: Kernel,
            domain: TownHallDomain,
            denials: None,
            config: PursuitConfig::default(),
        }
    }

    /// Replace the pursuit configuration (validated — see
    /// [`PursuitConfig::validated`]). The reconciler built over this
    /// coordinator shares it, so the two can never disagree (ADR-021).
    #[must_use]
    pub fn with_config(mut self, config: PursuitConfig) -> Self {
        self.config = config;
        self
    }

    /// Wire the denial log. Every refusal at every door then leaves a trace:
    /// `Denied` a durable row, `Undefined` an in-memory count (ADR-017).
    #[must_use]
    pub fn with_denial_log(mut self, log: Arc<townhall_store::denials::DenialLog>) -> Self {
        self.denials = Some(log);
        self
    }

    /// Override the Phase C re-classification budget. For tests that want
    /// contention to be deterministic rather than lucky.
    #[must_use]
    pub const fn with_attempts(mut self, attempts: u32) -> Self {
        self.config.reclassify_attempts = attempts;
        self
    }

    /// Classify a proposal and carry it as far as it legitimately goes.
    ///
    /// # Errors
    /// [`ServiceError::Store`] for a persistence failure;
    /// [`ServiceError::Contended`] when Phase C lost its compare-and-set more
    /// times than the budget allows.
    pub async fn propose(
        &self,
        id: &BookingId,
        proposal: BookingProposal,
        authority: &VerifiedAuthority,
    ) -> Result<Turn, ServiceError> {
        self.run_proposal(id, proposal, authority, None).await
    }

    /// As [`Self::propose`], with the caller's EXPECTED VERSION bound inside
    /// the trusted turn (ADR-021; spec §9.2). Refused before classification
    /// when it does not match the load; enforced by the CAS at exactly that
    /// version after it; and a replayed Phase A — a rival already performed
    /// this turn — is refused too, because THIS caller performed no mutation
    /// and the world already moved. All three refusals are
    /// [`ServiceError::PreconditionFailed`] carrying the current version, so a
    /// 412 always ships the fresh `ETag`. A handler-side load/compare/propose
    /// cannot deliver this: the stale request would be silently rebased
    /// between the compare and the turn's own load.
    ///
    /// # Errors
    /// As [`Self::propose`], plus [`ServiceError::PreconditionFailed`].
    pub async fn propose_at(
        &self,
        id: &BookingId,
        expected_version: u64,
        proposal: BookingProposal,
        authority: &VerifiedAuthority,
    ) -> Result<Turn, ServiceError> {
        self.run_proposal(id, proposal, authority, Some(expected_version))
            .await
    }

    /// The current version, for a `PreconditionFailed` that must carry it.
    async fn stale(&self, id: &BookingId) -> Result<ServiceError, ServiceError> {
        let current = self.repository.load(id).await?.version;
        Ok(ServiceError::PreconditionFailed { current })
    }

    async fn run_proposal(
        &self,
        id: &BookingId,
        proposal: BookingProposal,
        authority: &VerifiedAuthority,
        expected: Option<u64>,
    ) -> Result<Turn, ServiceError> {
        let aggregate = self.repository.load(id).await?;
        if let Some(version) = expected
            && aggregate.version != version
        {
            return Err(ServiceError::PreconditionFailed {
                current: aggregate.version,
            });
        }
        let booking = Booking::from(&aggregate);

        // The audit record is built from the proposal itself, before it is
        // consumed, so the trail cannot attribute this turn to anything else.
        let audit = TransitionAudit::driven_by(&proposal);
        let context = self
            .proposal_context(&booking, &proposal, aggregate.version)
            .await;

        let resolved = self
            .kernel
            .resolve_proposal(&self.domain, &booking, proposal, authority, &context)
            .await;

        match resolved {
            // Nothing ran and nothing was touched — counted, never rowed:
            // `Undefined` is constructible from pure garbage, and a durable row
            // per garbage request is a disk-filling attack (ADR-017).
            Resolution::Undefined => {
                if let Some(log) = &self.denials {
                    log.note_undefined(booking.state.name(), audit.driver_name());
                }
                Ok(BoundaryOutcome::Undefined)
            }
            Resolution::Denied(error) => {
                // The answer is computed first and returned regardless: the
                // recording is an audit fact, and a boundary whose answers
                // depended on its logbook would no longer be deterministic.
                if let Some(log) = &self.denials {
                    log.record_denied(townhall_store::denials::Denial {
                        booking_id: id.to_string(),
                        driver_kind: audit.provenance(),
                        driver_detail: audit.driver_name(),
                        reason: error.name(),
                        principal: authority.principal.to_string(),
                    })
                    .await;
                }
                Ok(BoundaryOutcome::Denied(error))
            }

            Resolution::Ready(TransitionPlan::Local { next_state }) => {
                match self
                    .repository
                    .commit(id, aggregate.version, next_state, audit)
                    .await
                {
                    Ok(committed) => Ok(BoundaryOutcome::Committed(committed)),
                    // Under an expected version, losing the CAS IS the stale
                    // precondition — reload so the 412 carries the fresh ETag.
                    Err(StoreError::StaleVersion { .. }) if expected.is_some() => {
                        Err(self.stale(id).await?)
                    }
                    Err(error) => Err(error.into()),
                }
            }

            Resolution::Ready(TransitionPlan::ExternalEffect { next_state, effect }) => {
                self.reach_outside(id, aggregate.version, next_state, effect, audit, expected)
                    .await
            }
        }
    }

    /// Carry a verified fact into the boundary. The reconciler's entry point, and
    /// the one the fact-driven cancellation route runs through.
    ///
    /// # Errors
    /// As [`Self::propose`].
    pub async fn observe(
        &self,
        id: &BookingId,
        fact: Verified<VerifiedProviderFact>,
    ) -> Result<Turn, ServiceError> {
        self.settle(id, &fact).await
    }

    /// Assemble what the proposal door needs and the booking cannot know.
    async fn proposal_context(
        &self,
        booking: &Booking,
        proposal: &BookingProposal,
        version: u64,
    ) -> BookingContext {
        let selected_facts = match booking.selected_venue.as_ref() {
            Some(selection) => {
                self.availability
                    .read(&selection.venue_id, &selection.slot_id)
                    .await
            }
            None => ObservedAvailability::none(),
        };

        // The identity is derived with the repository's own function, at the
        // version being transitioned from — so the value the domain puts on the
        // state and the value `prepare_effect` derives are the same by
        // construction, and the repository still verifies it.
        let pending_effect = TownHallDomain::intended_effect_kind(&booking.state, proposal)
            .map(|kind| derive_effect_intent_id(&booking.id, kind, version));

        BookingContext {
            selected_facts,
            pending_effect,
        }
    }

    /// Phases A, B and C for a transition that reaches outside.
    async fn reach_outside(
        &self,
        id: &BookingId,
        version: u64,
        next_state: Booking,
        effect: BookingEffect,
        audit: TransitionAudit,
        expected: Option<u64>,
    ) -> Result<Turn, ServiceError> {
        // PHASE A — the intent becomes durable, and the in-flight state is
        // committed, before anything external happens (ADR-014).
        let prepared = match self
            .repository
            .prepare_effect(PrepareEffect {
                booking_id: id.clone(),
                source_version: version,
                canonical_plan: effect.clone(),
                next: next_state,
                audit,
            })
            .await
        {
            Ok(prepared) => prepared,
            Err(StoreError::StaleVersion { .. }) if expected.is_some() => {
                return Err(self.stale(id).await?);
            }
            Err(error) => return Err(error.into()),
        };

        // A replay means this effect was already prepared and may already have
        // been executed. Re-running Phase B could double-book, so recovery owns
        // it from here — and under an EXPECTED VERSION a replay is refused as
        // stale (ADR-021): the rival's prepare bumped the version without this
        // turn ever reaching the CAS, so `Unresolved` would claim work this
        // request did not do. The store's replay-first contract (M4's
        // lost-acknowledgement guarantee) and the versionless surface are
        // untouched.
        if prepared.replayed {
            if expected.is_some() {
                return Err(self.stale(id).await?);
            }
            return Ok(BoundaryOutcome::Unresolved);
        }

        let effect_id = prepared.intent.effect_intent_id.clone();

        // The coordinator is a WORKER, not an owner: it claims the same lease a
        // reconciler would, holds it across the call and Phase C, and releases it
        // after — otherwise it is an unfenced concurrent writer racing its own
        // reconciler, which can spend the budget under it and escalate an intent
        // whose very first call is still in flight (review round 2).
        let Some(claimed) = self
            .repository
            .claim_effect(&effect_id, self.config.lease_ms)
            .await?
        else {
            // Someone else already owns this turn. The intent is durable and
            // whoever holds the lease is doing exactly what we would do.
            return Ok(BoundaryOutcome::Unresolved);
        };

        let outcome = self
            .send_claimed(
                id,
                &claimed.intent,
                claimed.token,
                self.config.retry_cadence_ms,
            )
            .await;

        // The lease is given back whatever happened, so the reconciler may pick
        // the intent up at its ordinary cadence rather than waiting out a dead
        // lease.
        let _ = self
            .repository
            .release_lease(&effect_id, claimed.token)
            .await;

        // A lost row means someone else owns this turn now — Unresolved, same
        // as losing the claim.
        Ok(outcome?.unwrap_or(BoundaryOutcome::Unresolved))
    }

    /// One SEND under a held claim: mark, execute the persisted plan, verify,
    /// settle, mark finished. The only place in this crate a capability is
    /// invoked — `propose` reaches it for a freshly prepared intent, and
    /// recovery reaches it through [`Reconciliation::attend`] for an intent
    /// that is still wanted (ADR-020). `None` means the token lost the row.
    ///
    /// Each wire call is an attempt: the mark is durable BEFORE the wire
    /// (ADR-014 one level in — the row moves Prepared → Unknown here, so
    /// `Prepared` keeps meaning "never attempted" and a crash mid-call still
    /// spent budget), and the finish is recorded when the call returns control,
    /// answer or not.
    async fn send_claimed(
        &self,
        id: &BookingId,
        intent: &townhall_domain::EffectIntent,
        token: i64,
        cadence_ms: i64,
    ) -> Result<Option<Turn>, ServiceError> {
        let effect_id = intent.effect_intent_id.clone();
        if !self
            .repository
            .note_attempt_started(&effect_id, token)
            .await?
        {
            return Ok(None);
        }

        // Both halves come off the *persisted* intent, and this is the only place
        // an attempt is built. The council binds `expires_at_ms` on first sight
        // and treats it as immutable, so a capability that sourced its own would
        // bind a value this row does not hold — and every later reconciliation
        // lookup, sending this one, would be refused as a conflict forever.
        let attempt = EffectAttempt {
            id: effect_id.clone(),
            expires_at_ms: intent.expires_at_ms,
        };

        // PHASE B — outside, with no transaction open.
        let outcome = match self
            .capability
            .execute(&intent.canonical_plan, &attempt)
            .await
        {
            // Neither success nor failure. The aggregate stays in flight and
            // reconciliation resolves it; treating this as failure would return
            // the booking to a re-proposable state while the council may hold a
            // live one.
            Err(Unknown { .. }) => Ok(BoundaryOutcome::Unresolved),
            Ok(raw) => match self.verifier.verify(raw) {
                // Provenance is the verifier's to establish. A response this
                // crate cannot have verified is not evidence.
                Err(_) => Ok(BoundaryOutcome::Unresolved),
                Ok(fact) => self.settle(id, &fact).await,
            },
        };

        // The call returned control — answer or not.
        let _ = self
            .repository
            .note_attempt_finished(&effect_id, token, cadence_ms)
            .await;

        outcome.map(Some)
    }

    /// PHASE C — classify the evidence against freshly loaded state, and commit.
    ///
    /// The reload is the point. A reconciler may have moved the booking while the
    /// provider call was in flight (ADR-016's race), and `resolve_fact` is built
    /// for exactly that: the fact is re-evaluated against whatever state now
    /// holds, and `Converged` means "already applied", not "failed".
    ///
    /// The retry loop covers this phase **only**. Re-entering Phase B could
    /// execute twice, and the fact does not change on a retry — only the state it
    /// is evaluated against does.
    async fn settle(
        &self,
        id: &BookingId,
        fact: &Verified<VerifiedProviderFact>,
    ) -> Result<Turn, ServiceError> {
        for _ in 0..self.config.reclassify_attempts {
            let aggregate = self.repository.load(id).await?;
            let booking = Booking::from(&aggregate);
            let audit = TransitionAudit::driven_by(fact.get());

            let effect_id = fact.get().effect_intent_id().clone();
            let intent = self.repository.load_effect(&effect_id).await?;
            let plan_for_denials = intent.canonical_plan.clone();

            let successor_kind =
                TownHallDomain::fact_intended_effect_kind(&booking.state, fact.get());
            let context = FactContext {
                intent: Some(intent),
                pending_effect: successor_kind
                    .map(|kind| derive_effect_intent_id(id, kind, aggregate.version)),
            };

            let classified = self
                .kernel
                .resolve_fact(&self.domain, &booking, fact.clone(), &context)
                .await;

            let attempt = match classified {
                FactResolution::Undefined => {
                    if let Some(log) = &self.denials {
                        log.note_undefined(booking.state.name(), fact.get().name());
                    }
                    return Ok(BoundaryOutcome::Undefined);
                }
                FactResolution::Denied(error) => {
                    // The fact door's refusals include the most consequential
                    // ones the system can produce — DuplicateProviderEffect is
                    // one identity resolving to two council bookings — and the
                    // review that shaped ADR-017's amendment found the original
                    // design recording none of them. The principal is derived
                    // where one exists: the fact's own, else the persisted
                    // plan's, else explicitly unattributed.
                    if let Some(log) = &self.denials {
                        log.record_denied(townhall_store::denials::Denial {
                            booking_id: id.to_string(),
                            driver_kind: bld_types::Provenance::Fact,
                            driver_detail: fact.get().name(),
                            reason: error.name(),
                            principal: principal_of_fact(fact.get(), &plan_for_denials),
                        })
                        .await;
                    }
                    return Ok(BoundaryOutcome::Denied(error));
                }
                // Authoritative state already reflects the fact. Nothing to
                // write, and that is success — a reconciler re-applies facts by
                // design.
                FactResolution::Converged => return Ok(BoundaryOutcome::Converged),

                FactResolution::Ready(TransitionPlan::Local { next_state }) => {
                    let outcome = fact.get().establishes();
                    self.repository
                        .finalize_effect(FinalizeEffect {
                            booking_id: id.clone(),
                            source_version: aggregate.version,
                            effect_intent_id: effect_id,
                            status: outcome.status,
                            provider_reference: outcome.provider_reference,
                            outcome_detail: outcome.detail,
                            next: next_state,
                            audit,
                        })
                        .await
                        .map(Committed::from_finalised)
                }

                FactResolution::Ready(TransitionPlan::ExternalEffect { next_state, effect }) => {
                    let outcome = fact.get().establishes();
                    self.repository
                        .handoff_effect(HandoffEffect {
                            booking_id: id.clone(),
                            source_version: aggregate.version,
                            finalising: effect_id,
                            finalising_status: outcome.status,
                            finalising_reference: outcome.provider_reference,
                            finalising_detail: outcome.detail,
                            successor_plan: effect,
                            next: next_state,
                            audit,
                        })
                        .await
                        .map(Committed::from_handed_off)
                }
            };

            match attempt {
                // A replay means the outcome was already recorded — by a
                // concurrent turn carrying the same evidence, most likely — so
                // *this* turn wrote nothing. Reporting it as a commit would have
                // the loser of a race claim work it did not do, which is the same
                // asserted-not-derived mistake one layer up. Authoritative state
                // already reflects the fact, and that is what `Converged` says.
                Ok(Committed { replayed: true, .. }) => {
                    return Ok(BoundaryOutcome::Converged);
                }
                Ok(Committed { aggregate, .. }) => {
                    return Ok(BoundaryOutcome::Committed(aggregate));
                }
                // Someone else moved the booking between the load and the commit.
                // Classification is a pure function of state, so reloading and
                // re-asking is safe — and the successor identity is re-derived
                // from the new version, where `handoff_effect`'s idempotency key
                // catches a handoff the winner already committed.
                Err(StoreError::StaleVersion { .. }) => {}
                Err(error) => return Err(error.into()),
            }
        }

        Err(ServiceError::Contended {
            attempts: self.config.reclassify_attempts,
        })
    }

    #[must_use]
    pub fn repository(&self) -> &Arc<R> {
        &self.repository
    }

    #[must_use]
    pub fn capability(&self) -> &Arc<C> {
        &self.capability
    }
}

/// What a Phase C commit produced, and whether this turn is what produced it.
///
/// The flag is the whole point: the repository's Phase C operations are
/// idempotent, so a turn can be told "already recorded" — and a turn that wrote
/// nothing must not report a commit.
struct Committed {
    aggregate: BookingAggregate,
    replayed: bool,
}

impl Committed {
    fn from_finalised(finalised: townhall_store::FinalizedEffect) -> Self {
        Self {
            aggregate: finalised.aggregate,
            replayed: finalised.replayed,
        }
    }

    fn from_handed_off(handed: townhall_store::HandedOffEffect) -> Self {
        Self {
            aggregate: handed.aggregate,
            replayed: handed.replayed,
        }
    }
}

/// Who was refused at the fact or system-event door, where anyone knows.
///
/// The fact's own principal where it carries one (`BookingExists` does), else
/// the persisted plan's — and since slice F closed the attribution debt, EVERY
/// plan carries one: `Book` its booker, `CancelBooking` its canceller
/// (ADR-020). The amended ADR-017's empty string ("explicitly unattributed")
/// is no longer producible from a persisted plan, which was the point of the
/// stored-plan break.
fn principal_of_fact(fact: &VerifiedProviderFact, plan: &BookingEffect) -> String {
    if let VerifiedProviderFact::BookingExists { principal, .. } = fact {
        return principal.to_string();
    }
    principal_of_plan(plan)
}

/// The persisted plan's own principal — the attribution every denial on an
/// effect intent falls back to (ADR-017 as amended; ADR-020).
fn principal_of_plan(plan: &BookingEffect) -> String {
    match plan {
        BookingEffect::Book { principal, .. } | BookingEffect::CancelBooking { principal, .. } => {
            principal.to_string()
        }
    }
}

/// The effect identity a coordinator would derive for a booking at `version`.
///
/// Exposed so tests and a reconciler can name an effect without reimplementing
/// the derivation.
#[must_use]
pub fn effect_identity_for(
    id: &BookingId,
    kind: townhall_domain::OperationKind,
    version: u64,
) -> EffectIntentId {
    derive_effect_intent_id(id, kind, version)
}

// ---------------------------------------------------------------- reconciliation

/// The whole reconciliation turn, behind a surface with nothing dangerous on it.
///
/// # Why the loop is given this and nothing else
///
/// Three designs died in review before this one. A reconciler holding
/// `BookingRepository` can call `finalize_effect` with a status it invented — the
/// forbidden `BookingInProgress → AwaitingBooking` edge with no fact and no
/// domain. One holding a `Coordinator` is one public accessor from the same
/// place. One that verifies for itself must name `bld-kernel`, and can then mint
/// `Verified<EffectAbsent>` out of nothing — converting *our silence* into *the
/// council's determination*, which is the milestone's defect inside the component
/// built to prevent it.
///
/// So the loop receives [`Reconciliation::due`] and [`Reconciliation::attend`],
/// and **nothing this type returns can be used to assert anything**: identities
/// in, outcomes out. The asking, the verifying, the classifying, the committing
/// and the giving-up all happen inside, under a lease token the caller never
/// sees. Per ADR-012, this is a surface guarantee rather than a compiler one —
/// and it is stated at exactly that strength.
pub struct Reconciliation<R, C, V, A, L> {
    /// Shared with [`BookingApi`] and the composition root — one coordinator,
    /// one configuration, so the reconciler and the wire can never disagree
    /// about cadences or budgets (ADR-021).
    coordinator: Arc<Coordinator<R, C, V, A>>,
    resolver: Arc<L>,
}

/// What a send amounted to, from the reconciler's doorway.
fn attended_from(sent: Option<&Turn>, attempts_started: u32) -> Attended {
    match sent {
        // The token lost the row: someone else owns this turn now.
        None => Attended::NotDue,
        Some(BoundaryOutcome::Committed(_) | BoundaryOutcome::Converged) => Attended::Settled,
        Some(_) => Attended::StillUnknown { attempts_started },
    }
}

/// What one reconciliation turn amounted to.
///
/// Deliberately coarse. `Settled` carries no aggregate, no fact and no state
/// name: a caller with an outcome can log it and loop, and nothing more.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Attended {
    /// Not eligible right now: leased elsewhere, not yet due, or already
    /// settled. `attend` re-checks eligibility inside the claim, atomically —
    /// calling it in a tight loop with a known id cannot pump the budget.
    NotDue,
    /// The council answered; the boundary classified and committed (or found
    /// itself already converged — same thing from out here).
    Settled,
    /// No usable answer. The count advanced; the cadence decides when next.
    StillUnknown { attempts_started: u32 },
    /// This turn found the budget spent and recorded the escalation — the
    /// booking did not move, and the chase continues at the long cadence
    /// (ADR-019).
    Escalated,
}

impl<R, C, V, A, L> Reconciliation<R, C, V, A, L>
where
    R: BookingRepository,
    C: Capability<BookingEffect>,
    V: Verifier<C::Raw, VerifiedProviderFact>,
    A: AvailabilitySource,
    L: EffectResolver<C::Raw>,
{
    pub fn new(coordinator: Arc<Coordinator<R, C, V, A>>, resolver: Arc<L>) -> Self {
        Self {
            coordinator,
            resolver,
        }
    }

    /// Identities due for attention, longest-neglected first. Identities only —
    /// not intents, so the caller cannot read a canonical plan out of this and
    /// build a fact shaped like it.
    ///
    /// # Errors
    /// [`ServiceError::Store`] on a read failure.
    pub async fn due(&self, limit: u32) -> Result<Vec<EffectIntentId>, ServiceError> {
        Ok(self.coordinator.repository.due_effects(limit).await?)
    }

    /// One whole turn for one identity: claim, ask, verify, classify, commit —
    /// or escalate, if this turn finds the budget already spent.
    ///
    /// # Errors
    /// [`ServiceError::Store`] on a persistence failure mid-turn.
    pub async fn attend(&self, id: &EffectIntentId) -> Result<Attended, ServiceError> {
        let repository = &self.coordinator.repository;

        // The claim IS the eligibility check, atomically — not `due`, which only
        // suggests. `None` covers leased-elsewhere, not-yet-due and settled
        // alike, and none of them advances any count.
        let Some(claimed) = repository
            .claim_effect(id, self.coordinator.config.lease_ms)
            .await?
        else {
            return Ok(Attended::NotDue);
        };

        let turn = self.attend_claimed(id, &claimed).await;

        // The lease is given back whatever happened — including on an error path,
        // where leaving it to expire would stall the intent for the lease term
        // for no one's benefit.
        let _ = repository.release_lease(id, claimed.token).await;
        turn
    }

    async fn attend_claimed(
        &self,
        id: &EffectIntentId,
        claimed: &ClaimedEffect,
    ) -> Result<Attended, ServiceError> {
        let repository = &self.coordinator.repository;

        // Budget spent? Then this turn escalates instead of asking — ONCE, and
        // fenced: the write is conditional on the token, on `escalated_at_ms IS
        // NULL`, and on the intent still being live, so a replay, a race with a
        // settling fact, and a stale owner all collapse to a no-op.
        if claimed.attempts_started >= self.coordinator.config.attempt_budget && !claimed.escalated
        {
            return self.escalate(id, claimed).await;
        }

        let cadence = if claimed.escalated {
            self.coordinator.config.escalated_cadence_ms
        } else {
            self.coordinator.config.retry_cadence_ms
        };
        let booking_id = claimed.intent.booking_id.clone();

        // The pursuit consultation (ADR-020): may this turn CAUSE the effect,
        // or only learn its fate? The domain's table answers per state; the
        // extra bindings are conservative — a state that no longer names this
        // intent, or names it as a different kind, wants nothing sent. Read
        // once, before the wire: a withdrawal that lands mid-turn is the
        // best-effort case ADR-020 accepts, bounded by the deadline and healed
        // by the fact arms.
        let state = Booking::from(&repository.load(&booking_id).await?).state;
        let wanted = state.effect_intent_id() == Some(id)
            && state.in_flight_kind() == Some(claimed.intent.operation_kind)
            && matches!(
                state.pursuit(),
                Some(townhall_domain::Pursuit::SendAndResolve)
            );

        // Never attempted, and still wanted: recovery's FIRST-SEND leg —
        // the crash window between a handoff's commit and its successor's
        // mark, resumed under the same identity (test 12's sentence).
        if claimed.intent.status == townhall_domain::EffectStatus::Prepared && wanted {
            return Ok(attended_from(
                self.coordinator
                    .send_claimed(&booking_id, &claimed.intent, claimed.token, cadence)
                    .await?
                    .as_ref(),
                claimed.attempts_started + 1,
            ));
        }

        // The QUERY — its own attempt, marked before the wire like any other.
        if !repository.note_attempt_started(id, claimed.token).await? {
            return Ok(Attended::NotDue);
        }
        let attempt = EffectAttempt {
            id: id.clone(),
            expires_at_ms: claimed.intent.expires_at_ms,
        };
        let asked = self
            .resolver
            .resolve(&attempt, claimed.intent.operation_kind)
            .await;
        let _ = repository
            .note_attempt_finished(id, claimed.token, cadence)
            .await;

        match asked {
            // The reconciler never interprets the fact — the coordinator's
            // settle path asks the domain and commits, exactly as it would for
            // a fact that arrived any other way.
            Ok(Resolved::Answer(raw)) => match self.coordinator.verifier.verify(raw) {
                Ok(fact) => match self.coordinator.settle(&booking_id, &fact).await? {
                    BoundaryOutcome::Committed(_) | BoundaryOutcome::Converged => {
                        Ok(Attended::Settled)
                    }
                    // `Denied`/`Undefined` here mean the fact did not fit the
                    // state — a wrong-kind answer, a stale fact. Nothing was
                    // asserted, nothing committed; the intent stays live and
                    // the cadence decides when to ask again.
                    _ => Ok(Attended::StillUnknown {
                        attempts_started: claimed.attempts_started + 1,
                    }),
                },
                Err(_) => Ok(Attended::StillUnknown {
                    attempts_started: claimed.attempts_started + 1,
                }),
            },
            // The council's authenticated, identity-bound "nothing yet" — and
            // the state still wants the effect: RESEND the persisted plan under
            // the same identity (ADR-020). A second wire call, so a second
            // attempt on the books.
            Ok(Resolved::NotYetVisible) if wanted => Ok(attended_from(
                self.coordinator
                    .send_claimed(&booking_id, &claimed.intent, claimed.token, cadence)
                    .await?
                    .as_ref(),
                claimed.attempts_started + 2,
            )),
            // "Nothing yet" for an effect nobody wants caused (the deadline
            // will end that story — ADR-016), or no usable reply at all:
            // honestly unknown either way, and nothing is sent.
            Ok(Resolved::NotYetVisible) | Err(Unknown { .. }) => Ok(Attended::StillUnknown {
                attempts_started: claimed.attempts_started + 1,
            }),
        }
    }

    /// Give up chasing at retry cadence — with the domain consulted first,
    /// because only the domain can say whether exhaustion is even coherent at
    /// this state (ADR-013). `Record` moves nothing: the write is a pursuit
    /// marker on the intent, the count derived in the write itself, and the
    /// booking stays exactly where it is (ADR-019).
    async fn escalate(
        &self,
        id: &EffectIntentId,
        claimed: &ClaimedEffect,
    ) -> Result<Attended, ServiceError> {
        let repository = &self.coordinator.repository;
        let aggregate = repository.load(&claimed.intent.booking_id).await?;
        let booking = Booking::from(&aggregate);

        let event = SystemEvent::ReconciliationExhausted {
            effect_intent_id: id.clone(),
        };
        match self
            .coordinator
            .kernel
            .resolve_system_event(&self.coordinator.domain, &booking, event)
            .await
        {
            bld_kernel::SystemEventResolution::Record => {
                let wrote = repository
                    .mark_escalated(
                        id,
                        claimed.token,
                        self.coordinator.config.escalated_cadence_ms,
                    )
                    .await?;
                Ok(match wrote {
                    townhall_store::EscalationWrite::Recorded => Attended::Escalated,
                    townhall_store::EscalationWrite::Noop => Attended::NotDue,
                })
            }
            // The aggregate is no longer waiting on this effect (or is
            // incoherent, which is an operator matter below the domain). Nothing
            // to escalate; nothing is written — but a refusal at this door is
            // still a refusal, and it records like any other.
            bld_kernel::SystemEventResolution::Denied(error) => {
                if let Some(log) = &self.coordinator.denials {
                    log.record_denied(townhall_store::denials::Denial {
                        booking_id: claimed.intent.booking_id.to_string(),
                        driver_kind: bld_types::Provenance::SystemEvent,
                        driver_detail: "ReconciliationExhausted",
                        reason: error.name(),
                        principal: principal_of_plan(&claimed.intent.canonical_plan),
                    })
                    .await;
                }
                Ok(Attended::NotDue)
            }
            bld_kernel::SystemEventResolution::Undefined => {
                if let Some(log) = &self.coordinator.denials {
                    log.note_undefined(booking.state.name(), "ReconciliationExhausted");
                }
                Ok(Attended::NotDue)
            }
        }
    }
}

// ------------------------------------------------------------------- the facade

/// The application boundary the wire talks to (ADR-021).
///
/// Handlers hold this and nothing else, so "handlers do not mutate directly"
/// (the M5 gate's third clause) is a fact about what their code can express:
/// the complete mutation surface is [`BookingApi::create`] and
/// [`BookingApi::propose_at`], both of which run the full coordinator turn.
/// Everything else is a read or the reconcile trigger, which can only ask.
pub struct BookingApi<R, C, V, A, L> {
    coordinator: Arc<Coordinator<R, C, V, A>>,
    reconciliation: Arc<Reconciliation<R, C, V, A, L>>,
    /// The browse catalogue — dyn, because browsing carries no authority worth
    /// a generic parameter.
    catalogue: Arc<dyn CatalogueSource>,
    /// The per-slot verified availability, projected WITHOUT its grant: grants
    /// live in persisted plans and never in a response body.
    availability: Arc<dyn AvailabilitySource>,
}

/// The facade's closed error vocabulary — everything an adapter needs to map,
/// with no store type on the surface (the crate boundary is the point: the
/// HTTP crate cannot name `StoreError`, so this enum is its whole world).
#[derive(Debug, Error)]
pub enum ApiError {
    #[error("no such booking")]
    UnknownBooking,
    #[error("the booking already exists at version {current}")]
    AlreadyExists { current: u64 },
    #[error("expected version is stale; the current version is {current}")]
    PreconditionFailed { current: u64 },
    #[error("gave up re-classifying under contention")]
    Contended,
    /// An internal invariant failed — never dressed as provider trouble
    /// (ADR-021: this is a 500, not a 503).
    #[error("internal invariant failure: {0}")]
    Internal(String),
    /// The store or another prerequisite could not answer at all.
    #[error("infrastructure unavailable: {0}")]
    Unavailable(String),
}

impl ApiError {
    fn from_service(error: ServiceError) -> Self {
        match error {
            ServiceError::PreconditionFailed { current } => Self::PreconditionFailed { current },
            ServiceError::Contended { .. } => Self::Contended,
            ServiceError::UnexpectedPlan { reason } => Self::Internal(reason.to_owned()),
            ServiceError::Store(StoreError::NotFound(_) | StoreError::EffectNotFound(_)) => {
                Self::UnknownBooking
            }
            ServiceError::Store(error) => Self::Unavailable(error.to_string()),
        }
    }
}

/// What one versioned mutation amounted to, with everything a response needs:
/// the typed outcome, the version an `ETag` reports, and — when the turn is
/// `Unresolved` — the STORE's own schedule for the next attempt, which is what
/// an honest `Retry-After` projects (ADR-021).
#[derive(Debug)]
pub struct Mutated {
    pub outcome: Turn,
    pub current_version: u64,
    pub retry_after_ms: Option<i64>,
}

/// The read projection: the aggregate's client-visible fields plus the
/// domain's exported behaviour MENU — the same table the topology docs are
/// generated from, so what the API advertises and what the matrix permits are
/// one fact (ADR-018's export, consumed).
#[derive(Clone, Debug)]
pub struct Projection {
    pub id: BookingId,
    pub version: u64,
    pub state: &'static str,
    pub requirements: bld_types::BookingRequirements,
    pub selected_venue: Option<townhall_domain::SelectedVenueRef>,
    pub booking_ref: Option<bld_types::CouncilBookingRef>,
    pub available_behaviours: &'static [&'static str],
}

/// Per-slot verified facts as the wire sees them: everything the council
/// signed EXCEPT the grant, which lives in persisted plans only.
#[derive(Clone, Debug)]
pub struct SlotFacts {
    pub venue_id: String,
    pub slot_id: String,
    pub capacity: u16,
    pub accessible: bool,
    pub fee_pence: u64,
    pub available: bool,
}

/// One audit row, as the wire may see it — a service-owned projection, not the
/// store's record (re-exporting a storage type would hide the import, not the
/// contract).
#[derive(Clone, Debug)]
pub struct AuditEntry {
    pub driver_kind: &'static str,
    pub driver_detail: String,
    pub outcome: String,
    pub from_version: u64,
    pub to_version: u64,
    pub at_ms: i64,
}

impl<R, C, V, A, L> BookingApi<R, C, V, A, L>
where
    R: BookingRepository,
    C: Capability<BookingEffect>,
    V: Verifier<C::Raw, VerifiedProviderFact>,
    A: AvailabilitySource,
    L: EffectResolver<C::Raw>,
{
    pub fn new(
        coordinator: Arc<Coordinator<R, C, V, A>>,
        reconciliation: Arc<Reconciliation<R, C, V, A, L>>,
        catalogue: Arc<dyn CatalogueSource>,
        availability: Arc<dyn AvailabilitySource>,
    ) -> Self {
        Self {
            coordinator,
            reconciliation,
            catalogue,
            availability,
        }
    }

    /// Browse the catalogue, filtered here so the wire's query never reaches
    /// the provider. `None` from the source means it could not be asked (503).
    ///
    /// # Errors
    /// [`ApiError::Unavailable`] when the catalogue cannot be asked.
    pub async fn venues(&self, filters: VenueFilters) -> Result<Vec<VenueSummary>, ApiError> {
        let Some(rows) = self.catalogue.venues().await else {
            return Err(ApiError::Unavailable(
                "the catalogue could not be asked".to_owned(),
            ));
        };
        Ok(rows
            .into_iter()
            .filter(|row| {
                filters
                    .attendees
                    .is_none_or(|needed| row.capacity >= u64::from(needed))
                    && filters
                        .accessible
                        .is_none_or(|needed| !needed || row.accessible)
                    && filters
                        .max_fee_pence
                        .is_none_or(|ceiling| row.fee_pence <= ceiling)
            })
            .collect())
    }

    /// The per-slot verified facts, grant withheld. `None` facts is an ANSWER;
    /// an unreachable provider is [`ApiError::Unavailable`] (ADR-021).
    ///
    /// # Errors
    /// [`ApiError::Unavailable`].
    pub async fn slot_facts(
        &self,
        venue: &VenueId,
        slot: &SlotId,
    ) -> Result<Option<SlotFacts>, ApiError> {
        match self.availability.read(venue, slot).await {
            ObservedAvailability::Unavailable => Err(ApiError::Unavailable(
                "the availability provider could not be asked".to_owned(),
            )),
            ObservedAvailability::Answered(observation) => Ok(observation.map(|verified| {
                let facts = &verified.get().facts;
                SlotFacts {
                    venue_id: facts.venue_id.to_string(),
                    slot_id: facts.slot_id.to_string(),
                    capacity: facts.capacity,
                    accessible: facts.wheelchair_accessible,
                    fee_pence: facts.fee.pence(),
                    available: facts.available,
                }
            })),
        }
    }

    /// Create the durable booking intent. No precondition applies — a new
    /// resource has no version to match (ADR-021).
    ///
    /// # Errors
    /// [`ApiError::AlreadyExists`] with the existing version (a 409 that ships
    /// the existing `ETag`); [`ApiError::Unavailable`] for store failure.
    pub async fn create(
        &self,
        id: BookingId,
        requirements: bld_types::BookingRequirements,
    ) -> Result<Projection, ApiError> {
        let repository = self.coordinator.repository();
        match repository
            .create(townhall_store::NewBooking {
                id: id.clone(),
                requirements,
            })
            .await
        {
            Ok(aggregate) => Ok(Self::project(&aggregate)),
            Err(StoreError::AlreadyExists(_)) => {
                let current = repository
                    .load(&id)
                    .await
                    .map_err(|error| ApiError::Unavailable(error.to_string()))?
                    .version;
                Err(ApiError::AlreadyExists { current })
            }
            Err(error) => Err(ApiError::Unavailable(error.to_string())),
        }
    }

    /// The authoritative projection, with the state's exported menu.
    ///
    /// # Errors
    /// [`ApiError::UnknownBooking`]; [`ApiError::Unavailable`].
    pub async fn read(&self, id: &BookingId) -> Result<Projection, ApiError> {
        let aggregate = self.load(id).await?;
        Ok(Self::project(&aggregate))
    }

    /// One versioned mutation turn — see [`Coordinator::propose_at`].
    ///
    /// # Errors
    /// [`ApiError::PreconditionFailed`] carrying the current version;
    /// otherwise as [`ApiError::from_service`] maps them.
    pub async fn propose_at(
        &self,
        id: &BookingId,
        expected_version: u64,
        proposal: BookingProposal,
        authority: &VerifiedAuthority,
    ) -> Result<Mutated, ApiError> {
        let outcome = self
            .coordinator
            .propose_at(id, expected_version, proposal, authority)
            .await
            .map_err(ApiError::from_service)?;

        // The ETag after the turn, and — only for an unresolved turn — the
        // store's own schedule for the next attempt.
        let (current_version, retry_after_ms) = match &outcome {
            BoundaryOutcome::Committed(aggregate) => (aggregate.version, None),
            BoundaryOutcome::Unresolved => {
                let aggregate = self.load(id).await?;
                let hint = match &aggregate.active_effect {
                    Some(effect) => self
                        .coordinator
                        .repository()
                        .retry_hint_ms(effect)
                        .await
                        .map_err(|error| ApiError::Unavailable(error.to_string()))?,
                    None => None,
                };
                (aggregate.version, hint)
            }
            _ => (self.load(id).await?.version, None),
        };
        Ok(Mutated {
            outcome,
            current_version,
            retry_after_ms,
        })
    }

    /// The typed audit trail, projected.
    ///
    /// # Errors
    /// [`ApiError::UnknownBooking`]; [`ApiError::Unavailable`].
    pub async fn audit(&self, id: &BookingId) -> Result<Vec<AuditEntry>, ApiError> {
        // Reads on an unknown booking answer 404, not an empty trail.
        self.load(id).await?;
        let events = self
            .coordinator
            .repository()
            .audit_events(id)
            .await
            .map_err(|error| ApiError::Unavailable(error.to_string()))?;
        Ok(events
            .into_iter()
            .map(|event| AuditEntry {
                driver_kind: event.driver_kind.name(),
                driver_detail: event.driver_detail,
                outcome: event.outcome,
                from_version: event.from_version,
                to_version: event.to_version,
                at_ms: event.created_at_ms,
            })
            .collect())
    }

    /// The spec's demo/admin reconcile trigger: drive THIS booking's chase to
    /// quiescence through the reconciler's own surface — `attend`, never
    /// `propose` — following handoffs to their successors (bounded). Exempt
    /// from preconditions by classification (ADR-021): it asserts no expected
    /// state; the claim is atomic and the facts are version-fenced below.
    ///
    /// # Errors
    /// [`ApiError::UnknownBooking`]; [`ApiError::Unavailable`].
    pub async fn attend_booking(&self, id: &BookingId) -> Result<Vec<Attended>, ApiError> {
        let mut outcomes = Vec::new();
        for _ in 0..4 {
            let Some(effect) = self.load(id).await?.active_effect else {
                break;
            };
            let attended = self
                .reconciliation
                .attend(&effect)
                .await
                .map_err(ApiError::from_service)?;
            let done = attended == Attended::NotDue;
            outcomes.push(attended);
            if done {
                break;
            }
        }
        Ok(outcomes)
    }

    async fn load(&self, id: &BookingId) -> Result<BookingAggregate, ApiError> {
        match self.coordinator.repository().load(id).await {
            Ok(aggregate) => Ok(aggregate),
            Err(StoreError::NotFound(_)) => Err(ApiError::UnknownBooking),
            Err(error) => Err(ApiError::Unavailable(error.to_string())),
        }
    }

    fn project(aggregate: &BookingAggregate) -> Projection {
        Projection {
            id: aggregate.id.clone(),
            version: aggregate.version,
            state: aggregate.state.name(),
            requirements: aggregate.requirements.clone(),
            selected_venue: aggregate.selected_venue.clone(),
            booking_ref: aggregate.booking_ref.clone(),
            available_behaviours: aggregate.state.proposal_menu(),
        }
    }
}


/// The facade, object-safe — the shape `townhall-http` holds, so the adapter
/// crate never names a store, client, or even the facade's generics.
#[async_trait]
pub trait BookingFacade: Send + Sync {
    async fn create(
        &self,
        id: BookingId,
        requirements: bld_types::BookingRequirements,
    ) -> Result<Projection, ApiError>;
    async fn read(&self, id: &BookingId) -> Result<Projection, ApiError>;
    async fn propose_at(
        &self,
        id: &BookingId,
        expected_version: u64,
        proposal: BookingProposal,
        authority: &VerifiedAuthority,
    ) -> Result<Mutated, ApiError>;
    async fn audit(&self, id: &BookingId) -> Result<Vec<AuditEntry>, ApiError>;
    async fn attend_booking(&self, id: &BookingId) -> Result<Vec<Attended>, ApiError>;
    async fn venues(&self, filters: VenueFilters) -> Result<Vec<VenueSummary>, ApiError>;
    async fn slot_facts(
        &self,
        venue: &VenueId,
        slot: &SlotId,
    ) -> Result<Option<SlotFacts>, ApiError>;
}

#[async_trait]
impl<R, C, V, A, L> BookingFacade for BookingApi<R, C, V, A, L>
where
    R: BookingRepository,
    C: Capability<BookingEffect>,
    V: Verifier<C::Raw, VerifiedProviderFact>,
    A: AvailabilitySource,
    L: EffectResolver<C::Raw>,
{
    async fn create(
        &self,
        id: BookingId,
        requirements: bld_types::BookingRequirements,
    ) -> Result<Projection, ApiError> {
        Self::create(self, id, requirements).await
    }
    async fn read(&self, id: &BookingId) -> Result<Projection, ApiError> {
        Self::read(self, id).await
    }
    async fn propose_at(
        &self,
        id: &BookingId,
        expected_version: u64,
        proposal: BookingProposal,
        authority: &VerifiedAuthority,
    ) -> Result<Mutated, ApiError> {
        Self::propose_at(self, id, expected_version, proposal, authority).await
    }
    async fn audit(&self, id: &BookingId) -> Result<Vec<AuditEntry>, ApiError> {
        Self::audit(self, id).await
    }
    async fn attend_booking(&self, id: &BookingId) -> Result<Vec<Attended>, ApiError> {
        Self::attend_booking(self, id).await
    }
    async fn venues(&self, filters: VenueFilters) -> Result<Vec<VenueSummary>, ApiError> {
        Self::venues(self, filters).await
    }
    async fn slot_facts(
        &self,
        venue: &VenueId,
        slot: &SlotId,
    ) -> Result<Option<SlotFacts>, ApiError> {
        Self::slot_facts(self, venue, slot).await
    }
}

/// The reconciler's loop surface, object-safe — due and attend, nothing else,
/// exactly as ADR-019 scoped it.
#[async_trait]
pub trait ReconcilerHandle: Send + Sync {
    async fn due(&self, limit: u32) -> Result<Vec<EffectIntentId>, ServiceError>;
    async fn attend(&self, id: &EffectIntentId) -> Result<Attended, ServiceError>;
}

#[async_trait]
impl<R, C, V, A, L> ReconcilerHandle for Reconciliation<R, C, V, A, L>
where
    R: BookingRepository,
    C: Capability<BookingEffect>,
    V: Verifier<C::Raw, VerifiedProviderFact>,
    A: AvailabilitySource,
    L: EffectResolver<C::Raw>,
{
    async fn due(&self, limit: u32) -> Result<Vec<EffectIntentId>, ServiceError> {
        Self::due(self, limit).await
    }
    async fn attend(&self, id: &EffectIntentId) -> Result<Attended, ServiceError> {
        Self::attend(self, id).await
    }
}
