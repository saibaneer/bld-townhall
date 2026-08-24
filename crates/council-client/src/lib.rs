#![forbid(unsafe_code)]

//! The council over HTTP, wearing the traits the boundary already speaks.
//!
//! # What this crate is, and what it is not
//!
//! It is an adapter, not a server. It makes a network call *look like* the
//! [`Capability`], [`Verifier`], [`AvailabilitySource`] and [`EffectResolver`]
//! traits the coordinator was already written against — which is why the
//! coordinator does not change at all when the in-process fake is swapped for a
//! real socket. That was the payoff for making it generic in slice C, and this is
//! where it gets collected.
//!
//! # Where the verification happens, and where it deliberately does not
//!
//! [`CouncilVerifier`] establishes one thing: this response was produced by the
//! holder of the council's private key, intact. That is exactly what
//! `bld-kernel`'s `Verifier` contract asks of it, and it is why a
//! field-perfect *unsigned* response is refused.
//!
//! It does **not** check that the response is about the effect we asked about. It
//! cannot — `verify` receives only the response, with no request context, and
//! threading "the last request" through a shared verifier would race under
//! concurrency. Nor should it: a signed response for another identity is
//! *genuinely council-originated*, so it passes verification correctly and is then
//! refused by the domain binding it against the persisted intent. The kernel's own
//! documentation assigns wrong-effect to the domain, and duplicating the check
//! here would put one fact in two places.
//!
//! # Why the client does not check the availability window
//!
//! Every availability answer carries `valid_until_ms`, and this client ignores it.
//!
//! Checking it would need a client clock, and a client clock cannot make the check
//! safe: running fast it refuses live facts, running slow it accepts dead ones.
//! Only the council can compare its own deadline against its own clock, which is
//! what it does when the grant comes back. Having the check here as an
//! "optimisation" would invite a later reader to mistake it for the guarantee, and
//! saving one doomed round trip is not worth that.

use async_trait::async_trait;
use bld_kernel::{Capability, Unknown, VerificationError, Verified, Verifier};
use bld_types::{
    AvailabilityGrant, BoundedString, CouncilBookingRef, EffectAttempt, Money, PrincipalId, SlotId,
    VenueId,
};
use council_wire::{
    CouncilKey, EffectOutcome, SignedAvailabilityResponse, SignedEffectResponse,
    body::{
        AvailabilityResponseBody, CancelBookingBody, CreateBookingBody, EffectResponseBody,
        ResolveBody,
    },
};
use townhall_domain::{
    BookingEffect, OperationKind, VenueFacts, VerifiedAvailability, VerifiedProviderFact,
};
use townhall_service::{AvailabilitySource, EffectResolver};

pub use council_wire::{CouncilSigner, WireError};

/// Talks to one council.
pub struct CouncilClient {
    http: reqwest::Client,
    /// No trailing slash.
    base_url: String,
    key: CouncilKey,
}

impl CouncilClient {
    #[must_use]
    pub fn new(base_url: impl Into<String>, key: CouncilKey) -> Self {
        Self::with_timeout(base_url, key, std::time::Duration::from_secs(10))
    }

    /// A client whose patience is explicit.
    ///
    /// Without a timeout, a slow answer is merely slow — it never becomes
    /// `Unknown`, so a council that answers after five minutes holds the caller
    /// for five minutes and the `Delay` fault tests nothing. With one, lateness
    /// becomes what it honestly is: no answer within our patience, which says
    /// nothing about whether the effect happened — exactly `Unknown`'s meaning.
    #[must_use]
    pub fn with_timeout(
        base_url: impl Into<String>,
        key: CouncilKey,
        timeout: std::time::Duration,
    ) -> Self {
        Self {
            http: reqwest::Client::builder()
                .timeout(timeout)
                .build()
                .unwrap_or_default(),
            base_url: base_url.into().trim_end_matches('/').to_owned(),
            key,
        }
    }

    /// The pinned key, for a caller that also needs to verify.
    #[must_use]
    pub const fn verifier(&self) -> CouncilVerifier {
        CouncilVerifier { key: self.key }
    }

    /// Send a request and read a signed effect answer back.
    ///
    /// Everything that is not a well-formed answer becomes [`Unknown`]: a
    /// connection that failed, a body that would not parse, an outcome tag this
    /// build does not recognise. That collapse is deliberate. `Unknown` means "the
    /// attempt produced no answer at all", and every one of those cases says
    /// nothing whatsoever about whether the council acted — which is precisely the
    /// situation the coordinator must not resolve either way.
    ///
    /// A **non-2xx status is not itself an error here.** The council answers a
    /// refusal with 422 and a conflict with 409, and those are things it *said*.
    /// Treating status codes as transport failures would turn an authoritative
    /// rejection into ambiguity, and ambiguity into an effect nobody ever settles.
    async fn effect_call(
        &self,
        request: reqwest::RequestBuilder,
    ) -> Result<SignedEffectResponse, Unknown> {
        let unknown = |detail: String| Unknown::new(BoundedString::truncating(detail));

        let response = request
            .send()
            .await
            .map_err(|error| unknown(format!("the council could not be reached: {error}")))?;

        let body: EffectResponseBody = response
            .json()
            .await
            .map_err(|error| unknown(format!("the council's answer was unreadable: {error}")))?;

        SignedEffectResponse::try_from(body)
            .map_err(|error| unknown(format!("the council's answer was uninterpretable: {error}")))
    }
}

#[async_trait]
impl Capability<BookingEffect> for CouncilClient {
    type Raw = SignedEffectResponse;

    async fn execute(
        &self,
        effect: &BookingEffect,
        attempt: &EffectAttempt,
    ) -> Result<Self::Raw, Unknown> {
        let request = match effect {
            BookingEffect::Book {
                principal,
                attendees,
                facts,
                grant,
            } => self
                .http
                .post(format!("{}/bookings", self.base_url))
                .json(&CreateBookingBody {
                    effect_intent_id: attempt.id.as_str().to_owned(),
                    // From the attempt, which the coordinator built from the
                    // persisted intent. Never re-derived here: the council binds
                    // this value permanently on first sight, so a deadline of our
                    // own invention would make every later lookup a conflict.
                    expires_at_ms: attempt.expires_at_ms,
                    venue_id: facts.venue_id.as_str().to_owned(),
                    slot_id: facts.slot_id.as_str().to_owned(),
                    attendees: *attendees,
                    fee_pence: facts.fee.pence(),
                    principal: principal.as_str().to_owned(),
                    // From the *plan*, so a retry sends the grant this booking was
                    // authorised with rather than a fresh one. Re-reading
                    // availability here would fetch a currently-valid warrant for
                    // facts the plan no longer reflects.
                    grant: grant.on_the_wire().to_owned(),
                }),
            BookingEffect::CancelBooking { booking_ref } => self
                .http
                .post(format!(
                    "{}/bookings/{}/cancel",
                    self.base_url,
                    booking_ref.as_str()
                ))
                .json(&CancelBookingBody {
                    effect_intent_id: attempt.id.as_str().to_owned(),
                    expires_at_ms: attempt.expires_at_ms,
                }),
        };

        self.effect_call(request).await
    }
}

#[async_trait]
impl EffectResolver<SignedEffectResponse> for CouncilClient {
    async fn resolve(
        &self,
        attempt: &EffectAttempt,
        kind: OperationKind,
    ) -> Result<SignedEffectResponse, Unknown> {
        // POST, because answering this writes: past its deadline the council
        // tombstones the identity before replying (ADR-016 §§3-4).
        let request = self
            .http
            .post(format!(
                "{}/effects/{}/resolve",
                self.base_url,
                attempt.id.as_str()
            ))
            .json(&ResolveBody {
                expires_at_ms: attempt.expires_at_ms,
                operation_kind: kind.name().to_owned(),
            });

        self.effect_call(request).await
    }
}

#[async_trait]
impl AvailabilitySource for CouncilClient {
    async fn read(&self, venue: &VenueId, slot: &SlotId) -> Option<Verified<VerifiedAvailability>> {
        let body: AvailabilityResponseBody = self
            .http
            .get(format!(
                "{}/venues/{}/slots/{}",
                self.base_url,
                venue.as_str(),
                slot.as_str()
            ))
            .send()
            .await
            .ok()?
            .json()
            .await
            .ok()?;

        let response = SignedAvailabilityResponse::from(body);

        // Provenance first, and before anything is read out of the payload. These
        // facts decide accessibility, fee and capacity; a substituted response
        // would have the boundary approve from forged context with a clean audit
        // trail behind it.
        self.key.check_availability(&response).ok()?;

        // The signature covers the venue and slot, so this comparison is not
        // redundant with it — it stops a genuinely signed answer about a
        // *different* slot being bound to this question.
        if response.venue_id != venue.as_str() || response.slot_id != slot.as_str() {
            return None;
        }

        let facts = response.facts?;
        Some(Verified::assert_verified(VerifiedAvailability {
            facts: VenueFacts {
                venue_id: venue.clone(),
                slot_id: slot.clone(),
                capacity: facts.capacity,
                wheelchair_accessible: facts.accessible,
                fee: Money::from_pence(facts.fee_pence),
                available: facts.available,
            },
            grant: AvailabilityGrant::new(facts.grant),
        }))
    }
}

/// Establishes that a response came from the council, and what fact it carries.
#[derive(Clone, Copy, Debug)]
pub struct CouncilVerifier {
    key: CouncilKey,
}

impl CouncilVerifier {
    #[must_use]
    pub const fn new(key: CouncilKey) -> Self {
        Self { key }
    }
}

impl Verifier<SignedEffectResponse, VerifiedProviderFact> for CouncilVerifier {
    /// # The refusal rule is the load-bearing part
    ///
    /// A terminal rejection is acted on irreversibly — the booking returns to a
    /// re-proposable state and the intent is finalised — so only a refusal the
    /// council has *permanently closed* may become one.
    ///
    /// So three of the seven outcomes stay [`VerificationError::Unknown`], and each
    /// for its own reason:
    ///
    /// - `NotYetVisible` — the council has heard of this identity and nothing has
    ///   settled. Reading it as absence is exactly how a live booking gets
    ///   cancelled underneath us.
    /// - `ProtocolConflict` — we sent a deadline or kind that contradicts what the
    ///   council already bound. **Our** bug, not a fact about the world, and
    ///   fabricating a provider fact from our own error would be the worst
    ///   available response.
    /// - `Unavailable` — the council could not answer. Says nothing about whether
    ///   the effect exists.
    fn verify(
        &self,
        raw: SignedEffectResponse,
    ) -> Result<Verified<VerifiedProviderFact>, VerificationError> {
        // Provenance before interpretation. An unattributable response is not
        // evidence, and a body this crate cannot verify has told it nothing.
        self.key.check_effect(&raw).map_err(|error| {
            VerificationError::Rejected(BoundedString::truncating(error.to_string()))
        })?;

        let effect_intent_id = bld_types::EffectIntentId::new(raw.effect_intent_id);

        let fact = match raw.outcome {
            EffectOutcome::BookingCreated(facts) => VerifiedProviderFact::BookingExists {
                effect_intent_id,
                booking_ref: CouncilBookingRef::new(facts.booking_reference),
                venue_id: VenueId::new(facts.venue_id),
                slot_id: SlotId::new(facts.slot_id),
                attendees: facts.attendees,
                fee: Money::from_pence(facts.fee_pence),
                principal: PrincipalId::new(facts.principal),
            },
            EffectOutcome::CancellationApplied { booking_reference } => {
                VerifiedProviderFact::CancellationExists {
                    effect_intent_id,
                    booking_ref: CouncilBookingRef::new(booking_reference),
                }
            }
            EffectOutcome::DefinitivelyAbsent => {
                VerifiedProviderFact::EffectAbsent { effect_intent_id }
            }
            EffectOutcome::ProviderRejected { reason } => VerifiedProviderFact::ProviderRejected {
                effect_intent_id,
                reason: BoundedString::truncating(reason),
            },
            EffectOutcome::NotYetVisible => {
                return Err(VerificationError::Unknown(BoundedString::truncating(
                    "the council has not settled this effect yet",
                )));
            }
            EffectOutcome::ProtocolConflict { reason } => {
                return Err(VerificationError::Unknown(BoundedString::truncating(
                    format!("the council's binding contradicts our request: {reason}"),
                )));
            }
            EffectOutcome::Unavailable { reason } => {
                return Err(VerificationError::Unknown(BoundedString::truncating(
                    format!("the council could not answer: {reason}"),
                )));
            }
        };

        Ok(Verified::assert_verified(fact))
    }
}
