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

/// W7: a resumed `PreparePayment` whose create-session call timed out AFTER the
/// local prepare committed re-settles at the same frozen version — and must NOT
/// rebind the session or the await intent. The `AND status = 'prepared'` guard in
/// `record_session` makes the second settle a structural no-op, so the ORIGINAL
/// session id (the one the webhook carries) and its await intent survive unchanged.
/// This is the store half of "a booking owed is a booking made, exactly once": a
/// late or retried Stripe session cannot silently rebind the intent to the wrong
/// await effect.
#[tokio::test]
async fn a_second_record_session_does_not_rebind_the_session_or_await_intent() {
    let temp = TempDir::new().expect("temp dir");
    let store = store(&temp).await;
    let id = PaymentIntentId::new("PAY-6");
    store
        .prepare(&intent("PAY-6", "BKG-6", 12_000), T0)
        .await
        .expect("prepare");

    // First settle: the session is created and bound (prepared -> awaiting).
    store
        .record_session(
            &SessionCreated {
                payment_intent_id: id.clone(),
                stripe_session_id: "cs_test_A".to_owned(),
                hosted_url: "https://checkout.stripe.test/cs_test_A".to_owned(),
                await_effect_intent_id: EffectIntentId::new("EFF-BKG-6-PAY-4"),
                expires_at_ms: T0 + 86_400_000,
            },
            T0 + 100,
        )
        .await
        .expect("record session");

    // The first create-session call timed out post-commit; the reconciler resumes
    // and re-settles, this time carrying a DIFFERENT (late/retried) session id and
    // await intent. The write runs, but the `prepared` guard must match zero rows.
    store
        .record_session(
            &SessionCreated {
                payment_intent_id: id.clone(),
                stripe_session_id: "cs_test_B".to_owned(),
                hosted_url: "https://checkout.stripe.test/cs_test_B".to_owned(),
                await_effect_intent_id: EffectIntentId::new("EFF-BKG-6-PAY-99"),
                expires_at_ms: T0 + 86_400_000,
            },
            T0 + 200,
        )
        .await
        .expect("the write runs (it simply matches no rows)");

    let got = store.find(&id).await.expect("find").expect("exists");
    assert_eq!(got.status, PaymentStatus::Awaiting);
    assert_eq!(
        got.stripe_session_id.as_deref(),
        Some("cs_test_A"),
        "the ORIGINAL session binding is never overwritten by a resumed re-settle"
    );
    assert_eq!(
        got.await_effect_intent_id,
        Some(EffectIntentId::new("EFF-BKG-6-PAY-4")),
        "the await intent the webhook advances is not rebound"
    );
    assert!(
        store
            .find_by_session("cs_test_A")
            .await
            .expect("lookup")
            .is_some(),
        "the first session still reaches its intent"
    );
    assert!(
        store
            .find_by_session("cs_test_B")
            .await
            .expect("lookup")
            .is_none(),
        "the second, retried session was never bound"
    );
}

/// W8: the payment-events dedupe ledger is atomic under CONCURRENT redelivery, not
/// only sequentially (W5). Two webhook redeliveries of the SAME event id, racing on
/// two pooled connections, must resolve to exactly one `Recorded` and one
/// `Duplicate` — the `ON CONFLICT(event_id) DO NOTHING` on the `event_id` primary
/// key serialises them. (This witnesses LEDGER integrity under a race; advancing the
/// booking exactly once is separately the observe version-CAS, proven under real
/// concurrency in `townhall-service` protocol tests. The handler discards this
/// result, so the ledger is defence-in-depth, never the advance-gate.)
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn concurrent_redeliveries_record_the_event_exactly_once() {
    use sqlx::Row;
    use std::sync::Arc;
    use std::sync::atomic::{AtomicUsize, Ordering};

    for round in 0..8 {
        let temp = TempDir::new().expect("temp dir");
        // Construct inline (mirroring store()'s body) so we keep `repo` for the
        // authoritative COUNT read — store() throws the pool away.
        let repo = SqliteBookingRepository::open(temp.path().join("townhall.sqlite"))
            .await
            .expect("open the repository (runs migrations)");
        let store = SqlPaymentStore::new(repo.pool().clone());
        store
            .prepare(&intent("PAY-C", "BKG-C", 12_000), T0)
            .await
            .expect("prepare");
        let store = Arc::new(store);

        let event_id = format!("evt_race_{round}");
        let recorded = Arc::new(AtomicUsize::new(0));
        let duplicate = Arc::new(AtomicUsize::new(0));
        let barrier = Arc::new(std::sync::Barrier::new(2));
        let handles: Vec<_> = (0..2)
            .map(|_| {
                let (store, event_id, recorded, duplicate, barrier) = (
                    Arc::clone(&store),
                    event_id.clone(),
                    Arc::clone(&recorded),
                    Arc::clone(&duplicate),
                    Arc::clone(&barrier),
                );
                tokio::spawn(async move {
                    barrier.wait();
                    let outcome = store
                        .record_event(
                            &event_id,
                            &PaymentIntentId::new("PAY-C"),
                            "payment_intent.succeeded",
                            "verified",
                            T0,
                        )
                        .await
                        .expect("record");
                    match outcome {
                        EventOutcome::Recorded => recorded.fetch_add(1, Ordering::SeqCst),
                        EventOutcome::Duplicate => duplicate.fetch_add(1, Ordering::SeqCst),
                    };
                })
            })
            .collect();
        for h in handles {
            h.await.expect("no task panicked");
        }

        // The load-bearing witness: exactly one racer recorded, one saw the
        // duplicate. (COUNT alone cannot catch a mis-classification — event_id is
        // the primary key, so the row is capped at 1 regardless.)
        assert_eq!(
            recorded.load(Ordering::SeqCst),
            1,
            "round {round}: exactly one concurrent redelivery is Recorded"
        );
        assert_eq!(
            duplicate.load(Ordering::SeqCst),
            1,
            "round {round}: the other is classified Duplicate, not double-recorded"
        );
        let n: i64 = sqlx::query("SELECT COUNT(*) AS n FROM payment_events WHERE event_id = ?")
            .bind(&event_id)
            .fetch_one(repo.pool())
            .await
            .unwrap()
            .try_get("n")
            .unwrap();
        assert_eq!(
            n, 1,
            "round {round}: exactly one ledger row for the event id"
        );
    }
}
