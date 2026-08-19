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
use bld_types::{BoundedString, CouncilBookingRef, EffectIntentId, Money, PrincipalId};
use std::{
    collections::HashMap,
    sync::{Arc, Mutex},
};
use townhall_domain::{BookingEffect, VerifiedProviderFact};

/// What the council was asked to do, recorded in the order it was asked.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Call {
    pub effect_intent_id: EffectIntentId,
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
        id: &EffectIntentId,
    ) -> Result<Self::Raw, Unknown> {
        (self.during)(id);
        self.council.execute(effect, id).await
    }
}

#[async_trait]
impl Capability<BookingEffect> for FakeCouncil {
    type Raw = RawResponse;

    async fn execute(
        &self,
        effect: &BookingEffect,
        id: &EffectIntentId,
    ) -> Result<Self::Raw, Unknown> {
        let mut inner = self.lock();
        inner.calls.push(Call {
            effect_intent_id: id.clone(),
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
            Script::RefusePermanently(reason) => RawBody::RefusedPermanently { reason },
            Script::RefuseTemporarily(reason) => RawBody::RefusedForNow { reason },
            Script::Succeed | Script::Forge => match effect {
                BookingEffect::Book {
                    principal,
                    attendees,
                    facts,
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
                BookingEffect::CancelBooking { booking_ref } => {
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
/// Slice D replaces this with a council lookup. It exists so the protocol tests
/// are about the protocol.
pub struct FixedAvailability {
    facts: townhall_domain::VenueFacts,
}

impl FixedAvailability {
    #[must_use]
    pub const fn new(facts: townhall_domain::VenueFacts) -> Self {
        Self { facts }
    }
}

#[async_trait]
impl crate::AvailabilitySource for FixedAvailability {
    async fn read(
        &self,
        venue: &bld_types::VenueId,
        slot: &bld_types::SlotId,
    ) -> Option<townhall_domain::VenueFacts> {
        // Answers only for the venue it knows about, so a test cannot accidentally
        // bind facts to a venue nobody selected.
        (self.facts.venue_id == *venue && self.facts.slot_id == *slot).then(|| self.facts.clone())
    }
}
