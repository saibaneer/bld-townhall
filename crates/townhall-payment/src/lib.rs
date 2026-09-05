#![forbid(unsafe_code)]

//! The trusted payment verifier (M10, spec §17; ADR-030).
//!
//! # What this layer is
//!
//! The Stripe webhook signature verifier, and nothing else yet: a Stripe webhook
//! is authenticated by an HMAC-SHA256 over `t + "." + raw_body`, keyed by the
//! endpoint's `whsec_` secret (Stripe signs; the server verifies with the same
//! symmetric secret — the codebase's HMAC case, not the ed25519 one). This is the
//! ONE place that check is spelled; the webhook route calls it before it reads a
//! single field of the body.
//!
//! # Why it lives in a trusted crate, apart from the adapter
//!
//! The `stripe-client` adapter handles UNTRUSTED input (raw webhook bytes, Stripe
//! API responses) and must never be able to mint a verified fact. So the check —
//! and, in a later layer, the `Verified<VerifiedProviderFact::Payment*>` it
//! authorises — lives here, in a crate the adapter does not name.

use hmac::{Hmac, KeyInit as _, Mac as _};
use sha2::Sha256;

use bld_kernel::{VerificationError, Verified, Verifier};
use bld_types::{BoundedString, EffectIntentId, PaymentIntentId, PaymentRef};
use townhall_domain::VerifiedProviderFact;

/// Unverified Stripe state returned by the transport adapter.
///
/// The evidence shape lives beside its verifier so the trusted fact-minter does
/// not acquire the adapter's service/store dependency graph. `stripe-client`
/// re-exports it, preserving the adapter-facing `stripe_client::StripeRaw` path.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum StripeRaw {
    SessionCreated {
        effect_intent_id: EffectIntentId,
        stripe_session_id: String,
        hosted_url: String,
        payment_intent_id: PaymentIntentId,
        expires_at_ms: i64,
    },
    SessionRetrieved {
        effect_intent_id: EffectIntentId,
        stripe_session_id: String,
        payment_intent_id: Option<PaymentIntentId>,
        checkout_status: Option<String>,
        payment_status: String,
        payment_intent_status: Option<String>,
        expires_at_ms: i64,
    },
}

/// Establishes the provider facts carried by Stripe API responses.
///
/// The transport adapter deliberately returns only [`StripeRaw`]. This trusted
/// verifier is the audit point where an attributable Stripe observation becomes
/// a fact the domain can bind to its persisted canonical payment plan.
#[derive(Clone, Copy, Debug, Default)]
pub struct StripeVerifier;

impl Verifier<StripeRaw, VerifiedProviderFact> for StripeVerifier {
    fn verify(&self, raw: StripeRaw) -> Result<Verified<VerifiedProviderFact>, VerificationError> {
        let fact = match raw {
            StripeRaw::SessionCreated {
                effect_intent_id,
                stripe_session_id,
                hosted_url,
                payment_intent_id,
                ..
            } => VerifiedProviderFact::SessionCreated {
                effect_intent_id,
                payment_intent_id,
                payment_ref: PaymentRef::new(stripe_session_id),
                hosted_url,
            },
            StripeRaw::SessionRetrieved {
                effect_intent_id,
                stripe_session_id,
                payment_intent_id: Some(payment_intent_id),
                checkout_status: _,
                payment_intent_status,
                ..
            } if payment_intent_status.as_deref() == Some("succeeded") => {
                VerifiedProviderFact::PaymentConfirmed {
                    effect_intent_id,
                    payment_intent_id,
                    payment_ref: PaymentRef::new(stripe_session_id),
                }
            }
            StripeRaw::SessionRetrieved {
                effect_intent_id,
                stripe_session_id,
                payment_intent_id: Some(payment_intent_id),
                checkout_status,
                payment_intent_status,
                ..
            } if checkout_status.as_deref() == Some("expired")
                || payment_intent_status.as_deref() == Some("canceled") =>
            {
                VerifiedProviderFact::PaymentAbandoned {
                    effect_intent_id,
                    payment_intent_id,
                    payment_ref: PaymentRef::new(stripe_session_id),
                }
            }
            StripeRaw::SessionRetrieved { .. } => {
                return Err(VerificationError::Unknown(BoundedString::truncating(
                    "Stripe has not established a terminal payment outcome",
                )));
            }
        };

        Ok(Verified::assert_verified(fact))
    }
}

/// The endpoint's Stripe webhook signing secret (`whsec_…`).
///
/// Unlike the delegation [`EnvelopeKey`](../townhall_authority), there is no
/// minimum-length gate: Stripe's `whsec_` string is used as its raw ASCII bytes,
/// verbatim, as the HMAC key — so the type takes it as given and only guarantees
/// it never prints. A secret in a log line is the secret (ADR-023).
#[derive(Clone)]
pub struct WebhookSecret(Vec<u8>);

impl WebhookSecret {
    /// Take the `whsec_…` string's bytes as the HMAC key.
    #[must_use]
    pub fn new(secret: impl Into<Vec<u8>>) -> Self {
        Self(secret.into())
    }
}

impl std::fmt::Debug for WebhookSecret {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        // Masked completely, and no accessor exists that could print it by accident.
        f.write_str("WebhookSecret(****)")
    }
}

#[derive(Debug, thiserror::Error, PartialEq, Eq)]
pub enum SignatureError {
    /// The `Stripe-Signature` header was not `t=…,v1=…[,v1=…]`.
    #[error("the Stripe-Signature header is malformed")]
    MalformedHeader,
    /// The header carried a timestamp but no `v1` signature to check.
    #[error("the Stripe-Signature header carried no v1 signature")]
    NoSignature,
    /// The timestamp is outside the tolerance window — a replay defence.
    #[error("the webhook timestamp is {skew_secs}s from now, outside tolerance")]
    Expired { skew_secs: i64 },
    /// No `v1` entry is this secret's tag over `t.body`.
    #[error("no v1 signature verifies against the webhook secret")]
    BadSignature,
}

/// Verify a Stripe webhook signature over the RAW body.
///
/// `signature_header` is the `Stripe-Signature` value; `now_unix`/`tolerance_secs`
/// are supplied by the caller (the port owns the clock) so this is a pure,
/// testable function. Both the timestamp tolerance (crypto anti-replay) and the
/// signature must pass.
///
/// Stripe may carry MORE THAN ONE `v1=` entry during secret rotation; this accepts
/// if ANY of them verifies. The signed message is `t + "." + raw_body`, using the
/// timestamp EXACTLY as it appeared in the header (not a re-serialized parse), so
/// the bytes match what Stripe signed.
///
/// # Errors
/// [`SignatureError::MalformedHeader`] / [`SignatureError::NoSignature`] for a bad
/// header, [`SignatureError::Expired`] outside the window, [`SignatureError::BadSignature`]
/// if no `v1` matches.
pub fn verify_webhook(
    secret: &WebhookSecret,
    raw_body: &[u8],
    signature_header: &str,
    now_unix: i64,
    tolerance_secs: i64,
) -> Result<(), SignatureError> {
    let mut timestamp_field: Option<&str> = None;
    let mut v1s: Vec<&str> = Vec::new();
    for part in signature_header.split(',') {
        let (scheme, value) = part
            .split_once('=')
            .ok_or(SignatureError::MalformedHeader)?;
        match scheme.trim() {
            "t" => timestamp_field = Some(value.trim()),
            "v1" => v1s.push(value.trim()),
            // Other schemes (e.g. the legacy `v0`) are ignored, not rejected —
            // matching Stripe's own reference verifier.
            _ => {}
        }
    }

    let timestamp_field = timestamp_field.ok_or(SignatureError::MalformedHeader)?;
    let timestamp: i64 = timestamp_field
        .parse()
        .map_err(|_| SignatureError::MalformedHeader)?;
    if v1s.is_empty() {
        return Err(SignatureError::NoSignature);
    }

    // Anti-replay: the signed timestamp must be recent. Checked BEFORE the HMAC so
    // a valid-but-old captured event cannot be replayed.
    let skew = now_unix - timestamp;
    if skew.abs() > tolerance_secs {
        return Err(SignatureError::Expired { skew_secs: skew });
    }

    // The signed message is the timestamp field's bytes, a literal '.', then the
    // exact raw body — Stripe signs the bytes as received.
    let mut signed = Vec::with_capacity(timestamp_field.len() + 1 + raw_body.len());
    signed.extend_from_slice(timestamp_field.as_bytes());
    signed.push(b'.');
    signed.extend_from_slice(raw_body);

    for candidate in &v1s {
        let Some(tag) = decode_hex(candidate) else {
            continue;
        };
        if tag.len() != 32 {
            continue;
        }
        // HMAC accepts a key of any length, so this never errs; handling it
        // (rather than `expect`) keeps the function panic-free.
        let Ok(mut mac) = Hmac::<Sha256>::new_from_slice(&secret.0) else {
            continue;
        };
        mac.update(&signed);
        // `verify_slice` compares in constant time — never `==` over a
        // secret-dependent value.
        if mac.verify_slice(&tag).is_ok() {
            return Ok(());
        }
    }
    Err(SignatureError::BadSignature)
}

/// Mint a valid `Stripe-Signature` header over `raw_body` for `timestamp` — the
/// exact inverse of [`verify_webhook`].
///
/// `sign_webhook(secret, body, t)` returns a `t=…,v1=…` header that
/// `verify_webhook(secret, body, header, t, tol)` accepts. Behind the
/// `test-signing` feature so it never enters a production build: an end-to-end
/// harness (and the `stripe-live` lane) signs HERE, so the signed-bytes format
/// lives in one place and a test can never pass against a subtly different one.
///
/// # Panics
/// Never in practice: `Hmac::<Sha256>` accepts a key of any length, so the keying
/// step cannot fail — the `expect` documents that invariant rather than a
/// reachable panic.
#[cfg(feature = "test-signing")]
#[must_use]
pub fn sign_webhook(secret: &WebhookSecret, raw_body: &[u8], timestamp: i64) -> String {
    let ts = timestamp.to_string();
    let mut signed = Vec::with_capacity(ts.len() + 1 + raw_body.len());
    signed.extend_from_slice(ts.as_bytes());
    signed.push(b'.');
    signed.extend_from_slice(raw_body);
    // HMAC accepts a key of any length, so this never errs.
    let mut mac = Hmac::<Sha256>::new_from_slice(&secret.0).expect("HMAC accepts any key length");
    mac.update(&signed);
    let tag = mac.finalize().into_bytes();
    let mut hex = String::with_capacity(tag.len() * 2);
    for byte in tag {
        use std::fmt::Write as _;
        let _ = write!(hex, "{byte:02x}");
    }
    format!("t={ts},v1={hex}")
}

/// Decode an even-length hex string to bytes — a variable-length mirror of
/// `bld_manifest::decode_hex_32`, kept dependency-free.
fn decode_hex(hex: &str) -> Option<Vec<u8>> {
    if hex.len() % 2 != 0 {
        return None;
    }
    (0..hex.len() / 2)
        .map(|i| u8::from_str_radix(hex.get(2 * i..2 * i + 2)?, 16).ok())
        .collect()
}
