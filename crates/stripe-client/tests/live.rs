#![cfg(feature = "stripe-live")]

//! The `stripe-live` lane (M10, ADR-030): the SAME adapter protocol as
//! `checkout.rs`, driven against the REAL Stripe sandbox at `api.stripe.com`
//! rather than the hermetic `mock-stripe` double.
//!
//! It exists to prove the one thing the mock cannot: that the adapter's REQUEST
//! ENCODING and RESPONSE PARSING match real Stripe — the form fields, the bearer
//! and idempotency headers, the JSON shape of a Checkout Session — not merely the
//! mock we wrote to mirror them. A mock and its adapter can drift together and
//! stay green (ADR-023); real Stripe cannot be talked into agreeing with a wrong
//! request.
//!
//! It is OPT-IN and never part of a normal `cargo test` run — it needs a
//! test-mode key and network, so it lives behind the `stripe-live` feature:
//!
//! ```text
//! STRIPE_SECRET_KEY=sk_test_… cargo test -p stripe-client --features stripe-live
//! ```
//!
//! What it does NOT cover: receiving a real webhook from Stripe (that needs a
//! public endpoint / tunnel, out of scope for a `cargo test`). The webhook path
//! is proven hermetically end-to-end in `townhall-server/tests/payments.rs`, and
//! the signature FORMAT is locked against an independent OpenSSL vector in
//! `townhall-payment`'s `signature.rs` — an independent reference, as decisive as
//! a real Stripe signature for the bytes it signs.

use bld_kernel::Capability as _;
use bld_types::{
    AvailabilityGrant, EffectAttempt, EffectIntentId, Money, PaymentIntentId, PrincipalId, SlotId,
    VenueId,
};
use stripe_client::{StripeClient, StripeRaw, StripeSecretKey};
use townhall_domain::{BookingEffect, OperationKind, SelectedVenueRef};
use townhall_service::{EffectResolver as _, Resolved};

/// The real test-mode key, from the ENVIRONMENT — never argv, never a literal,
/// never logged. Missing means FAIL LOUDLY: this lane is meaningless without it,
/// and a silent skip would let "the live lane is green" mean "the live lane never
/// ran". It must be a SANDBOX key: a `sk_live_…` here would move real money, so
/// the lane refuses anything but `sk_test_…`.
fn secret_key() -> StripeSecretKey {
    let key = std::env::var("STRIPE_SECRET_KEY").expect(
        "the stripe-live lane needs a test-mode key in STRIPE_SECRET_KEY (an sk_test_… value)",
    );
    assert!(
        key.starts_with("sk_test_"),
        "the stripe-live lane refuses a non-sandbox key: it must be an sk_test_… value, never a live key"
    );
    StripeSecretKey::new(key)
}

fn client() -> StripeClient {
    StripeClient::new(
        "https://api.stripe.com",
        secret_key(),
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
        grant: AvailabilityGrant::new("live-grant"),
        payment_ref: None,
    }
}

/// A per-run suffix so the Idempotency-Key of one run never collides with a
/// session Stripe still holds from an earlier run (keys are retained ~24h) — each
/// run gets its own fresh, unexpired session to assert against.
fn unique_suffix() -> String {
    use std::time::{SystemTime, UNIX_EPOCH};
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .expect("after the epoch")
        .as_millis()
        .to_string()
}

/// Create, idempotently retry, and retrieve a real Checkout Session — the whole
/// adapter round-trip against Stripe's sandbox.
#[tokio::test]
async fn the_real_sandbox_creates_retries_and_retrieves_a_session() {
    let stripe = client();
    let suffix = unique_suffix();
    let effect_attempt = attempt(&format!("EFF-LIVE-{suffix}"));
    let our_payment_id = format!("PAY-LIVE-{suffix}");
    let pay = payment(&our_payment_id);

    // CREATE — a real POST /v1/checkout/sessions with our form encoding.
    let created = stripe
        .execute(&pay, &effect_attempt)
        .await
        .expect("Stripe creates the Checkout session");
    let StripeRaw::SessionCreated {
        effect_intent_id,
        stripe_session_id,
        hosted_url,
        payment_intent_id,
        expires_at_ms,
    } = &created
    else {
        panic!("expected a created session, got {created:?}");
    };
    assert_eq!(effect_intent_id, &effect_attempt.id);
    assert!(
        stripe_session_id.starts_with("cs_test_"),
        "a real test-mode session id, got {stripe_session_id}"
    );
    assert!(
        hosted_url.starts_with("https://checkout.stripe.com/"),
        "a real Stripe-hosted Checkout URL, got {hosted_url}"
    );
    assert_eq!(
        payment_intent_id,
        &PaymentIntentId::new(our_payment_id.clone()),
        "real Stripe echoed OUR metadata payment_intent_id back"
    );
    assert!(*expires_at_ms > 0, "a real future expiry");

    // IDEMPOTENT RETRY — the same Idempotency-Key returns the committed session,
    // Stripe's real contract (not just the mock's).
    let repeated = stripe
        .execute(&pay, &effect_attempt)
        .await
        .expect("the idempotent retry returns the same session");
    assert_eq!(
        repeated, created,
        "the same Idempotency-Key must return the committed session at real Stripe"
    );

    // RETRIEVE — the adapter's read path (GET the session by id) against Stripe.
    let retrieved = stripe
        .resolve(&attempt(stripe_session_id), OperationKind::Pay)
        .await
        .expect("Stripe returns the session on retrieval");
    let Resolved::Answer(StripeRaw::SessionRetrieved {
        stripe_session_id: retrieved_id,
        checkout_status,
        payment_status,
        ..
    }) = retrieved
    else {
        panic!("expected a retrieved Stripe session state");
    };
    assert_eq!(&retrieved_id, stripe_session_id);
    assert_eq!(
        checkout_status.as_deref(),
        Some("open"),
        "a just-created, unpaid session is open at Stripe"
    );
    assert_eq!(
        payment_status, "unpaid",
        "and unpaid until a human completes Checkout"
    );
}
