#![forbid(unsafe_code)]

//! Trusted dispatch for the town hall's heterogeneous external effects.

use std::sync::Arc;

use async_trait::async_trait;
use bld_kernel::{Capability, Unknown, VerificationError, Verified, Verifier};
use bld_types::{BoundedString, EffectAttempt, EffectIntentId};
use council_client::{CouncilClient, CouncilVerifier};
use council_wire::SignedEffectResponse;
use stripe_client::{StripeClient, StripeRaw};
use townhall_domain::{
    BookingEffect, ObservedAvailability, OperationKind, VerifiedAvailability, VerifiedProviderFact,
};
use townhall_payment::StripeVerifier;
use townhall_service::{AvailabilitySource, EffectResolver, Resolved};

/// The verified availability observation plus the durable effect identity it
/// answers. The source has already established the facts/grant pair; the router
/// preserves that provenance until its verifier mints the provider fact.
#[derive(Clone, Debug)]
pub struct AvailabilityRaw {
    effect_intent_id: EffectIntentId,
    observation: VerifiedAvailability,
}

/// One raw answer from whichever provider owns the effect variant.
#[derive(Clone, Debug)]
pub enum CompositeRaw {
    Council(SignedEffectResponse),
    Stripe(StripeRaw),
    Availability(AvailabilityRaw),
}

/// The single capability/resolver/verifier held by the coordinator.
pub struct EffectsRouter {
    council: Arc<CouncilClient>,
    stripe: Arc<StripeClient>,
    availability: Arc<dyn AvailabilitySource>,
    council_verifier: CouncilVerifier,
    stripe_verifier: StripeVerifier,
}

impl EffectsRouter {
    #[must_use]
    pub fn new(
        council: Arc<CouncilClient>,
        stripe: Arc<StripeClient>,
        availability: Arc<dyn AvailabilitySource>,
    ) -> Self {
        let council_verifier = council.verifier();
        Self {
            council,
            stripe,
            availability,
            council_verifier,
            stripe_verifier: StripeVerifier,
        }
    }
}

#[async_trait]
impl Capability<BookingEffect> for EffectsRouter {
    type Raw = CompositeRaw;

    async fn execute(
        &self,
        effect: &BookingEffect,
        attempt: &EffectAttempt,
    ) -> Result<Self::Raw, Unknown> {
        match effect {
            BookingEffect::Book { .. } | BookingEffect::CancelBooking { .. } => self
                .council
                .execute(effect, attempt)
                .await
                .map(CompositeRaw::Council),
            BookingEffect::PreparePayment { .. } => self
                .stripe
                .execute(effect, attempt)
                .await
                .map(CompositeRaw::Stripe),
            BookingEffect::VerifyAvailability { selection, .. } => match self
                .availability
                .read(&selection.venue_id, &selection.slot_id)
                .await
            {
                ObservedAvailability::Answered(Some(observation)) => {
                    Ok(CompositeRaw::Availability(AvailabilityRaw {
                        effect_intent_id: attempt.id.clone(),
                        observation: observation.into_inner(),
                    }))
                }
                ObservedAvailability::Answered(None) | ObservedAvailability::Unavailable => {
                    Err(Unknown::new(BoundedString::truncating(
                        "availability could not be established",
                    )))
                }
            },
        }
    }
}

#[async_trait]
impl EffectResolver<CompositeRaw> for EffectsRouter {
    async fn resolve(
        &self,
        attempt: &EffectAttempt,
        kind: OperationKind,
    ) -> Result<Resolved<CompositeRaw>, Unknown> {
        match kind {
            OperationKind::Book | OperationKind::Cancel => self
                .council
                .resolve(attempt, kind)
                .await
                .map(|answer| answer.map(CompositeRaw::Council)),
            OperationKind::Pay => self
                .stripe
                .resolve(attempt, kind)
                .await
                .map(|answer| answer.map(CompositeRaw::Stripe)),
            OperationKind::Verify => Err(Unknown::new(BoundedString::truncating(
                "availability is resolved synchronously when its effect is executed",
            ))),
        }
    }
}

impl Verifier<CompositeRaw, VerifiedProviderFact> for EffectsRouter {
    fn verify(
        &self,
        raw: CompositeRaw,
    ) -> Result<Verified<VerifiedProviderFact>, VerificationError> {
        match raw {
            CompositeRaw::Council(raw) => self.council_verifier.verify(raw),
            CompositeRaw::Stripe(raw) => self.stripe_verifier.verify(raw),
            CompositeRaw::Availability(raw) => Ok(Verified::assert_verified(
                VerifiedProviderFact::AvailabilityVerified {
                    effect_intent_id: raw.effect_intent_id,
                    facts: raw.observation.facts,
                    grant: raw.observation.grant,
                },
            )),
        }
    }
}

trait MapResolved<T> {
    fn map<U>(self, f: impl FnOnce(T) -> U) -> Resolved<U>;
}

impl<T> MapResolved<T> for Resolved<T> {
    fn map<U>(self, f: impl FnOnce(T) -> U) -> Resolved<U> {
        match self {
            Self::Answer(raw) => Resolved::Answer(f(raw)),
            Self::NotYetVisible => Resolved::NotYetVisible,
        }
    }
}
