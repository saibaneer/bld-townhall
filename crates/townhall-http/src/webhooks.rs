//! The Stripe webhook route (M10, spec §17; ADR-030).
//!
//! `POST /webhooks/stripe` is the one inbound path a payment can advance through,
//! and it is deliberately unlike every other route here:
//!
//! - It is OUTSIDE the bearer/`authorize_change` gate. A webhook carries no
//!   bearer; its authority is the Stripe SIGNATURE, verified before a single
//!   field of the body is read.
//! - It reads the RAW body (`Bytes`), because Stripe signs the exact bytes — a
//!   parsed-then-reserialized body would not match the signature. Under a small
//!   size cap, because this is the one unauthenticated, mutating endpoint and the
//!   bytes must be buffered to compute the MAC.
//!
//! Because it MUTATES, this crate names only a trusted PORT ([`StripeWebhookPort`],
//! mirroring [`crate::approvals::ApprovalIssuer`]); the implementation — which
//! verifies the signature, maps the event to a booking through `payment_intents`,
//! and drives `Coordinator::observe` — lives in a trusted crate the composition
//! root wires. The port owns the webhook secret and the clock.

use crate::mapping;
use axum::{
    Router,
    body::Bytes,
    extract::State,
    http::{HeaderMap, StatusCode},
    response::{IntoResponse as _, Response},
    routing::post,
};
use std::sync::Arc;

/// The largest webhook body accepted before verification. Stripe events are small
/// (a few KB); this caps the pre-verification buffering so the unauthenticated
/// endpoint cannot be used for memory exhaustion.
const MAX_WEBHOOK_BODY: usize = 256 * 1024;

/// What a Stripe webhook amounted to, once the trusted port has verified and
/// applied it. Every arm is a `200` to Stripe (so it stops retrying a delivery it
/// has made) EXCEPT a rejected signature.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum WebhookOutcome {
    /// A verified event advanced (or converged) the booking.
    Advanced,
    /// A verified but non-advancing event (a card decline, `processing`, a
    /// duplicate) — recorded, no transition.
    Recorded,
    /// The signature verified but the event named nothing this server tracks.
    Ignored,
}

/// Why a webhook was refused BEFORE it could touch any state.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum WebhookRejection {
    /// The `Stripe-Signature` was missing, malformed, expired, or not this
    /// endpoint's secret over these bytes. A `400`, and NOTHING changed — a
    /// success redirect or a forged body cannot advance a payment.
    BadSignature,
    /// The body was larger than [`MAX_WEBHOOK_BODY`], refused before buffering.
    TooLarge,
}

/// The trusted webhook handler. `townhall-http` names only this; the composition
/// root supplies an implementation holding the secret, the payment store, the
/// verifier and the coordinator.
#[async_trait::async_trait]
pub trait StripeWebhookPort: Send + Sync {
    /// Verify the signature over `raw_body` using the `Stripe-Signature` header,
    /// then — only if it verifies — apply the event (dedup, map to the booking,
    /// `observe`). The port owns its own clock.
    ///
    /// # Errors
    /// [`WebhookRejection::BadSignature`] if the signature does not verify; the
    /// body must not have changed anything.
    async fn handle(
        &self,
        raw_body: &[u8],
        signature_header: &str,
    ) -> Result<WebhookOutcome, WebhookRejection>;
}

/// The state the route carries: the trusted handler.
#[derive(Clone)]
pub struct WebhookState {
    pub handler: Arc<dyn StripeWebhookPort>,
}

/// The one route: `POST /webhooks/stripe`. Merged into the server only when
/// payments are enabled (like discovery's opt-in merge), and OUTSIDE the bearer
/// gate.
pub fn webhook_router(state: WebhookState) -> Router {
    Router::new()
        .route("/webhooks/stripe", post(stripe_webhook))
        .with_state(state)
}

async fn stripe_webhook(
    State(state): State<WebhookState>,
    headers: HeaderMap,
    body: Bytes,
) -> Response {
    // Size cap FIRST — before anything reads the bytes.
    if body.len() > MAX_WEBHOOK_BODY {
        return mapping::plain_error(StatusCode::PAYLOAD_TOO_LARGE, "webhook body too large");
    }
    let signature = headers
        .get("stripe-signature")
        .and_then(|value| value.to_str().ok())
        .unwrap_or_default();

    match state.handler.handle(&body, signature).await {
        // A verified event, whatever it did to state, is a 200 so Stripe stops
        // redelivering it.
        Ok(WebhookOutcome::Advanced | WebhookOutcome::Recorded | WebhookOutcome::Ignored) => {
            (StatusCode::OK, "ok").into_response()
        }
        // An unverified webhook changed nothing and is refused. NOT a 200: a
        // caller who cannot sign cannot advance a payment.
        Err(WebhookRejection::BadSignature) => {
            mapping::plain_error(StatusCode::BAD_REQUEST, "invalid webhook signature")
        }
        Err(WebhookRejection::TooLarge) => {
            mapping::plain_error(StatusCode::PAYLOAD_TOO_LARGE, "webhook body too large")
        }
    }
}
