//! The Stripe webhook handler (M10, ADR-030) — the trusted implementation the
//! composition root wires behind `townhall_http::webhooks::StripeWebhookPort`.
//!
//! This is the ONE place a payment advances, and its order is the security
//! contract: verify the HMAC signature over the RAW bytes BEFORE anything reads a
//! field; then map the event to the booking through `payment_intents`; then mint
//! the fact and `observe` it. `observe` is idempotent (the version-CAS +
//! `active_effect` guard), so a duplicate webhook converges rather than double-
//! advancing, and `payment_events` is an audit ledger — never a gate consulted
//! before `observe`.

use std::sync::Arc;

use bld_kernel::Verifier as _;
use bld_types::PaymentIntentId;
use stripe_client::StripeRaw;
use townhall_http::webhooks::{StripeWebhookPort, WebhookOutcome, WebhookRejection};
use townhall_payment::{StripeVerifier, WebhookSecret, verify_webhook};
use townhall_service::PaymentObserver;
use townhall_store::payment::SqlPaymentStore;

/// Stripe's default replay-tolerance window, in seconds.
const TOLERANCE_SECS: i64 = 300;

/// The trusted handler: it owns the webhook secret and the clock, verifies, and
/// drives the boundary through the `PaymentObserver` port (never the concrete
/// coordinator type).
pub struct StripeWebhookHandler {
    secret: WebhookSecret,
    payments: Arc<SqlPaymentStore>,
    observer: Arc<dyn PaymentObserver>,
}

impl StripeWebhookHandler {
    pub fn new(
        secret: WebhookSecret,
        payments: Arc<SqlPaymentStore>,
        observer: Arc<dyn PaymentObserver>,
    ) -> Self {
        Self {
            secret,
            payments,
            observer,
        }
    }
}

// The Stripe event, pared to what the handler needs. Re-declared independently of
// any Stripe SDK (ADR-023) — the wire shape is the contract.
#[derive(serde::Deserialize)]
struct StripeEvent {
    id: String,
    #[serde(rename = "type")]
    event_type: String,
    data: EventData,
}

#[derive(serde::Deserialize)]
struct EventData {
    object: EventObject,
}

#[derive(serde::Deserialize)]
struct EventObject {
    id: String,
    #[serde(default)]
    metadata: Metadata,
}

#[derive(serde::Deserialize, Default)]
struct Metadata {
    #[serde(default)]
    payment_intent_id: Option<String>,
}

#[async_trait::async_trait]
impl StripeWebhookPort for StripeWebhookHandler {
    async fn handle(
        &self,
        raw_body: &[u8],
        signature_header: &str,
    ) -> Result<WebhookOutcome, WebhookRejection> {
        let now_secs = now_unix_secs();

        // SECURITY GATE, first and unconditional: the signature over the RAW bytes.
        // A success redirect, a forged body, or an agent's claim has no valid
        // signature and is refused here, before a field is read.
        verify_webhook(
            &self.secret,
            raw_body,
            signature_header,
            now_secs,
            TOLERANCE_SECS,
        )
        .map_err(|_| WebhookRejection::BadSignature)?;

        // Verified: the event is genuine. Parse and map it to a booking through the
        // payment records. An event we cannot map is Ignored (a 200 — Stripe should
        // not keep redelivering it), never an error and never an advance.
        let Ok(event) = serde_json::from_slice::<StripeEvent>(raw_body) else {
            return Ok(WebhookOutcome::Ignored);
        };
        let Some(our_id) = event.data.object.metadata.payment_intent_id.clone() else {
            return Ok(WebhookOutcome::Ignored);
        };
        let payment_intent_id = PaymentIntentId::new(our_id);
        let Ok(Some(record)) = self.payments.find(&payment_intent_id).await else {
            return Ok(WebhookOutcome::Ignored);
        };
        // The AWAIT effect id recorded FOR THIS session — the id `observe`'s fact
        // must carry, so a stale/cross-session late success is rejected by the CAS.
        let Some(await_id) = record.await_effect_intent_id.clone() else {
            return Ok(WebhookOutcome::Ignored);
        };
        let session_ref = record
            .stripe_session_id
            .clone()
            .unwrap_or_else(|| event.data.object.id.clone());

        let now_ms = now_secs.saturating_mul(1000);
        // The dedup ledger — audit + defence-in-depth, written the same as any
        // verified event. It NEVER gates `observe` below (that would strand a
        // confirmed payment on a crash between insert and advance); exactly-once is
        // the CAS + active_effect guard inside `observe`.
        let _ = self
            .payments
            .record_event(
                &event.id,
                &payment_intent_id,
                &event.event_type,
                "verified",
                now_ms,
            )
            .await;

        // Only genuinely terminal outcomes advance. A decline / processing / 3DS is
        // recorded and parks (no `observe`, no transition).
        let (raw, is_terminal_success) = match event.event_type.as_str() {
            "checkout.session.completed" | "payment_intent.succeeded" => (
                StripeRaw::SessionRetrieved {
                    effect_intent_id: await_id,
                    stripe_session_id: session_ref,
                    payment_intent_id: Some(payment_intent_id.clone()),
                    checkout_status: Some("complete".to_owned()),
                    payment_status: "paid".to_owned(),
                    payment_intent_status: Some("succeeded".to_owned()),
                    expires_at_ms: 0,
                },
                Some(true),
            ),
            "checkout.session.expired" | "payment_intent.canceled" => (
                StripeRaw::SessionRetrieved {
                    effect_intent_id: await_id,
                    stripe_session_id: session_ref,
                    payment_intent_id: Some(payment_intent_id.clone()),
                    checkout_status: Some("expired".to_owned()),
                    payment_status: "unpaid".to_owned(),
                    payment_intent_status: Some("canceled".to_owned()),
                    expires_at_ms: 0,
                },
                Some(false),
            ),
            // Non-terminal (payment_failed / processing / requires_action): recorded
            // above, no advance.
            _ => return Ok(WebhookOutcome::Recorded),
        };

        // Mint the fact in the trusted verifier and carry it into the boundary.
        let Ok(fact) = StripeVerifier.verify(raw) else {
            return Ok(WebhookOutcome::Ignored);
        };
        let _ = self.observer.observe_fact(&record.booking_id, fact).await;
        match is_terminal_success {
            Some(true) => {
                let _ = self
                    .payments
                    .mark_confirmed(&payment_intent_id, now_ms)
                    .await;
            }
            _ => {
                let _ = self
                    .payments
                    .mark_abandoned(&payment_intent_id, now_ms)
                    .await;
            }
        }
        Ok(WebhookOutcome::Advanced)
    }
}

fn now_unix_secs() -> i64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .ok()
        .and_then(|since| i64::try_from(since.as_secs()).ok())
        .unwrap_or(0)
}
