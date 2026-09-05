//! Stripe webhook signature verification (M10, ADR-030).
//!
//! The decisive witness is a FROZEN known-answer vector whose expected signature
//! was computed by OpenSSL — an implementation independent of Rust's `hmac` crate.
//! That is deliberate (a review should-fix): if the verifier and its test double
//! shared one HMAC assumption, a systematic divergence from Stripe's real
//! algorithm (the `t.raw_body` construction, the key bytes, hex handling) would be
//! wrong-but-self-consistent and pass. An independently-computed vector cannot be.

use townhall_payment::{SignatureError, WebhookSecret, verify_webhook};

// The frozen vector. Recomputed independently with:
//   printf '%s' "$TS.$PAYLOAD" | openssl dgst -sha256 -hmac "$SECRET" -r
const KAT_SECRET: &str = "whsec_kat_fixture_secret_do_not_use_in_prod";
const KAT_TS: i64 = 1_700_000_000;
const KAT_PAYLOAD: &str =
    r#"{"id":"evt_kat_1","type":"payment_intent.succeeded","data":{"object":{"id":"pi_kat"}}}"#;
const KAT_V1: &str = "2c7fb4e4eb24e0b7f034ae4b88f29975e0640c924afafee14a067244c1180e4b";

fn secret() -> WebhookSecret {
    WebhookSecret::new(KAT_SECRET.to_owned())
}

fn kat_header() -> String {
    format!("t={KAT_TS},v1={KAT_V1}")
}

/// W1 (decisive): the OpenSSL-computed vector verifies against our Rust verifier.
#[test]
fn the_frozen_openssl_vector_verifies() {
    verify_webhook(
        &secret(),
        KAT_PAYLOAD.as_bytes(),
        &kat_header(),
        KAT_TS,
        300,
    )
    .expect("the independently-computed Stripe signature verifies");
}

/// W2: one altered body byte breaks the signature.
#[test]
fn a_tampered_body_is_rejected() {
    let tampered = format!("{KAT_PAYLOAD} ");
    let refused = verify_webhook(&secret(), tampered.as_bytes(), &kat_header(), KAT_TS, 300)
        .expect_err("a changed body no longer matches the signature");
    assert_eq!(refused, SignatureError::BadSignature);
}

/// W3: the signature does not verify under a different secret.
#[test]
fn the_wrong_secret_is_rejected() {
    let impostor = WebhookSecret::new("whsec_not_the_endpoint_secret".to_owned());
    let refused = verify_webhook(
        &impostor,
        KAT_PAYLOAD.as_bytes(),
        &kat_header(),
        KAT_TS,
        300,
    )
    .expect_err("a different secret does not produce this tag");
    assert_eq!(refused, SignatureError::BadSignature);
}

/// W4: a valid-but-old event is refused — the replay defence, BEFORE the HMAC.
#[test]
fn an_expired_timestamp_is_rejected() {
    // now is 400s after the signed timestamp; tolerance is 300s.
    let refused = verify_webhook(
        &secret(),
        KAT_PAYLOAD.as_bytes(),
        &kat_header(),
        KAT_TS + 400,
        300,
    )
    .expect_err("outside the tolerance window");
    assert!(
        matches!(refused, SignatureError::Expired { .. }),
        "an old event is a replay, not a bad signature: {refused:?}"
    );
}

/// W5: during secret rotation Stripe sends MULTIPLE v1 entries; any one matching
/// accepts (a bogus one alongside the real one must not break verification).
#[test]
fn any_of_several_v1_signatures_may_match() {
    let bogus = "0".repeat(64);
    let header = format!("t={KAT_TS},v1={bogus},v1={KAT_V1}");
    verify_webhook(&secret(), KAT_PAYLOAD.as_bytes(), &header, KAT_TS, 300)
        .expect("a real v1 alongside a bogus one still verifies");
}

/// W6: a header that is not `t=…,v1=…` is malformed, not silently accepted.
#[test]
fn a_malformed_header_is_rejected() {
    let refused = verify_webhook(&secret(), KAT_PAYLOAD.as_bytes(), "garbage", KAT_TS, 300)
        .expect_err("no scheme=value pairs");
    assert_eq!(refused, SignatureError::MalformedHeader);
}

/// W7: a timestamp with no v1 signature carries nothing to verify.
#[test]
fn a_header_with_no_v1_is_rejected() {
    let refused = verify_webhook(
        &secret(),
        KAT_PAYLOAD.as_bytes(),
        &format!("t={KAT_TS}"),
        KAT_TS,
        300,
    )
    .expect_err("no v1 present");
    assert_eq!(refused, SignatureError::NoSignature);
}

/// W8: the secret never prints — a secret in a log line is the secret.
#[test]
fn the_secret_is_masked_in_debug() {
    let shown = format!("{:?}", secret());
    assert_eq!(shown, "WebhookSecret(****)");
    assert!(
        !shown.contains("whsec"),
        "the raw secret must never appear in Debug: {shown}"
    );
}
