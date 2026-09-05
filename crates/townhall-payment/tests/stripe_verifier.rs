use bld_kernel::{VerificationError, Verifier as _};
use bld_types::{EffectIntentId, PaymentIntentId, PaymentRef};
use townhall_domain::VerifiedProviderFact;
use townhall_payment::{StripeRaw, StripeVerifier};

fn retrieved(
    checkout_status: &str,
    payment_status: &str,
    payment_intent_status: &str,
) -> StripeRaw {
    StripeRaw::SessionRetrieved {
        effect_intent_id: EffectIntentId::new("EFF-BKG-PAY-5"),
        stripe_session_id: "cs_test_exact".to_owned(),
        payment_intent_id: Some(PaymentIntentId::new("PAY-BKG-3")),
        checkout_status: Some(checkout_status.to_owned()),
        payment_status: payment_status.to_owned(),
        payment_intent_status: Some(payment_intent_status.to_owned()),
        expires_at_ms: 2_000_000_000_000,
    }
}

#[test]
fn a_created_session_mints_its_exact_payment_fact() {
    let fact = StripeVerifier
        .verify(StripeRaw::SessionCreated {
            effect_intent_id: EffectIntentId::new("EFF-BKG-PAY-4"),
            stripe_session_id: "cs_test_created".to_owned(),
            hosted_url: "https://checkout.stripe.test/cs_test_created".to_owned(),
            payment_intent_id: PaymentIntentId::new("PAY-BKG-3"),
            expires_at_ms: 2_000_000_000_000,
        })
        .expect("created session is provider evidence")
        .into_inner();

    assert_eq!(
        fact,
        VerifiedProviderFact::SessionCreated {
            effect_intent_id: EffectIntentId::new("EFF-BKG-PAY-4"),
            payment_intent_id: PaymentIntentId::new("PAY-BKG-3"),
            payment_ref: PaymentRef::new("cs_test_created"),
            hosted_url: "https://checkout.stripe.test/cs_test_created".to_owned(),
        }
    );
}

#[test]
fn succeeded_and_terminal_statuses_mint_only_the_matching_facts() {
    let confirmed = StripeVerifier
        .verify(retrieved("complete", "paid", "succeeded"))
        .expect("succeeded payment")
        .into_inner();
    assert_eq!(
        confirmed,
        VerifiedProviderFact::PaymentConfirmed {
            effect_intent_id: EffectIntentId::new("EFF-BKG-PAY-5"),
            payment_intent_id: PaymentIntentId::new("PAY-BKG-3"),
            payment_ref: PaymentRef::new("cs_test_exact"),
        }
    );

    let abandoned = StripeVerifier
        .verify(retrieved("expired", "unpaid", "canceled"))
        .expect("terminal abandonment")
        .into_inner();
    assert_eq!(
        abandoned,
        VerifiedProviderFact::PaymentAbandoned {
            effect_intent_id: EffectIntentId::new("EFF-BKG-PAY-5"),
            payment_intent_id: PaymentIntentId::new("PAY-BKG-3"),
            payment_ref: PaymentRef::new("cs_test_exact"),
        }
    );
}

#[test]
fn an_open_or_incomplete_session_establishes_no_fact() {
    let result = StripeVerifier.verify(retrieved("open", "unpaid", "requires_payment_method"));
    assert!(matches!(result, Err(VerificationError::Unknown(_))));
}
