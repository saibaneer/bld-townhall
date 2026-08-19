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
use bld_types::{BookingId, EffectIntentId, SlotId, VenueId};
use std::sync::Arc;
use thiserror::Error;
use townhall_domain::{
    Booking, BookingAggregate, BookingContext, BookingEffect, BookingError, BookingProposal,
    FactContext, SystemEvent, TownHallDomain, VenueFacts, VerifiedAuthority, VerifiedProviderFact,
};
use townhall_store::{
    BookingRepository, FinalizeEffect, HandoffEffect, PrepareEffect, StoreError, TransitionAudit,
    derive_effect_intent_id,
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
#[async_trait]
pub trait AvailabilitySource: Send + Sync {
    async fn read(&self, venue: &VenueId, slot: &SlotId) -> Option<VenueFacts>;
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

/// Sequences the three phases around one proposal.
pub struct Coordinator<R, C, V, A> {
    repository: Arc<R>,
    capability: Arc<C>,
    verifier: Arc<V>,
    availability: Arc<A>,
    kernel: Kernel,
    domain: TownHallDomain,
    /// How many times Phase C may reload and re-classify after losing a
    /// compare-and-set.
    ///
    /// Correctness does not depend on this number. What makes the bound safe is
    /// that the intent is durable and reconciliation owns anything unfinished, so
    /// exhausting the budget reports the current state rather than inventing an
    /// outcome. Three is an availability choice, not a correctness argument.
    attempts: u32,
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
            attempts: 3,
        }
    }

    /// Override the Phase C attempt budget. For tests that want contention to be
    /// deterministic rather than lucky.
    #[must_use]
    pub const fn with_attempts(mut self, attempts: u32) -> Self {
        self.attempts = attempts;
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
        let aggregate = self.repository.load(id).await?;
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
            // Nothing ran and nothing was touched. ADR-017's recording of these
            // is a separate concern and is not wired here yet.
            Resolution::Undefined => Ok(BoundaryOutcome::Undefined),
            Resolution::Denied(error) => Ok(BoundaryOutcome::Denied(error)),

            Resolution::Ready(TransitionPlan::Local { next_state }) => {
                let committed = self
                    .repository
                    .commit(id, aggregate.version, next_state, audit)
                    .await?;
                Ok(BoundaryOutcome::Committed(committed))
            }

            Resolution::Ready(TransitionPlan::ExternalEffect { next_state, effect }) => {
                self.reach_outside(id, aggregate.version, next_state, effect, audit)
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

    /// Carry a runtime determination into the boundary.
    ///
    /// # Errors
    /// [`ServiceError::Store`] for a persistence failure.
    pub async fn record(&self, id: &BookingId, event: SystemEvent) -> Result<Turn, ServiceError> {
        let aggregate = self.repository.load(id).await?;
        let booking = Booking::from(&aggregate);
        let audit = TransitionAudit::driven_by(&event);
        // Read before classifying: the effect this event is about is the one the
        // state is waiting on, and the domain refuses the event outright if there
        // is none.
        let in_flight = booking.active_effect.clone();

        match self
            .kernel
            .resolve_system_event(&self.domain, &booking, event)
            .await
        {
            Resolution::Undefined => Ok(BoundaryOutcome::Undefined),
            Resolution::Denied(error) => Ok(BoundaryOutcome::Denied(error)),
            Resolution::Ready(TransitionPlan::Local { next_state }) => {
                // Giving up finalises the effect it gave up on: the intent must
                // stop looking live, or a reconciler would keep chasing what this
                // event just abandoned.
                let Some(effect) = in_flight else {
                    return Err(ServiceError::UnexpectedPlan {
                        reason: "a system event moved a state with no effect in flight",
                    });
                };
                let finalised = self
                    .repository
                    .finalize_effect(FinalizeEffect {
                        booking_id: id.clone(),
                        source_version: aggregate.version,
                        effect_intent_id: effect,
                        // Reconciliation ran out of attempts without ever
                        // establishing an outcome. `Absent` is the only terminal
                        // status that claims nothing about the provider — we are
                        // recording that WE gave up, not that the council refused.
                        status: townhall_domain::EffectStatus::Absent,
                        provider_reference: None,
                        outcome_detail: Some(bld_types::BoundedString::truncating(
                            "reconciliation exhausted; escalated for human resolution",
                        )),
                        next: next_state,
                        audit,
                    })
                    .await?;
                Ok(BoundaryOutcome::Committed(finalised.aggregate))
            }
            // A system event never plans an external effect. Refused by name
            // rather than guessed at, so a future edit that changed that would
            // fail loudly here instead of quietly reaching outside.
            Resolution::Ready(TransitionPlan::ExternalEffect { .. }) => {
                Err(ServiceError::UnexpectedPlan {
                    reason: "a system event asked to reach outside the boundary",
                })
            }
        }
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
            None => None,
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
    ) -> Result<Turn, ServiceError> {
        // PHASE A — the intent becomes durable, and the in-flight state is
        // committed, before anything external happens (ADR-014).
        let prepared = self
            .repository
            .prepare_effect(PrepareEffect {
                booking_id: id.clone(),
                source_version: version,
                canonical_plan: effect.clone(),
                next: next_state,
                audit,
            })
            .await?;

        // A replay means this effect was already prepared and may already have
        // been executed. Re-running Phase B could double-book, so recovery owns it
        // from here.
        if prepared.replayed {
            return Ok(BoundaryOutcome::Unresolved);
        }

        let effect_id = prepared.intent.effect_intent_id.clone();

        // PHASE B — outside, with no transaction open.
        let raw = match self.capability.execute(&effect, &effect_id).await {
            Ok(raw) => raw,
            // Neither success nor failure. The aggregate stays in flight and
            // reconciliation resolves it; treating this as failure would return
            // the booking to a re-proposable state while the council may hold a
            // live one.
            Err(Unknown { .. }) => return Ok(BoundaryOutcome::Unresolved),
        };

        // Provenance is the verifier's to establish. A response this crate cannot
        // have verified is not evidence, and a coordinator that concluded anything
        // from it would be asserting provenance it does not have.
        let Ok(fact) = self.verifier.verify(raw) else {
            return Ok(BoundaryOutcome::Unresolved);
        };

        self.settle(id, &fact).await
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
        for _ in 0..self.attempts {
            let aggregate = self.repository.load(id).await?;
            let booking = Booking::from(&aggregate);
            let audit = TransitionAudit::driven_by(fact.get());

            let effect_id = fact.get().effect_intent_id().clone();
            let intent = self.repository.load_effect(&effect_id).await?;

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
                FactResolution::Undefined => return Ok(BoundaryOutcome::Undefined),
                FactResolution::Denied(error) => return Ok(BoundaryOutcome::Denied(error)),
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
            attempts: self.attempts,
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
