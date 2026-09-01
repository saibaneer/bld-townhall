//! An in-process council, and the verifier for its responses.
//!
//! Public rather than `#[cfg(test)]` on purpose: slice C's gate needs it, D
//! compares the real council against it, and E needs a capability it can make
//! misbehave. A test-only type would be built three times.
//!
//! It implements **provider-side idempotency keyed on `EffectIntentId`** — the
//! property D's real council must also have, proven here first, where a failing
//! test means the protocol is wrong rather than an HTTP client misconfigured.
//!
//! What it deliberately does **not** do is enforce expiry. ADR-016's expiry
//! semantics are the council's obligation and slice D builds them; claiming this
//! fake proves them would be an overclaim.

use async_trait::async_trait;
use bld_kernel::{Capability, Unknown, VerificationError, Verified, Verifier};
use bld_types::{
    AvailabilityGrant, BoundedString, CouncilBookingRef, EffectAttempt, EffectIntentId, Money,
    PrincipalId,
};
use std::{
    collections::HashMap,
    sync::{Arc, Mutex},
};
use townhall_domain::{BookingEffect, VerifiedProviderFact};

/// What the council was asked to do, recorded in the order it was asked.
///
/// `expires_at_ms` is recorded, not just accepted, so a test can assert the
/// deadline that arrived is the one the intent row holds. A fake that took the
/// attempt and dropped it would let a capability substitute a deadline and no
/// test would notice — which is exactly the defect the envelope exists to close.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Call {
    pub effect_intent_id: EffectIntentId,
    pub expires_at_ms: i64,
    pub plan: BookingEffect,
}

/// What the fake should do when next called.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Script {
    /// Behave: create or cancel, idempotently.
    Succeed,
    /// Refuse authoritatively and permanently. Travels as a *response*, so it
    /// goes through the verifier like any other — a capability error could never
    /// become a durable rejection (ADR-012).
    RefusePermanently(&'static str),
    /// Refuse for a reason that says nothing about whether the effect happened:
    /// a validation error, a rate limit, a "try again". Must never become a
    /// rejection.
    RefuseTemporarily(&'static str),
    /// Answer nothing at all. Neither success nor failure.
    GoQuiet(&'static str),
    /// Do the work — the booking is created, exactly as an honest call would —
    /// and then answer nothing. The dropped-response scenario: the council
    /// committed, the network ate the answer, and only a later lookup under the
    /// same identity can discover what happened.
    SucceedThenGoQuiet(&'static str),
    /// Return a response that cannot be attributed to the council.
    Forge,
}

/// The council's unexamined answer. Carries success *and* authoritative refusal,
/// because both are things the council said.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct RawResponse {
    pub effect_intent_id: EffectIntentId,
    pub body: RawBody,
    /// Whether this response carries the council's attestation. A forged one does
    /// not, and the verifier refuses it — this stands in for a signature or a
    /// mutually-authenticated channel.
    pub attested: bool,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum RawBody {
    Created {
        booking_ref: CouncilBookingRef,
        principal: PrincipalId,
        venue_id: String,
        slot_id: String,
        attendees: u16,
        fee: Money,
    },
    Cancelled {
        booking_ref: CouncilBookingRef,
    },
    /// The council will never do this, for this identity, ever. Tombstoned.
    RefusedPermanently {
        reason: &'static str,
    },
    /// The council declined this attempt. It says nothing about whether the
    /// effect exists.
    RefusedForNow {
        reason: &'static str,
    },
}

/// Which wire the council was reached over — an ask or a cause.
///
/// The interleaved log exists for one discriminating witness (ADR-020): on a
/// resend path, the resolve must strictly precede the execute, because a blind
/// resend that never asks is exactly what same-identity idempotency would
/// otherwise hide in every happy-path test.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum WireOp {
    Resolve,
    Execute,
}

/// A council that keeps one booking per effect identity.
#[derive(Debug, Default)]
pub struct FakeCouncil {
    inner: Mutex<Inner>,
}

#[derive(Debug, Default)]
struct Inner {
    /// Provider-side idempotency: one identity, one booking, forever.
    created: HashMap<String, CouncilBookingRef>,
    cancelled: HashMap<String, CouncilBookingRef>,
    calls: Vec<Call>,
    script: Vec<Script>,
    next_reference: u32,
    /// First-seen binding, exactly as the real council binds (slice D): kind
    /// and deadline are recorded on first sight — execute or resolve alike —
    /// and immutable after. A later envelope that contradicts the binding is
    /// answered as unusable, the same protocol history the real council
    /// refuses with `ProtocolConflict`. Without this, the fake would accept
    /// histories the real council rejects, and every reply became
    /// consequential the moment `NotYetVisible` could authorize a resend.
    bound: HashMap<String, (townhall_domain::OperationKind, i64)>,
    /// Every wire arrival, in order, both kinds — see [`WireOp`].
    wire_log: Vec<(WireOp, String)>,
    /// When armed, every execute waits here before doing its work — the
    /// blocking fake gate M3's composed race needs: a call held past its
    /// lease's expiry while a second worker moves.
    execute_gate: Option<std::sync::Arc<ExecuteGate>>,
}

/// The blocking fake's synchronisation, both directions signalled — a test
/// AWAITS `arrived` (the call is genuinely on the wire) and PERMITS `release`
/// (the council finally answers), so no participant ever sleeps.
#[derive(Debug)]
pub struct ExecuteGate {
    pub arrived: tokio::sync::Semaphore,
    pub release: tokio::sync::Semaphore,
}

impl Default for ExecuteGate {
    fn default() -> Self {
        Self {
            arrived: tokio::sync::Semaphore::new(0),
            release: tokio::sync::Semaphore::new(0),
        }
    }
}

impl Inner {
    /// Enforce the first-seen binding for one arriving envelope.
    fn bind(
        &mut self,
        id: &str,
        kind: townhall_domain::OperationKind,
        expires_at_ms: i64,
    ) -> Result<(), Unknown> {
        match self.bound.get(id) {
            None => {
                self.bound.insert(id.to_owned(), (kind, expires_at_ms));
                Ok(())
            }
            Some((bound_kind, bound_expiry))
                if *bound_kind == kind && *bound_expiry == expires_at_ms =>
            {
                Ok(())
            }
            Some(_) => Err(Unknown::new(BoundedString::truncating(
                "the envelope contradicts what the council already bound",
            ))),
        }
    }
}

impl FakeCouncil {
    #[must_use]
    pub fn new() -> Self {
        Self::default()
    }

    /// Queue what the council should do, one entry per call. Once the queue is
    /// empty it succeeds.
    pub fn script(&self, steps: impl IntoIterator<Item = Script>) {
        let mut inner = self.lock();
        inner.script.extend(steps);
    }

    /// Everything the council has been asked to do, in order.
    #[must_use]
    pub fn calls(&self) -> Vec<Call> {
        self.lock().calls.clone()
    }

    /// How many times the council has been called at all. `0` is the assertion
    /// that matters for a crash-before-the-call test.
    #[must_use]
    pub fn call_count(&self) -> usize {
        self.lock().calls.len()
    }

    /// How many bookings actually exist. Idempotency means a retried identity
    /// must not move this.
    #[must_use]
    pub fn booking_count(&self) -> usize {
        self.lock().created.len()
    }

    /// Every wire arrival in order — asks and causes interleaved. The witness
    /// for "the resolve strictly precedes the execute" on a resend path.
    #[must_use]
    pub fn wire_log(&self) -> Vec<(WireOp, String)> {
        self.lock().wire_log.clone()
    }

    /// Arm the execute gate: every subsequent `execute` announces its arrival
    /// on `gate.arrived` (one permit per call, for the test to await — never a
    /// sleep) and then waits for a permit on `gate.release` before doing its
    /// work. This is how a test holds a call past its lease's expiry.
    pub fn gate_executes(&self, gate: std::sync::Arc<ExecuteGate>) {
        self.lock().execute_gate = Some(gate);
    }

    fn lock(&self) -> std::sync::MutexGuard<'_, Inner> {
        self.inner
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
    }
}

/// A hook that runs *while* the council is being called.
///
/// This is what lets a test observe the world mid-call: that the intent is
/// already durable, and that no transaction is being held across the call.
pub type DuringCall = Arc<dyn Fn(&EffectIntentId) + Send + Sync>;

/// The council plus a hook, for tests that need to look around mid-call.
pub struct ObservedCouncil {
    council: Arc<FakeCouncil>,
    during: DuringCall,
}

impl ObservedCouncil {
    #[must_use]
    pub fn new(council: Arc<FakeCouncil>, during: DuringCall) -> Self {
        Self { council, during }
    }
}

#[async_trait]
impl Capability<BookingEffect> for ObservedCouncil {
    type Raw = RawResponse;

    async fn execute(
        &self,
        effect: &BookingEffect,
        attempt: &EffectAttempt,
    ) -> Result<Self::Raw, Unknown> {
        (self.during)(&attempt.id);
        self.council.execute(effect, attempt).await
    }
}

#[async_trait]
impl crate::EffectResolver<RawResponse> for FakeCouncil {
    /// Ask what became of an identity. The fake's honest answers only: a created
    /// booking or cancellation is returned, and an identity it holds nothing
    /// settled for is `NotYetVisible` — honest, because that claims nothing
    /// about absence, which this fake deliberately cannot determine (no expiry;
    /// see the module header). The fake IS the council in-process, so its
    /// not-yet is authentic and identity-bound by construction — the bar the
    /// trait states is met trivially rather than cryptographically. The script
    /// is consulted first, so a test can make the lookup itself misbehave; a
    /// turn that queries and then resends consumes TWO script operations from
    /// this one shared queue.
    async fn resolve(
        &self,
        attempt: &EffectAttempt,
        kind: townhall_domain::OperationKind,
    ) -> Result<crate::Resolved<RawResponse>, Unknown> {
        let mut inner = self.lock();
        let id = &attempt.id;
        inner
            .wire_log
            .push((WireOp::Resolve, id.as_str().to_owned()));
        inner.bind(id.as_str(), kind, attempt.expires_at_ms)?;

        let step = if inner.script.is_empty() {
            // No script: answer honestly, including the honest "nothing yet".
            if let Some(reference) = inner.cancelled.get(id.as_str()) {
                return Ok(crate::Resolved::Answer(RawResponse {
                    effect_intent_id: id.clone(),
                    body: RawBody::Cancelled {
                        booking_ref: reference.clone(),
                    },
                    attested: true,
                }));
            }
            if let Some(reference) = inner.created.get(id.as_str()) {
                // The fake keeps no catalogue, so the canonical facts echo
                // the fixture's; the real council derives them (slice D).
                return Ok(crate::Resolved::Answer(RawResponse {
                    effect_intent_id: id.clone(),
                    body: RawBody::Created {
                        booking_ref: reference.clone(),
                        principal: PrincipalId::new("lucy"),
                        venue_id: "TH-A".to_owned(),
                        slot_id: "SLOT-A".to_owned(),
                        attendees: 20,
                        fee: Money::from_pence(4_500),
                    },
                    attested: true,
                }));
            }
            return Ok(crate::Resolved::NotYetVisible);
        } else {
            inner.script.remove(0)
        };

        let attested = step != Script::Forge;
        let body = match step {
            Script::GoQuiet(detail) | Script::SucceedThenGoQuiet(detail) => {
                return Err(Unknown::new(BoundedString::truncating(detail)));
            }
            Script::RefusePermanently(reason) => RawBody::RefusedPermanently { reason },
            Script::RefuseTemporarily(reason) => RawBody::RefusedForNow { reason },
            Script::Succeed | Script::Forge => {
                if let Some(reference) = inner.cancelled.get(id.as_str()) {
                    RawBody::Cancelled {
                        booking_ref: reference.clone(),
                    }
                } else if let Some(reference) = inner.created.get(id.as_str()) {
                    RawBody::Created {
                        booking_ref: reference.clone(),
                        principal: PrincipalId::new("lucy"),
                        venue_id: "TH-A".to_owned(),
                        slot_id: "SLOT-A".to_owned(),
                        attendees: 20,
                        fee: Money::from_pence(4_500),
                    }
                } else {
                    // An explicit "succeed" scripted for an identity holding
                    // nothing: the script asked for the impossible, and the
                    // only honest answer is no answer.
                    return Err(Unknown::new(BoundedString::truncating(
                        "the fake council has nothing for this identity",
                    )));
                }
            }
        };

        Ok(crate::Resolved::Answer(RawResponse {
            effect_intent_id: id.clone(),
            body,
            attested,
        }))
    }
}

#[async_trait]
impl Capability<BookingEffect> for FakeCouncil {
    type Raw = RawResponse;

    async fn execute(
        &self,
        effect: &BookingEffect,
        attempt: &EffectAttempt,
    ) -> Result<Self::Raw, Unknown> {
        // The arrival is logged and announced BEFORE the gate, because arrival
        // is what happened. The gate, when armed, then holds the call HERE —
        // after the caller's durable attempt mark, before any work — which is
        // where a slow council lives from the boundary's point of view.
        // Awaited outside the state lock, or the held call would block every
        // other wire.
        let gate = {
            let mut inner = self.lock();
            inner
                .wire_log
                .push((WireOp::Execute, attempt.id.as_str().to_owned()));
            inner.execute_gate.clone()
        };
        if let Some(gate) = gate {
            gate.arrived.add_permits(1);
            let permit = gate
                .release
                .acquire()
                .await
                .map_err(|_| Unknown::new(BoundedString::truncating("the gate was closed")))?;
            permit.forget();
        }

        let id = &attempt.id;
        let mut inner = self.lock();
        inner.bind(id.as_str(), effect.operation_kind(), attempt.expires_at_ms)?;
        inner.calls.push(Call {
            effect_intent_id: id.clone(),
            expires_at_ms: attempt.expires_at_ms,
            plan: effect.clone(),
        });

        let step = if inner.script.is_empty() {
            Script::Succeed
        } else {
            inner.script.remove(0)
        };

        let attested = step != Script::Forge;
        let body = match step {
            Script::GoQuiet(detail) => {
                return Err(Unknown::new(BoundedString::truncating(detail)));
            }
            Script::SucceedThenGoQuiet(detail) => {
                // The work happens — same idempotent create as Succeed — and the
                // answer does not.
                if let BookingEffect::Book { .. } = effect {
                    if !inner.created.contains_key(id.as_str()) {
                        inner.next_reference += 1;
                        let minted = CouncilBookingRef::new(format!(
                            "TH-{:05}",
                            90_000 + inner.next_reference
                        ));
                        inner.created.insert(id.as_str().to_owned(), minted);
                    }
                }
                return Err(Unknown::new(BoundedString::truncating(detail)));
            }
            Script::RefusePermanently(reason) => RawBody::RefusedPermanently { reason },
            Script::RefuseTemporarily(reason) => RawBody::RefusedForNow { reason },
            Script::Succeed | Script::Forge => match effect {
                BookingEffect::Book {
                    principal,
                    attendees,
                    facts,
                    // Recorded on the `Call` via `plan`, not inspected. This fake
                    // does not model the catalogue, so it has no row version to
                    // check a grant against, and pretending to validate one would
                    // be the overclaim this module's header refuses elsewhere for
                    // expiry. Slice D's real council is where the grant is
                    // enforced.
                    grant: _,
                } => {
                    // One identity, one booking. A retry returns the original
                    // rather than creating a second — the property the real
                    // council must also have.
                    let booking_ref = if let Some(existing) = inner.created.get(id.as_str()) {
                        existing.clone()
                    } else {
                        inner.next_reference += 1;
                        let minted = CouncilBookingRef::new(format!(
                            "TH-{:05}",
                            90_000 + inner.next_reference
                        ));
                        inner.created.insert(id.as_str().to_owned(), minted.clone());
                        minted
                    };
                    RawBody::Created {
                        booking_ref,
                        principal: principal.clone(),
                        venue_id: facts.venue_id.to_string(),
                        slot_id: facts.slot_id.to_string(),
                        attendees: *attendees,
                        fee: facts.fee,
                    }
                }
                BookingEffect::CancelBooking { booking_ref, .. } => {
                    inner
                        .cancelled
                        .entry(id.as_str().to_owned())
                        .or_insert_with(|| booking_ref.clone());
                    RawBody::Cancelled {
                        booking_ref: booking_ref.clone(),
                    }
                }
            },
        };

        Ok(RawResponse {
            effect_intent_id: id.clone(),
            body,
            attested,
        })
    }
}

/// Establishes provenance for the fake council's responses.
///
/// The refusal rule is the interesting one, and it is why `RawBody` distinguishes
/// permanent from temporary refusal at all. A terminal rejection is acted on
/// irreversibly — the booking returns to a re-proposable state and the intent is
/// finalised — so only a refusal the council has *permanently* closed may become
/// one. A rate limit or a validation error is `Unknown`.
#[derive(Debug, Default, Clone, Copy)]
pub struct CouncilVerifier;

impl Verifier<RawResponse, VerifiedProviderFact> for CouncilVerifier {
    fn verify(
        &self,
        raw: RawResponse,
    ) -> Result<Verified<VerifiedProviderFact>, VerificationError> {
        // Provenance first. An unattributable response is not evidence, and a
        // boundary that cannot read an answer has not received one.
        if !raw.attested {
            return Err(VerificationError::Rejected(BoundedString::truncating(
                "response carries no council attestation",
            )));
        }

        let fact = match raw.body {
            RawBody::Created {
                booking_ref,
                principal,
                venue_id,
                slot_id,
                attendees,
                fee,
            } => VerifiedProviderFact::BookingExists {
                effect_intent_id: raw.effect_intent_id,
                booking_ref,
                venue_id: bld_types::VenueId::new(venue_id),
                slot_id: bld_types::SlotId::new(slot_id),
                attendees,
                fee,
                principal,
            },
            RawBody::Cancelled { booking_ref } => VerifiedProviderFact::CancellationExists {
                effect_intent_id: raw.effect_intent_id,
                booking_ref,
            },
            RawBody::RefusedPermanently { reason } => VerifiedProviderFact::ProviderRejected {
                effect_intent_id: raw.effect_intent_id,
                reason: BoundedString::truncating(reason),
            },
            // The council declined this attempt and said nothing about whether
            // the effect exists. Concluding a rejection here is precisely how an
            // ordinary refusal would become ADR-016's durable tombstone.
            RawBody::RefusedForNow { reason } => {
                return Err(VerificationError::Unknown(BoundedString::truncating(
                    reason,
                )));
            }
        };

        Ok(Verified::assert_verified(fact))
    }
}

/// Availability that always answers with the same verified facts.
///
/// Slice D's `CouncilClient` replaces this with a signed council lookup. It exists
/// so the protocol tests are about the protocol.
///
/// It issues a fixed placeholder grant. That is honest rather than lazy: this fake
/// models no catalogue, so it has no row version and no validity window to sign,
/// and the grant's guarantees are the real council's to provide. What the fake
/// *does* establish is that the grant travels — through the context, into the
/// plan, and out to the capability — which is a separate property from whether it
/// is enforced, and the one the C-slice protocol tests are about.
pub struct FixedAvailability {
    facts: townhall_domain::VenueFacts,
    grant: AvailabilityGrant,
    /// When false, every read answers `Unavailable` — the fake's honest stand-in
    /// for a provider that cannot be asked (ADR-021's 503 leg).
    reachable: bool,
}

/// The token [`FixedAvailability::new`] issues.
///
/// Public so a test asserting "the plan the council was handed is the plan we
/// persisted" can name the grant it expects, instead of repeating a string that
/// silently stops matching. A test whose expected grant and issued grant drift
/// apart passes for the wrong reason or fails for no reason.
pub const FAKE_GRANT: &str = "fake-grant";

impl FixedAvailability {
    #[must_use]
    pub fn new(facts: townhall_domain::VenueFacts) -> Self {
        Self {
            facts,
            grant: AvailabilityGrant::new(FAKE_GRANT),
            reachable: true,
        }
    }

    /// A source that cannot be asked at all.
    #[must_use]
    pub fn unreachable(facts: townhall_domain::VenueFacts) -> Self {
        Self {
            reachable: false,
            ..Self::new(facts)
        }
    }

    /// A source whose grant is a specific value, so a test can follow that exact
    /// token from the availability read all the way to the council call.
    #[must_use]
    pub fn granting(facts: townhall_domain::VenueFacts, grant: AvailabilityGrant) -> Self {
        Self {
            facts,
            grant,
            reachable: true,
        }
    }
}

#[async_trait]
impl crate::AvailabilitySource for FixedAvailability {
    async fn read(
        &self,
        venue: &bld_types::VenueId,
        slot: &bld_types::SlotId,
    ) -> townhall_domain::ObservedAvailability {
        if !self.reachable {
            return townhall_domain::ObservedAvailability::Unavailable;
        }
        // Answers only for the venue it knows about, so a test cannot accidentally
        // bind facts to a venue nobody selected.
        townhall_domain::ObservedAvailability::Answered(
            (self.facts.venue_id == *venue && self.facts.slot_id == *slot).then(|| {
                Verified::assert_verified(townhall_domain::VerifiedAvailability {
                    facts: self.facts.clone(),
                    grant: self.grant.clone(),
                })
            }),
        )
    }
}

/// A fixed browse catalogue for tests — the fake counterpart of the council's
/// `GET /venues`. `None` facts models an unreachable catalogue.
pub struct FixedCatalogue {
    rows: Option<Vec<crate::VenueSummary>>,
}

impl FixedCatalogue {
    #[must_use]
    pub fn of(rows: Vec<crate::VenueSummary>) -> Self {
        Self { rows: Some(rows) }
    }
    #[must_use]
    pub const fn unreachable() -> Self {
        Self { rows: None }
    }
}

#[async_trait]
impl crate::CatalogueSource for FixedCatalogue {
    async fn venues(&self) -> Option<Vec<crate::VenueSummary>> {
        self.rows.clone()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use bld_types::{SlotId, VenueId};
    use townhall_domain::{OperationKind, VenueFacts};

    fn attempt(id: &str, expires_at_ms: i64) -> EffectAttempt {
        EffectAttempt {
            id: EffectIntentId::new(id),
            expires_at_ms,
        }
    }

    fn book_plan() -> BookingEffect {
        BookingEffect::Book {
            principal: PrincipalId::new("lucy"),
            attendees: 20,
            facts: VenueFacts {
                venue_id: VenueId::new("TH-A"),
                slot_id: SlotId::new("SLOT-A"),
                capacity: 30,
                wheelchair_accessible: true,
                fee: Money::from_pence(4_500),
                available: true,
            },
            grant: AvailabilityGrant::new(FAKE_GRANT),
        }
    }

    fn cancel_plan() -> BookingEffect {
        BookingEffect::CancelBooking {
            booking_ref: CouncilBookingRef::new("TH-90001"),
            principal: PrincipalId::new("lucy"),
        }
    }

    fn contradicts(reply: &Unknown) -> bool {
        format!("{reply:?}").contains("contradicts")
    }

    /// The four first-seen-binding cases (ADR-020, plan review round 3): both
    /// first-seen directions, with the KIND conflict and the DEADLINE conflict
    /// varied independently. Every conflicting envelope is unusable and the
    /// first-seen record survives — the same protocol histories the real
    /// council refuses with `ProtocolConflict`, so a protocol test passing
    /// against this fake means something.
    #[tokio::test]
    async fn a_resolve_first_binding_refuses_a_conflicting_execute_kind() {
        let council = FakeCouncil::new();
        let seen = crate::EffectResolver::resolve(
            &council,
            &attempt("EFF-BIND-1", 1_000),
            OperationKind::Book,
        )
        .await
        .expect("first sight binds");
        assert_eq!(seen, crate::Resolved::NotYetVisible);

        let refused = council
            .execute(&cancel_plan(), &attempt("EFF-BIND-1", 1_000))
            .await
            .expect_err("a Cancel envelope against a Book binding is unusable");
        assert!(contradicts(&refused), "{refused:?}");

        // The first-seen record survives the refused envelope.
        let still = crate::EffectResolver::resolve(
            &council,
            &attempt("EFF-BIND-1", 1_000),
            OperationKind::Book,
        )
        .await
        .expect("the binding is intact");
        assert_eq!(still, crate::Resolved::NotYetVisible);
    }

    #[tokio::test]
    async fn a_resolve_first_binding_refuses_a_conflicting_execute_deadline() {
        let council = FakeCouncil::new();
        crate::EffectResolver::resolve(
            &council,
            &attempt("EFF-BIND-2", 1_000),
            OperationKind::Book,
        )
        .await
        .expect("first sight binds");

        let refused = council
            .execute(&book_plan(), &attempt("EFF-BIND-2", 2_000))
            .await
            .expect_err("a different deadline for a bound identity is unusable");
        assert!(contradicts(&refused), "{refused:?}");
    }

    #[tokio::test]
    async fn an_execute_first_binding_refuses_a_conflicting_resolve_kind() {
        let council = FakeCouncil::new();
        council
            .execute(&cancel_plan(), &attempt("EFF-BIND-3", 1_000))
            .await
            .expect("first sight binds and cancels");

        let refused = crate::EffectResolver::resolve(
            &council,
            &attempt("EFF-BIND-3", 1_000),
            OperationKind::Book,
        )
        .await
        .expect_err("asking about it as a Book is unusable");
        assert!(contradicts(&refused), "{refused:?}");

        // A MATCHING ask still answers: the settled cancellation.
        let answered = crate::EffectResolver::resolve(
            &council,
            &attempt("EFF-BIND-3", 1_000),
            OperationKind::Cancel,
        )
        .await
        .expect("the binding is intact");
        assert!(
            matches!(
                answered,
                crate::Resolved::Answer(RawResponse {
                    body: RawBody::Cancelled { .. },
                    ..
                })
            ),
            "{answered:?}"
        );
    }

    #[tokio::test]
    async fn an_execute_first_binding_refuses_a_conflicting_resolve_deadline() {
        let council = FakeCouncil::new();
        council
            .execute(&book_plan(), &attempt("EFF-BIND-4", 1_000))
            .await
            .expect("first sight binds and books");

        let refused = crate::EffectResolver::resolve(
            &council,
            &attempt("EFF-BIND-4", 9_999),
            OperationKind::Book,
        )
        .await
        .expect_err("a different deadline for a bound identity is unusable");
        assert!(contradicts(&refused), "{refused:?}");
    }
}
