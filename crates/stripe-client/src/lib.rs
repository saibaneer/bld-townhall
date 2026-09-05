#![forbid(unsafe_code)]

//! Stripe Checkout over HTTP, exposed through the external-effect seams.
//!
//! This trusted adapter transports canonical payment plans and returns raw
//! provider observations. It never turns those observations into verified
//! domain facts; that authority belongs to `townhall-payment`.

use async_trait::async_trait;
use bld_kernel::{Capability, Unknown};
use bld_types::{BoundedString, EffectAttempt, PaymentIntentId};
use serde::Deserialize;
use std::{fmt, time::Duration};
use thiserror::Error;
use townhall_domain::{BookingEffect, OperationKind};
use townhall_service::{EffectResolver, Resolved};

pub use townhall_payment::StripeRaw;

/// A Stripe secret API key whose debug representation never reveals the key.
#[derive(Clone)]
pub struct StripeSecretKey(String);

impl StripeSecretKey {
    #[must_use]
    pub fn new(value: impl Into<String>) -> Self {
        Self(value.into())
    }
}

impl fmt::Debug for StripeSecretKey {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str("StripeSecretKey(****)")
    }
}

/// Talks to one Stripe-compatible Checkout API.
pub struct StripeClient {
    http: reqwest::Client,
    base_url: String,
    secret_key: StripeSecretKey,
    success_url: String,
    cancel_url: String,
}

impl StripeClient {
    #[must_use]
    pub fn new(
        base_url: impl Into<String>,
        secret_key: StripeSecretKey,
        success_url: impl Into<String>,
        cancel_url: impl Into<String>,
    ) -> Self {
        Self::with_timeout(
            base_url,
            secret_key,
            success_url,
            cancel_url,
            Duration::from_secs(10),
        )
    }

    #[must_use]
    pub fn with_timeout(
        base_url: impl Into<String>,
        secret_key: StripeSecretKey,
        success_url: impl Into<String>,
        cancel_url: impl Into<String>,
        timeout: Duration,
    ) -> Self {
        Self {
            http: reqwest::Client::builder()
                .timeout(timeout)
                .build()
                .unwrap_or_default(),
            base_url: base_url.into().trim_end_matches('/').to_owned(),
            secret_key,
            success_url: success_url.into(),
            cancel_url: cancel_url.into(),
        }
    }

    async fn send_session(
        &self,
        request: reqwest::RequestBuilder,
    ) -> Result<CheckoutSession, Unknown> {
        let response = request
            .send()
            .await
            .map_err(|error| unknown(format!("Stripe could not be reached: {error}")))?;
        let status = response.status();
        let bytes = response
            .bytes()
            .await
            .map_err(|error| unknown(format!("Stripe's answer could not be read: {error}")))?;
        if !status.is_success() {
            return Err(unknown(format!("Stripe returned HTTP {status}")));
        }
        serde_json::from_slice(&bytes)
            .map_err(|error| unknown(format!("Stripe's answer was unreadable: {error}")))
    }
}

#[async_trait]
impl Capability<BookingEffect> for StripeClient {
    type Raw = StripeRaw;

    async fn execute(
        &self,
        effect: &BookingEffect,
        attempt: &EffectAttempt,
    ) -> Result<Self::Raw, Unknown> {
        let BookingEffect::PreparePayment {
            payment_intent_id,
            amount,
            selection,
            ..
        } = effect
        else {
            return Err(unknown("not a stripe effect"));
        };

        let amount = amount.pence().to_string();
        let product_name = format!(
            "Town hall {} / {}",
            selection.venue_id.as_str(),
            selection.slot_id.as_str()
        );
        let form = [
            ("mode", "payment"),
            ("line_items[0][price_data][currency]", "gbp"),
            ("line_items[0][price_data][unit_amount]", &amount),
            (
                "line_items[0][price_data][product_data][name]",
                &product_name,
            ),
            ("line_items[0][quantity]", "1"),
            ("metadata[payment_intent_id]", payment_intent_id.as_str()),
            ("client_reference_id", payment_intent_id.as_str()),
            ("success_url", &self.success_url),
            ("cancel_url", &self.cancel_url),
        ];
        let session = self
            .send_session(
                self.http
                    .post(format!("{}/v1/checkout/sessions", self.base_url))
                    .bearer_auth(&self.secret_key.0)
                    .header("Idempotency-Key", attempt.id.as_str())
                    .form(&form),
            )
            .await?;

        let returned_payment_id = session
            .metadata
            .payment_intent_id
            .ok_or_else(|| unknown("Stripe omitted payment_intent_id metadata"))?;
        if returned_payment_id != payment_intent_id.as_str() {
            return Err(unknown("Stripe returned a different payment_intent_id"));
        }

        Ok(StripeRaw::SessionCreated {
            effect_intent_id: attempt.id.clone(),
            stripe_session_id: session.id,
            hosted_url: session
                .url
                .ok_or_else(|| unknown("Stripe omitted the hosted Checkout URL"))?,
            payment_intent_id: payment_intent_id.clone(),
            expires_at_ms: milliseconds(session.expires_at)
                .map_err(|error| unknown(error.to_string()))?,
        })
    }
}

#[async_trait]
impl EffectResolver<StripeRaw> for StripeClient {
    async fn resolve(
        &self,
        attempt: &EffectAttempt,
        kind: OperationKind,
    ) -> Result<Resolved<StripeRaw>, Unknown> {
        if kind != OperationKind::Pay {
            return Err(unknown("not a stripe effect"));
        }

        // Layer 4 receives no persisted provider reference through the existing
        // resolver trait. Its caller therefore supplies the Checkout Session id
        // in this standalone attempt; Layer 5 owns the eventual reference route.
        let session = self
            .send_session(
                self.http
                    .get(format!(
                        "{}/v1/checkout/sessions/{}",
                        self.base_url,
                        attempt.id.as_str()
                    ))
                    .bearer_auth(&self.secret_key.0)
                    .query(&[("expand[]", "payment_intent")]),
            )
            .await?;

        Ok(Resolved::Answer(StripeRaw::SessionRetrieved {
            effect_intent_id: attempt.id.clone(),
            stripe_session_id: session.id,
            payment_intent_id: session.metadata.payment_intent_id.map(PaymentIntentId::new),
            checkout_status: session.status,
            payment_status: session.payment_status,
            payment_intent_status: session.payment_intent.and_then(PaymentIntent::status),
            expires_at_ms: milliseconds(session.expires_at)
                .map_err(|error| unknown(error.to_string()))?,
        }))
    }
}

fn unknown(detail: impl Into<String>) -> Unknown {
    Unknown::new(BoundedString::truncating(detail.into()))
}

#[derive(Debug, Error)]
enum ProtocolError {
    #[error("Stripe returned an expiry outside the supported millisecond range")]
    ExpiryOverflow,
}

fn milliseconds(seconds: i64) -> Result<i64, ProtocolError> {
    seconds
        .checked_mul(1_000)
        .ok_or(ProtocolError::ExpiryOverflow)
}

// Stripe wire DTOs are declared here, rather than shared with the mock. A mock
// and adapter importing one representation could drift together while tests
// remained green (ADR-023).
#[derive(Deserialize)]
struct CheckoutSession {
    id: String,
    url: Option<String>,
    expires_at: i64,
    #[serde(default)]
    metadata: CheckoutMetadata,
    #[serde(default)]
    status: Option<String>,
    #[serde(default)]
    payment_status: String,
    #[serde(default)]
    payment_intent: Option<PaymentIntent>,
}

#[derive(Default, Deserialize)]
struct CheckoutMetadata {
    payment_intent_id: Option<String>,
}

#[derive(Deserialize)]
#[serde(untagged)]
enum PaymentIntent {
    Id(String),
    Expanded { status: String },
}

impl PaymentIntent {
    fn status(self) -> Option<String> {
        match self {
            Self::Id(id) => {
                let _ = id;
                None
            }
            Self::Expanded { status } => Some(status),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::{StripeSecretKey, milliseconds};

    #[test]
    fn secret_debug_is_masked() {
        assert_eq!(
            format!("{:?}", StripeSecretKey::new("sk_test_do_not_print")),
            "StripeSecretKey(****)"
        );
    }

    #[test]
    fn stripe_seconds_become_milliseconds_without_saturation() {
        assert_eq!(milliseconds(1_234).expect("in range"), 1_234_000);
        assert!(milliseconds(i64::MAX).is_err());
    }
}
