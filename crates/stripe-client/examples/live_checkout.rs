//! A hands-on demo of the M10 payment handoff against the REAL Stripe sandbox —
//! the "prove it isn't smoke" tool.
//!
//! Two modes, both through the SAME `StripeClient` the server uses.
//!
//! Create a real Checkout session and print the link a human pays at:
//!
//! ```text
//! STRIPE_SECRET_KEY=sk_test_… cargo run -p stripe-client --example live_checkout
//! ```
//!
//! After paying (test card 4242 4242 4242 4242, any future expiry / CVC), read
//! the session back through our adapter to see it flip to paid:
//!
//! ```text
//! STRIPE_SECRET_KEY=sk_test_… cargo run -p stripe-client --example live_checkout -- <cs_test_…>
//! ```
//!
//! The key is read from the ENVIRONMENT, refused unless it is a sandbox
//! `sk_test_…` key, and never printed. The hosted URL it prints is Stripe's own
//! customer-facing payment page (safe to open/share — it is NOT the API key).

use bld_kernel::Capability as _;
use bld_types::{
    AvailabilityGrant, EffectAttempt, EffectIntentId, Money, PaymentIntentId, PrincipalId, SlotId,
    VenueId,
};
use stripe_client::{StripeClient, StripeRaw, StripeSecretKey};
use townhall_domain::{BookingEffect, OperationKind, SelectedVenueRef};
use townhall_service::{EffectResolver as _, Resolved};

#[tokio::main]
async fn main() {
    let key = std::env::var("STRIPE_SECRET_KEY")
        .expect("set STRIPE_SECRET_KEY to a sandbox sk_test_… key");
    assert!(
        key.starts_with("sk_test_"),
        "refusing a non-sandbox key — use an sk_test_… test-mode key, never a live one"
    );

    let stripe = StripeClient::new(
        "https://api.stripe.com",
        StripeSecretKey::new(key),
        "https://townhall.example/paid",
        "https://townhall.example/cancelled",
    );

    // Retrieve mode: the caller handed us a session id to check.
    if let Some(session_id) = std::env::args().nth(1) {
        let attempt = EffectAttempt {
            id: EffectIntentId::new(&session_id),
            expires_at_ms: 2_000_000_000_000,
        };
        match stripe.resolve(&attempt, OperationKind::Pay).await {
            Ok(Resolved::Answer(StripeRaw::SessionRetrieved {
                stripe_session_id,
                payment_status,
                checkout_status,
                payment_intent_status,
                ..
            })) => {
                println!("Retrieved {stripe_session_id} from Stripe:");
                println!("  checkout status : {checkout_status:?}");
                println!("  payment status  : {payment_status}");
                println!("  payment_intent  : {payment_intent_status:?}");
                if payment_status == "paid" {
                    println!(
                        "\nPAID. This is exactly the evidence StripeVerifier mints a \
                         PaymentConfirmed fact from — the booking would now advance to Booked."
                    );
                } else {
                    println!(
                        "\nNot paid yet. Open the Checkout link, pay with test card \
                         4242 4242 4242 4242, then re-run this with the same id."
                    );
                }
            }
            other => println!("Unexpected retrieval result: {other:?}"),
        }
        return;
    }

    // Create mode: mint a real Checkout session and print the human's link.
    let effect = BookingEffect::PreparePayment {
        principal: PrincipalId::new("lucy"),
        payment_intent_id: PaymentIntentId::new("PAY-DEMO-1"),
        selection: SelectedVenueRef {
            venue_id: VenueId::new("TH-A"),
            slot_id: SlotId::new("SLOT-A"),
        },
        amount: Money::from_pence(4_500),
        grant: AvailabilityGrant::new("demo-grant"),
        payment_ref: None,
    };
    let attempt = EffectAttempt {
        id: EffectIntentId::new("EFF-DEMO-PAY-1"),
        expires_at_ms: 2_000_000_000_000,
    };

    match stripe.execute(&effect, &attempt).await {
        Ok(StripeRaw::SessionCreated {
            stripe_session_id,
            hosted_url,
            ..
        }) => {
            println!("Real Stripe Checkout session created (£45.00, Town Hall TH-A / SLOT-A):\n");
            println!("  Pay here : {hosted_url}");
            println!(
                "  Test card: 4242 4242 4242 4242, any future expiry, any CVC, any postcode\n"
            );
            println!("  Session  : {stripe_session_id}");
            println!(
                "\nAfter paying, run:\n  STRIPE_SECRET_KEY=… cargo run -p stripe-client \
                 --example live_checkout -- {stripe_session_id}"
            );
        }
        other => println!("Could not create a session: {other:?}"),
    }
}
