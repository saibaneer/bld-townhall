//! The human-payment records over SQLite (M10, ADR-030, migration 0012).
//!
//! Two rows behind the boundary: `payment_intents` (the canonical-checkout warrant
//! frozen at `OfferSelected`, plus the Stripe session reference and the await
//! effect-intent id the webhook advances) and `payment_events` (the dedup ledger
//! keyed by Stripe's own `event.id`). Every guard is a conditional statement, so
//! each operation is idempotent: a re-prepared intent, a redelivered webhook, and
//! a repeated status transition each collapse to a no-op rather than a second one.
//!
//! This is the STORE layer only — persistence and its idempotency. Which turn
//! calls which method (freeze at `OfferSelected`, record the session at
//! `CheckoutPrepared`, advance on the verified webhook) is wired in the later
//! layers; the fact-minting and the Stripe wire stay out of this crate.

use bld_types::{AvailabilityGrant, BookingId, EffectIntentId, Money, PaymentIntentId};
use sqlx::{Row, SqlitePool};

/// A payment intent's lifecycle. Transitions are conditional UPDATEs, so a
/// `confirmed` intent is a terminal tombstone a later contradicting event cannot
/// move (ADR-016's definitive-determination discipline).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum PaymentStatus {
    /// Frozen at `OfferSelected`; the checkout is derived but no Stripe session
    /// exists yet.
    Prepared,
    /// The Stripe session is created and the human is paying.
    Awaiting,
    /// A verified `payment_intent.succeeded` advanced it.
    Confirmed,
    /// A verified terminal Stripe event (`checkout.session.expired` /
    /// `payment_intent.canceled`) ended it.
    Abandoned,
}

impl PaymentStatus {
    #[must_use]
    pub const fn name(self) -> &'static str {
        match self {
            Self::Prepared => "prepared",
            Self::Awaiting => "awaiting",
            Self::Confirmed => "confirmed",
            Self::Abandoned => "abandoned",
        }
    }

    /// Parse the persisted discriminator.
    ///
    /// # Errors
    /// The unrecognised text, if it is not a known status.
    pub fn parse(text: &str) -> Result<Self, String> {
        match text {
            "prepared" => Ok(Self::Prepared),
            "awaiting" => Ok(Self::Awaiting),
            "confirmed" => Ok(Self::Confirmed),
            "abandoned" => Ok(Self::Abandoned),
            other => Err(other.to_owned()),
        }
    }
}

/// Whether a webhook event was the first of its `event.id` seen, or a redelivery.
/// A duplicate is a structural no-op — the dedup key is Stripe's own id.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum EventOutcome {
    Recorded,
    Duplicate,
}

/// The canonical checkout to freeze at `OfferSelected`.
#[derive(Clone, Debug)]
pub struct NewPaymentIntent {
    pub payment_intent_id: PaymentIntentId,
    pub booking_id: BookingId,
    pub amount: Money,
    pub currency: String,
    /// A hash of the canonical checkout — the integrity witness for "bound to
    /// canonical checkout" (§9.1).
    pub checkout_hash: String,
    /// Re-sent verbatim at the post-payment council Book.
    pub frozen_grant: AvailabilityGrant,
    pub threshold_policy_version: String,
}

/// A payment intent as loaded — what a webhook needs to reach the right booking
/// and the right in-flight await intent.
#[derive(Clone, Debug)]
pub struct PaymentIntentRecord {
    pub payment_intent_id: PaymentIntentId,
    pub booking_id: BookingId,
    pub amount: Money,
    pub currency: String,
    pub checkout_hash: String,
    pub frozen_grant: AvailabilityGrant,
    pub threshold_policy_version: String,
    pub stripe_session_id: Option<String>,
    pub hosted_url: Option<String>,
    pub await_effect_intent_id: Option<EffectIntentId>,
    pub status: PaymentStatus,
    pub expires_at_ms: Option<i64>,
}

#[derive(Debug, thiserror::Error)]
pub enum PaymentStoreError {
    #[error("payment store database error: {0}")]
    Db(#[from] sqlx::Error),
    /// A persisted status string that is not a known [`PaymentStatus`].
    #[error("unknown persisted payment status {0:?}")]
    UnknownStatus(String),
}

/// The Stripe session facts recorded once a Checkout Session is created.
#[derive(Clone, Debug)]
pub struct SessionCreated {
    pub payment_intent_id: PaymentIntentId,
    pub stripe_session_id: String,
    pub hosted_url: String,
    /// The AWAIT Pay-kind intent the succeeded-webhook must advance — recorded
    /// HERE so the webhook binds to it (not the state's live pointer).
    pub await_effect_intent_id: EffectIntentId,
    pub expires_at_ms: i64,
}

/// The `payment_intents` + `payment_events` ports over SQLite.
#[derive(Clone, Debug)]
pub struct SqlPaymentStore {
    pool: SqlitePool,
}

impl SqlPaymentStore {
    #[must_use]
    pub const fn new(pool: SqlitePool) -> Self {
        Self { pool }
    }

    /// Freeze the canonical checkout. Idempotent on `payment_intent_id`: a
    /// re-reached `OfferSelected` (same id) finds the existing row and writes
    /// nothing, so the frozen amount/grant cannot be silently rewritten.
    ///
    /// # Errors
    /// The database rejected the write.
    pub async fn prepare(
        &self,
        intent: &NewPaymentIntent,
        now_ms: i64,
    ) -> Result<(), PaymentStoreError> {
        sqlx::query(
            "INSERT INTO payment_intents \
             (payment_intent_id, booking_id, amount_pence, currency, checkout_hash, \
              frozen_grant, threshold_policy_version, status, created_at_ms, updated_at_ms) \
             VALUES (?, ?, ?, ?, ?, ?, ?, 'prepared', ?, ?) \
             ON CONFLICT(payment_intent_id) DO NOTHING",
        )
        .bind(intent.payment_intent_id.as_str())
        .bind(intent.booking_id.as_str())
        .bind(i64::try_from(intent.amount.pence()).unwrap_or(i64::MAX))
        .bind(&intent.currency)
        .bind(&intent.checkout_hash)
        .bind(intent.frozen_grant.on_the_wire())
        .bind(&intent.threshold_policy_version)
        .bind(now_ms)
        .bind(now_ms)
        .execute(&self.pool)
        .await?;
        Ok(())
    }

    /// Record the created Stripe session and the await intent, moving the intent
    /// `prepared -> awaiting`. Conditional on `prepared`, so a retried creation is
    /// a no-op and a session cannot be attached twice.
    ///
    /// # Errors
    /// The database rejected the write.
    pub async fn record_session(
        &self,
        session: &SessionCreated,
        now_ms: i64,
    ) -> Result<(), PaymentStoreError> {
        sqlx::query(
            "UPDATE payment_intents \
             SET stripe_session_id = ?, hosted_url = ?, await_effect_intent_id = ?, \
                 expires_at_ms = ?, status = 'awaiting', updated_at_ms = ? \
             WHERE payment_intent_id = ? AND status = 'prepared'",
        )
        .bind(&session.stripe_session_id)
        .bind(&session.hosted_url)
        .bind(session.await_effect_intent_id.as_str())
        .bind(session.expires_at_ms)
        .bind(now_ms)
        .bind(session.payment_intent_id.as_str())
        .execute(&self.pool)
        .await?;
        Ok(())
    }

    /// Load an intent by its id.
    ///
    /// # Errors
    /// The database rejected the read, or a row carried an unknown status.
    pub async fn find(
        &self,
        id: &PaymentIntentId,
    ) -> Result<Option<PaymentIntentRecord>, PaymentStoreError> {
        let row = sqlx::query(SELECT_INTENT_BY_ID)
            .bind(id.as_str())
            .fetch_optional(&self.pool)
            .await?;
        row.as_ref().map(decode_intent).transpose()
    }

    /// Load an intent by its Stripe session id — the webhook's lookup path (its
    /// body carries a Stripe id, not our `BookingId`).
    ///
    /// # Errors
    /// The database rejected the read, or a row carried an unknown status.
    pub async fn find_by_session(
        &self,
        stripe_session_id: &str,
    ) -> Result<Option<PaymentIntentRecord>, PaymentStoreError> {
        let row = sqlx::query(SELECT_INTENT_BY_SESSION)
            .bind(stripe_session_id)
            .fetch_optional(&self.pool)
            .await?;
        row.as_ref().map(decode_intent).transpose()
    }

    /// Move an intent to `confirmed`, from `awaiting` only. Idempotent, and a
    /// confirmed intent is a terminal tombstone (a later abandon finds no row to
    /// move).
    ///
    /// # Errors
    /// The database rejected the write.
    pub async fn mark_confirmed(
        &self,
        id: &PaymentIntentId,
        now_ms: i64,
    ) -> Result<(), PaymentStoreError> {
        sqlx::query(
            "UPDATE payment_intents SET status = 'confirmed', updated_at_ms = ? \
             WHERE payment_intent_id = ? AND status = 'awaiting'",
        )
        .bind(now_ms)
        .bind(id.as_str())
        .execute(&self.pool)
        .await?;
        Ok(())
    }

    /// Move an intent to `abandoned`, from `prepared`/`awaiting` only — never from
    /// `confirmed` (a paid intent cannot be abandoned; the tombstone holds against
    /// a late terminal event racing a success).
    ///
    /// # Errors
    /// The database rejected the write.
    pub async fn mark_abandoned(
        &self,
        id: &PaymentIntentId,
        now_ms: i64,
    ) -> Result<(), PaymentStoreError> {
        sqlx::query(
            "UPDATE payment_intents SET status = 'abandoned', updated_at_ms = ? \
             WHERE payment_intent_id = ? AND status IN ('prepared', 'awaiting')",
        )
        .bind(now_ms)
        .bind(id.as_str())
        .execute(&self.pool)
        .await?;
        Ok(())
    }

    /// Record a verified webhook event, deduped on Stripe's `event.id`. A
    /// redelivery hits the primary key and is a no-op ([`EventOutcome::Duplicate`]).
    /// This is a ledger, never a skip-gate: callers still invoke the advance,
    /// which the version CAS + `active_effect` guard make exactly-once.
    ///
    /// # Errors
    /// The database rejected the write.
    pub async fn record_event(
        &self,
        event_id: &str,
        payment_intent_id: &PaymentIntentId,
        event_type: &str,
        verdict: &str,
        now_ms: i64,
    ) -> Result<EventOutcome, PaymentStoreError> {
        let result = sqlx::query(
            "INSERT INTO payment_events \
             (event_id, payment_intent_id, event_type, verdict, received_at_ms) \
             VALUES (?, ?, ?, ?, ?) \
             ON CONFLICT(event_id) DO NOTHING",
        )
        .bind(event_id)
        .bind(payment_intent_id.as_str())
        .bind(event_type)
        .bind(verdict)
        .bind(now_ms)
        .execute(&self.pool)
        .await?;
        Ok(if result.rows_affected() == 1 {
            EventOutcome::Recorded
        } else {
            EventOutcome::Duplicate
        })
    }
}

const SELECT_INTENT_BY_ID: &str = "SELECT payment_intent_id, booking_id, amount_pence, currency, \
     checkout_hash, frozen_grant, threshold_policy_version, stripe_session_id, \
     hosted_url, await_effect_intent_id, status, expires_at_ms \
     FROM payment_intents WHERE payment_intent_id = ?";

const SELECT_INTENT_BY_SESSION: &str = "SELECT payment_intent_id, booking_id, amount_pence, currency, \
     checkout_hash, frozen_grant, threshold_policy_version, stripe_session_id, \
     hosted_url, await_effect_intent_id, status, expires_at_ms \
     FROM payment_intents WHERE stripe_session_id = ?";

fn decode_intent(row: &sqlx::sqlite::SqliteRow) -> Result<PaymentIntentRecord, PaymentStoreError> {
    let status_text: String = row.get("status");
    let status = PaymentStatus::parse(&status_text).map_err(PaymentStoreError::UnknownStatus)?;
    let amount_pence: i64 = row.get("amount_pence");
    Ok(PaymentIntentRecord {
        payment_intent_id: PaymentIntentId::new(row.get::<String, _>("payment_intent_id")),
        booking_id: BookingId::new(row.get::<String, _>("booking_id")),
        amount: Money::from_pence(u64::try_from(amount_pence).unwrap_or(0)),
        currency: row.get("currency"),
        checkout_hash: row.get("checkout_hash"),
        frozen_grant: AvailabilityGrant::new(row.get::<String, _>("frozen_grant")),
        threshold_policy_version: row.get("threshold_policy_version"),
        stripe_session_id: row.get("stripe_session_id"),
        hosted_url: row.get("hosted_url"),
        await_effect_intent_id: row
            .get::<Option<String>, _>("await_effect_intent_id")
            .map(EffectIntentId::new),
        status,
        expires_at_ms: row.get("expires_at_ms"),
    })
}
