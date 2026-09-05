//! The human-payment records (M10, ADR-030, migration 0012), over a real
//! migrated SQLite. Each witness would fail a wrong implementation: the freeze is
//! asserted by re-preparing with a DIFFERENT amount and reading the ORIGINAL back;
//! the tombstone by trying to abandon a confirmed intent and reading it still
//! confirmed; the dedup by replaying an `event.id` and seeing a `Duplicate`.

use bld_types::{AvailabilityGrant, BookingId, EffectIntentId, Money, PaymentIntentId};
use tempfile::TempDir;
use townhall_store::SqliteBookingRepository;
use townhall_store::payment::{
    EventOutcome, NewPaymentIntent, PaymentStatus, SessionCreated, SqlPaymentStore,
};

const T0: i64 = 1_700_000_000_000;

async fn store(temp: &TempDir) -> SqlPaymentStore {
    // The booking repository runs the migrations (incl. 0012) and owns the pool.
    let repo = SqliteBookingRepository::open(temp.path().join("townhall.sqlite"))
        .await
        .expect("open the repository (runs migrations)");
    SqlPaymentStore::new(repo.pool().clone())
}

fn intent(id: &str, booking: &str, amount_pence: u64) -> NewPaymentIntent {
    NewPaymentIntent {
        payment_intent_id: PaymentIntentId::new(id),
        booking_id: BookingId::new(booking),
        amount: Money::from_pence(amount_pence),
        currency: "gbp".to_owned(),
        checkout_hash: format!("hash-of-{id}"),
        frozen_grant: AvailabilityGrant::new(format!("grant-for-{id}")),
        threshold_policy_version: "m10-fixed-v1".to_owned(),
    }
}

/// W1: `prepare` freezes the canonical checkout, and a re-prepare is a no-op — the
/// frozen amount cannot be silently rewritten (idempotent on the id).
#[tokio::test]
async fn prepare_freezes_the_checkout_and_is_idempotent() {
    let temp = TempDir::new().expect("temp dir");
    let store = store(&temp).await;
    let id = PaymentIntentId::new("PAY-1");

    store
        .prepare(&intent("PAY-1", "BKG-1", 12_000), T0)
        .await
        .expect("prepare");
    // A second prepare with a DIFFERENT amount must NOT overwrite the freeze.
    store
        .prepare(&intent("PAY-1", "BKG-1", 99_999), T0 + 10)
        .await
        .expect("re-prepare is a no-op");

    let got = store
        .find(&id)
        .await
        .expect("find")
        .expect("the intent exists");
    assert_eq!(
        got.amount,
        Money::from_pence(12_000),
        "the ORIGINAL amount is frozen"
    );
    assert_eq!(got.status, PaymentStatus::Prepared);
    assert_eq!(got.frozen_grant.on_the_wire(), "grant-for-PAY-1");
    assert!(got.stripe_session_id.is_none(), "no session yet");
    assert!(got.await_effect_intent_id.is_none());
}

/// W2: recording the session moves `prepared -> awaiting`, binds the await intent,
/// and is reachable by the Stripe session id (the webhook's lookup path).
#[tokio::test]
async fn record_session_binds_the_await_intent_and_is_findable_by_session() {
    let temp = TempDir::new().expect("temp dir");
    let store = store(&temp).await;
    let id = PaymentIntentId::new("PAY-2");
    store
        .prepare(&intent("PAY-2", "BKG-2", 12_000), T0)
        .await
        .expect("prepare");

    store
        .record_session(
            &SessionCreated {
                payment_intent_id: id.clone(),
                stripe_session_id: "cs_test_abc".to_owned(),
                hosted_url: "https://checkout.stripe.test/cs_test_abc".to_owned(),
                await_effect_intent_id: EffectIntentId::new("EFF-BKG-2-PAY-4"),
                expires_at_ms: T0 + 86_400_000,
            },
            T0 + 100,
        )
        .await
        .expect("record session");

    let by_session = store
        .find_by_session("cs_test_abc")
        .await
        .expect("find_by_session")
        .expect("the webhook can reach the intent by its session id");
    assert_eq!(by_session.payment_intent_id, id);
    assert_eq!(by_session.booking_id, BookingId::new("BKG-2"));
    assert_eq!(by_session.status, PaymentStatus::Awaiting);
    assert_eq!(
        by_session.await_effect_intent_id,
        Some(EffectIntentId::new("EFF-BKG-2-PAY-4")),
        "the webhook binds to the RECORDED await intent, not a live pointer"
    );
    assert_eq!(by_session.expires_at_ms, Some(T0 + 86_400_000));
}

/// W3: `mark_confirmed` only moves an `awaiting` intent — a `prepared` one (no
/// session yet) is not confirmable, so a stray success cannot skip the session.
#[tokio::test]
async fn confirmation_requires_the_awaiting_state() {
    let temp = TempDir::new().expect("temp dir");
    let store = store(&temp).await;
    let id = PaymentIntentId::new("PAY-3");
    store
        .prepare(&intent("PAY-3", "BKG-3", 12_000), T0)
        .await
        .expect("prepare");

    // Prepared, not awaiting: the guard refuses to confirm.
    store
        .mark_confirmed(&id, T0 + 5)
        .await
        .expect("the write runs");
    assert_eq!(
        store.find(&id).await.expect("find").expect("exists").status,
        PaymentStatus::Prepared,
        "a prepared intent (no session) cannot be confirmed"
    );
}

/// W4: a confirmed intent is a terminal tombstone — a later abandon (a late
/// `session.expired` racing the success) finds nothing to move.
#[tokio::test]
async fn a_confirmed_intent_cannot_be_abandoned() {
    let temp = TempDir::new().expect("temp dir");
    let store = store(&temp).await;
    let id = PaymentIntentId::new("PAY-4");
    store
        .prepare(&intent("PAY-4", "BKG-4", 12_000), T0)
        .await
        .expect("prepare");
    store
        .record_session(
            &SessionCreated {
                payment_intent_id: id.clone(),
                stripe_session_id: "cs_test_def".to_owned(),
                hosted_url: "https://checkout.stripe.test/cs_test_def".to_owned(),
                await_effect_intent_id: EffectIntentId::new("EFF-BKG-4-PAY-4"),
                expires_at_ms: T0 + 86_400_000,
            },
            T0 + 100,
        )
        .await
        .expect("record session");
    store.mark_confirmed(&id, T0 + 200).await.expect("confirm");

    // A late terminal event tries to abandon it — the tombstone holds.
    store
        .mark_abandoned(&id, T0 + 300)
        .await
        .expect("the write runs");
    assert_eq!(
        store.find(&id).await.expect("find").expect("exists").status,
        PaymentStatus::Confirmed,
        "a paid intent stays confirmed; a late abandon is a no-op"
    );
}

/// W5: the webhook dedup ledger — a redelivered `event.id` is a structural no-op,
/// a new one is recorded.
#[tokio::test]
async fn a_replayed_event_id_is_deduped() {
    let temp = TempDir::new().expect("temp dir");
    let store = store(&temp).await;
    let id = PaymentIntentId::new("PAY-5");
    store
        .prepare(&intent("PAY-5", "BKG-5", 12_000), T0)
        .await
        .expect("prepare");

    let first = store
        .record_event("evt_1", &id, "payment_intent.succeeded", "verified", T0)
        .await
        .expect("record");
    assert_eq!(first, EventOutcome::Recorded);

    let replay = store
        .record_event("evt_1", &id, "payment_intent.succeeded", "verified", T0 + 1)
        .await
        .expect("record");
    assert_eq!(
        replay,
        EventOutcome::Duplicate,
        "the same event.id is a no-op"
    );

    let other = store
        .record_event("evt_2", &id, "payment_intent.succeeded", "verified", T0 + 2)
        .await
        .expect("record");
    assert_eq!(
        other,
        EventOutcome::Recorded,
        "a fresh event.id is recorded"
    );
}
