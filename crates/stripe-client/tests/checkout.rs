mod support;

use bld_kernel::Capability as _;
use bld_types::{
    AvailabilityGrant, CouncilBookingRef, EffectAttempt, EffectIntentId, Money, PaymentIntentId,
    PrincipalId, SlotId, VenueId,
};
use stripe_client::{StripeClient, StripeRaw, StripeSecretKey};
use support::MockStripeProcess;
use townhall_domain::{BookingEffect, OperationKind, SelectedVenueRef};
use townhall_service::{EffectResolver as _, Resolved};

fn client(world: &MockStripeProcess) -> StripeClient {
    StripeClient::new(
        &world.base_url,
        StripeSecretKey::new("sk_test_fixture"),
        "https://townhall.test/payments/success",
        "https://townhall.test/payments/cancel",
    )
}

fn attempt(id: &str) -> EffectAttempt {
    EffectAttempt {
        id: EffectIntentId::new(id),
        expires_at_ms: 2_000_000_000_000,
    }
}

fn payment(payment_id: &str) -> BookingEffect {
    BookingEffect::PreparePayment {
        principal: PrincipalId::new("lucy"),
        payment_intent_id: PaymentIntentId::new(payment_id),
        selection: SelectedVenueRef {
            venue_id: VenueId::new("TH-A"),
            slot_id: SlotId::new("SLOT-A"),
        },
        amount: Money::from_pence(4_500),
        grant: AvailabilityGrant::new("fixture-grant"),
        payment_ref: None,
    }
}

#[tokio::test]
async fn council_effects_are_rejected_without_crossing_the_wire() {
    let stripe = StripeClient::new(
        "http://127.0.0.1:1",
        StripeSecretKey::new("sk_test_fixture"),
        "https://townhall.test/success",
        "https://townhall.test/cancel",
    );
    let result = stripe
        .execute(
            &BookingEffect::CancelBooking {
                booking_ref: CouncilBookingRef::new("COUNCIL-1"),
                principal: PrincipalId::new("lucy"),
            },
            &attempt("EFF-NOT-STRIPE"),
        )
        .await;

    assert!(
        result.is_err(),
        "a council effect must be Unknown to Stripe"
    );
}

#[tokio::test]
async fn create_retry_and_retrieve_are_real_protocol_witnesses() {
    let world = MockStripeProcess::spawn();
    let stripe = client(&world);
    let effect_attempt = attempt("EFF-BKG-1-PAY-4");

    let first = stripe
        .execute(&payment("PAY-BKG-1-3"), &effect_attempt)
        .await
        .expect("create session");
    let StripeRaw::SessionCreated {
        effect_intent_id,
        stripe_session_id,
        hosted_url,
        payment_intent_id,
        expires_at_ms,
    } = &first
    else {
        panic!("expected a created session, got {first:?}");
    };
    assert_eq!(effect_intent_id, &effect_attempt.id);
    assert_eq!(stripe_session_id, "cs_test_00000001");
    assert_eq!(hosted_url, "https://checkout.stripe.test/cs_test_00000001");
    assert_eq!(payment_intent_id, &PaymentIntentId::new("PAY-BKG-1-3"));
    assert!(*expires_at_ms > 0);

    let repeated = stripe
        .execute(&payment("PAY-BKG-1-3"), &effect_attempt)
        .await
        .expect("idempotent retry");
    assert_eq!(
        repeated, first,
        "the same Idempotency-Key must return the committed session"
    );

    let retrieved = stripe
        .resolve(&attempt(stripe_session_id), OperationKind::Pay)
        .await
        .expect("retrieve session");
    let Resolved::Answer(StripeRaw::SessionRetrieved {
        effect_intent_id: retrieved_effect_id,
        stripe_session_id: retrieved_id,
        payment_intent_id: retrieved_payment_id,
        checkout_status,
        payment_status,
        payment_intent_status,
        expires_at_ms: retrieved_expiry,
    }) = retrieved
    else {
        panic!("expected retrieved Stripe state");
    };
    assert_eq!(retrieved_effect_id, EffectIntentId::new(stripe_session_id));
    assert_eq!(retrieved_id, *stripe_session_id);
    assert_eq!(
        retrieved_payment_id,
        Some(PaymentIntentId::new("PAY-BKG-1-3"))
    );
    assert_eq!(checkout_status.as_deref(), Some("open"));
    assert_eq!(payment_status, "unpaid");
    assert_eq!(
        payment_intent_status.as_deref(),
        Some("requires_payment_method")
    );
    assert_eq!(retrieved_expiry, *expires_at_ms);
}

#[tokio::test]
async fn payment_metadata_alone_is_an_idempotency_key() {
    let world = MockStripeProcess::spawn();
    let stripe = client(&world);

    // The SAME payment metadata, but two DIFFERENT attempts (different effect
    // intents). Stripe returns the SAME session; only the per-attempt
    // effect_intent_id — which the verifier needs to name the intent this raw
    // settles — legitimately differs. So the SESSION identity is what must match,
    // not the whole struct.
    let StripeRaw::SessionCreated {
        stripe_session_id: first_session,
        hosted_url: first_url,
        payment_intent_id: first_intent,
        ..
    } = stripe
        .execute(&payment("PAY-BKG-2-3"), &attempt("EFF-BKG-2-PAY-4"))
        .await
        .expect("first create")
    else {
        panic!("create returns a session");
    };
    let StripeRaw::SessionCreated {
        stripe_session_id: repeated_session,
        hosted_url: repeated_url,
        payment_intent_id: repeated_intent,
        effect_intent_id: repeated_effect,
        ..
    } = stripe
        .execute(&payment("PAY-BKG-2-3"), &attempt("EFF-BKG-2-PAY-99"))
        .await
        .expect("metadata-idempotent create")
    else {
        panic!("create returns a session");
    };

    assert_eq!(
        repeated_session, first_session,
        "payment metadata must identify one session"
    );
    assert_eq!(repeated_url, first_url);
    assert_eq!(repeated_intent, first_intent);
    // The second attempt's own effect id rides on the same session.
    assert_eq!(repeated_effect, EffectIntentId::new("EFF-BKG-2-PAY-99"));
}

#[tokio::test]
async fn a_dropped_create_response_is_unknown_after_the_session_commits() {
    let world = MockStripeProcess::spawn();
    let stripe = client(&world);
    let key = "EFF-BKG-DROP-PAY-4";
    let arm: serde_json::Value = reqwest::Client::new()
        .post(format!("{}/test/faults", world.base_url))
        .json(&serde_json::json!({
            "key": key,
            "route": "create",
            "fault": "drop_response"
        }))
        .send()
        .await
        .expect("arm fault")
        .json()
        .await
        .expect("fault id");
    let fault_id = arm["fault_id"].as_u64().expect("numeric fault id");

    let result = stripe
        .execute(&payment("PAY-BKG-DROP-3"), &attempt(key))
        .await;
    assert!(result.is_err(), "a dropped response is never false success");

    let status: serde_json::Value = reqwest::Client::new()
        .get(format!("{}/test/faults/{fault_id}", world.base_url))
        .send()
        .await
        .expect("fault status")
        .json()
        .await
        .expect("fault status JSON");
    assert_eq!(status["consumed"], 1);

    let retry = stripe
        .execute(&payment("PAY-BKG-DROP-3"), &attempt(key))
        .await
        .expect("retry finds committed session");
    let StripeRaw::SessionCreated {
        stripe_session_id,
        hosted_url,
        ..
    } = retry
    else {
        panic!("expected committed session");
    };
    assert_eq!(stripe_session_id, "cs_test_00000001");
    assert_eq!(hosted_url, "https://checkout.stripe.test/cs_test_00000001");
}
