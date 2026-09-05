use std::sync::Arc;

use bld_kernel::{Capability as _, Verifier as _};
use bld_types::{
    AvailabilityGrant, BookingRequirements, EffectAttempt, EffectIntentId, Money, PaymentIntentId,
    PrincipalId, SlotId, TimeWindow, VenueId,
};
use council_client::CouncilClient;
use council_wire::{CouncilKey, CouncilSigner, CouncilSigningKey};
use mock_council::Council;
use stripe_client::{StripeClient, StripeSecretKey};
use tempfile::TempDir;
use townhall_domain::{
    BookingEffect, ObservedAvailability, SelectedVenueRef, VerifiedProviderFact,
};
use townhall_effects_router::{CompositeRaw, EffectsRouter};
use townhall_service::AvailabilitySource as _;

struct Harness {
    _dir: TempDir,
    _council: Council,
    router: EffectsRouter,
    council: Arc<CouncilClient>,
}

impl Harness {
    async fn new() -> Self {
        let dir = TempDir::new().expect("temporary provider database");
        let signer = Arc::new(CouncilSigner::new(CouncilSigningKey::from_bytes(&[7; 32])));
        let key = CouncilKey::new(signer.verifying_key());
        let council = Council::open(dir.path().join("council.sqlite"), signer)
            .await
            .expect("open mock council");
        let council_listener = tokio::net::TcpListener::bind("127.0.0.1:0")
            .await
            .expect("bind mock council");
        let council_address = council_listener.local_addr().expect("council address");
        let council_router = council.router();
        tokio::spawn(async move {
            let _ = axum::serve(council_listener, council_router).await;
        });
        let council_client = Arc::new(CouncilClient::new(format!("http://{council_address}"), key));

        let stripe = mock_stripe::MockStripe::new();
        let stripe_listener = tokio::net::TcpListener::bind("127.0.0.1:0")
            .await
            .expect("bind mock stripe");
        let stripe_address = stripe_listener.local_addr().expect("stripe address");
        let stripe_router = stripe.router();
        tokio::spawn(async move {
            let _ = axum::serve(stripe_listener, stripe_router).await;
        });
        let stripe_client = Arc::new(StripeClient::new(
            format!("http://{stripe_address}"),
            StripeSecretKey::new("sk_test_router"),
            "https://townhall.test/payments/success",
            "https://townhall.test/payments/cancel",
        ));

        let availability: Arc<dyn townhall_service::AvailabilitySource> = council_client.clone();
        let router = EffectsRouter::new(Arc::clone(&council_client), stripe_client, availability);
        Self {
            _dir: dir,
            _council: council,
            router,
            council: council_client,
        }
    }
}

fn attempt(id: &str) -> EffectAttempt {
    EffectAttempt {
        id: EffectIntentId::new(id),
        expires_at_ms: i64::MAX / 2,
    }
}

fn selection() -> SelectedVenueRef {
    SelectedVenueRef {
        venue_id: VenueId::new("TH-A"),
        slot_id: SlotId::new("SLOT-A"),
    }
}

#[tokio::test]
async fn prepare_payment_routes_to_stripe_and_mints_the_session_fact() {
    let harness = Harness::new().await;
    let raw = harness
        .router
        .execute(
            &BookingEffect::PreparePayment {
                principal: PrincipalId::new("lucy"),
                payment_intent_id: PaymentIntentId::new("PAY-BKG-ROUTER-3"),
                selection: selection(),
                amount: Money::from_pence(4_500),
                grant: AvailabilityGrant::new("frozen-grant"),
                payment_ref: None,
            },
            &attempt("EFF-BKG-ROUTER-PAY-4"),
        )
        .await
        .expect("Stripe creates a Checkout Session");

    let CompositeRaw::Stripe(stripe_client::StripeRaw::SessionCreated {
        ref stripe_session_id,
        ref hosted_url,
        ..
    }) = raw
    else {
        panic!("payment must route to Stripe");
    };
    assert_eq!(stripe_session_id, "cs_test_00000001");
    assert_eq!(hosted_url, "https://checkout.stripe.test/cs_test_00000001");

    let fact = harness
        .router
        .verify(raw)
        .expect("trusted Stripe raw verifies")
        .into_inner();
    assert_eq!(
        fact,
        VerifiedProviderFact::SessionCreated {
            effect_intent_id: EffectIntentId::new("EFF-BKG-ROUTER-PAY-4"),
            payment_intent_id: PaymentIntentId::new("PAY-BKG-ROUTER-3"),
            payment_ref: bld_types::PaymentRef::new("cs_test_00000001"),
            hosted_url: "https://checkout.stripe.test/cs_test_00000001".to_owned(),
        }
    );
}

#[tokio::test]
async fn book_routes_to_the_council_and_returns_its_real_reference() {
    let harness = Harness::new().await;
    let observation = match harness
        .council
        .read(&VenueId::new("TH-A"), &SlotId::new("SLOT-A"))
        .await
    {
        ObservedAvailability::Answered(Some(observation)) => observation.into_inner(),
        other => panic!("mock council must answer availability, got {other:?}"),
    };
    let raw = harness
        .router
        .execute(
            &BookingEffect::Book {
                principal: PrincipalId::new("lucy"),
                attendees: 20,
                facts: observation.facts,
                grant: observation.grant,
            },
            &attempt("EFF-BKG-ROUTER-BOOK-1"),
        )
        .await
        .expect("council creates the booking");
    assert!(matches!(raw, CompositeRaw::Council(_)));
    let fact = harness
        .router
        .verify(raw)
        .expect("signed council fact")
        .into_inner();
    let VerifiedProviderFact::BookingExists {
        effect_intent_id,
        booking_ref,
        venue_id,
        slot_id,
        ..
    } = fact
    else {
        panic!("book must produce BookingExists");
    };
    assert_eq!(
        effect_intent_id,
        EffectIntentId::new("EFF-BKG-ROUTER-BOOK-1")
    );
    assert_eq!(booking_ref.as_str(), "TH-90001");
    assert_eq!(venue_id, VenueId::new("TH-A"));
    assert_eq!(slot_id, SlotId::new("SLOT-A"));
}

#[tokio::test]
async fn verify_availability_reads_the_source_and_mints_the_exact_observation() {
    let harness = Harness::new().await;
    let raw = harness
        .router
        .execute(
            &BookingEffect::VerifyAvailability {
                principal: PrincipalId::new("lucy"),
                selection: selection(),
                requirements: BookingRequirements {
                    purpose: "community meeting".to_owned(),
                    requested_date: "2026-09-20".to_owned(),
                    time_window: TimeWindow {
                        from: "13:00".to_owned(),
                        to: "17:00".to_owned(),
                    },
                    attendees: 20,
                    wheelchair_accessible: true,
                    max_fee: Money::from_pence(5_000),
                },
                authority_max_fee: Money::from_pence(5_000),
                payment_threshold: Money::from_pence(3_000),
                threshold_policy_version: "threshold-v1".to_owned(),
            },
            &attempt("EFF-BKG-ROUTER-VERIFY-1"),
        )
        .await
        .expect("availability source answers");
    assert!(matches!(raw, CompositeRaw::Availability(_)));
    let fact = harness
        .router
        .verify(raw)
        .expect("verified observation mints")
        .into_inner();
    let VerifiedProviderFact::AvailabilityVerified {
        effect_intent_id,
        facts,
        grant,
    } = fact
    else {
        panic!("verify must produce AvailabilityVerified");
    };
    assert_eq!(
        effect_intent_id,
        EffectIntentId::new("EFF-BKG-ROUTER-VERIFY-1")
    );
    assert_eq!(facts.venue_id, VenueId::new("TH-A"));
    assert_eq!(facts.slot_id, SlotId::new("SLOT-A"));
    assert_eq!(facts.fee, Money::from_pence(4_500));
    assert!(facts.available);
    assert!(!grant.on_the_wire().is_empty());
}
