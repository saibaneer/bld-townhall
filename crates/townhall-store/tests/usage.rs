//! The usage meter over real SQLite (migration 0009, ADR-027). The in-memory
//! store proves the logic under a `Mutex`; this proves the actual SQL — the
//! conditional reserve guard, the unique-index meter-once, and the reservation
//! state transitions — including the property a `HashMap` cannot show: two
//! concurrent debits inside real transactions settle exactly once.

use bld_types::{PrincipalId, UsageAccountId, UsageIntentId};
use sqlx::Row;
use townhall_store::SqliteBookingRepository;
use townhall_store::usage::SqlUsageStore;
use townhall_usage::store::{StoreError, UsageStore};

const NOW: u64 = 1_700_000_000_000;
const TTL: u64 = 600_000;

async fn store() -> (SqlUsageStore, sqlx::SqlitePool, tempfile::TempDir) {
    let dir = tempfile::tempdir().expect("a temporary directory");
    let repo = SqliteBookingRepository::open(&dir.path().join("usage.db"))
        .await
        .expect("migrations apply");
    let pool = repo.pool().clone();
    (SqlUsageStore::new(pool.clone()), pool, dir)
}

fn lucy() -> PrincipalId {
    PrincipalId::new("lucy")
}

fn account() -> UsageAccountId {
    UsageAccountId::new("usage-lucy")
}

fn intent(tag: &str) -> UsageIntentId {
    UsageIntentId::new(format!("usage-{tag}"))
}

async fn debit_rows(pool: &sqlx::SqlitePool, intent: &UsageIntentId) -> i64 {
    sqlx::query(
        "SELECT COUNT(*) AS n FROM usage_ledger WHERE kind = 'Debit' AND usage_intent_id = ?",
    )
    .bind(intent.as_str())
    .fetch_one(pool)
    .await
    .expect("count")
    .try_get::<i64, _>("n")
    .expect("n")
}

/// Meter-once over real SQLite: the unique partial index on the settling Debit
/// collapses a replayed debit to one charge, and one audit row.
#[tokio::test]
async fn the_same_intent_meters_once_over_sqlite() {
    let (store, pool, _dir) = store().await;
    store
        .open_account(&account(), &lucy(), 10, NOW)
        .await
        .unwrap();
    let i = intent("turn-1");

    store.reserve(&lucy(), &i, 1, NOW, NOW + TTL).await.unwrap();
    store.debit(&i, 1, NOW + 1).await.unwrap();
    store.debit(&i, 1, NOW + 2).await.unwrap(); // replay

    let balance = store.load_balance(&lucy()).await.unwrap().expect("account");
    assert_eq!(
        balance.debited_units, 1,
        "one intent, one unit, over SQLite"
    );
    assert_eq!(balance.reserved_units, 0);
    assert_eq!(
        debit_rows(&pool, &i).await,
        1,
        "exactly one Debit audit row"
    );
}

/// The conditional reserve guard refuses an over-quota hold, over SQLite, and
/// writes nothing.
#[tokio::test]
async fn an_exhausted_quota_is_refused_over_sqlite() {
    let (store, pool, _dir) = store().await;
    store
        .open_account(&account(), &lucy(), 1, NOW)
        .await
        .unwrap();

    store
        .reserve(&lucy(), &intent("a"), 1, NOW, NOW + TTL)
        .await
        .unwrap();
    let refused = store
        .reserve(&lucy(), &intent("b"), 1, NOW + 1, NOW + 1 + TTL)
        .await;
    assert!(
        matches!(refused, Err(StoreError::QuotaExhausted)),
        "the second hold is refused, not a silent overspend: {refused:?}"
    );

    let balance = store.load_balance(&lucy()).await.unwrap().expect("account");
    assert_eq!(balance.reserved_units, 1, "only the first hold stands");
    assert_eq!(balance.debited_units, 0);
    // And no phantom Reserve row for the refused intent.
    let b_rows: i64 =
        sqlx::query("SELECT COUNT(*) AS n FROM usage_ledger WHERE usage_intent_id = ?")
            .bind(intent("b").as_str())
            .fetch_one(&pool)
            .await
            .unwrap()
            .try_get("n")
            .unwrap();
    assert_eq!(b_rows, 0, "a refused reserve writes no ledger row");
}

/// The property a `HashMap` cannot show: two debits for one intent, racing inside
/// real transactions, settle exactly once.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn two_simultaneous_debits_settle_once() {
    use std::sync::Arc;
    use std::sync::atomic::{AtomicUsize, Ordering};

    for round in 0..8 {
        let (store, pool, _dir) = store().await;
        store
            .open_account(&account(), &lucy(), 10, NOW)
            .await
            .unwrap();
        let store = Arc::new(store);
        let i = intent(&format!("race-{round}"));
        store.reserve(&lucy(), &i, 1, NOW, NOW + TTL).await.unwrap();

        let ok = Arc::new(AtomicUsize::new(0));
        let barrier = Arc::new(std::sync::Barrier::new(2));
        let handles: Vec<_> = (0..2)
            .map(|_| {
                let (store, i, ok, barrier) = (
                    Arc::clone(&store),
                    i.clone(),
                    Arc::clone(&ok),
                    Arc::clone(&barrier),
                );
                tokio::spawn(async move {
                    barrier.wait();
                    store.debit(&i, 1, NOW + 1).await.expect("debit answered");
                    ok.fetch_add(1, Ordering::SeqCst);
                })
            })
            .collect();
        for h in handles {
            h.await.expect("no task panicked");
        }

        assert_eq!(
            ok.load(Ordering::SeqCst),
            2,
            "round {round}: both debits answered"
        );
        let balance = store.load_balance(&lucy()).await.unwrap().expect("account");
        assert_eq!(
            balance.debited_units, 1,
            "round {round}: two racing debits charged exactly one unit"
        );
        assert_eq!(
            debit_rows(&pool, &i).await,
            1,
            "round {round}: one Debit row"
        );
    }
}

/// A stranded reservation is reclaimed by the next reserve over SQLite — the
/// deterministic expiry, so a crashed turn cannot lock the account out.
#[tokio::test]
async fn a_stranded_reservation_is_reclaimed_over_sqlite() {
    let (store, pool, _dir) = store().await;
    store
        .open_account(&account(), &lucy(), 1, NOW)
        .await
        .unwrap();

    // A turn holds the account's only unit, then the process dies (no debit).
    let ttl = 1_000;
    store
        .reserve(&lucy(), &intent("crashed"), 1, NOW, NOW + ttl)
        .await
        .unwrap();

    // Past the TTL, the next reserve reclaims the stranded hold and fits.
    store
        .reserve(
            &lucy(),
            &intent("next"),
            1,
            NOW + ttl + 1,
            NOW + ttl + 1 + ttl,
        )
        .await
        .expect("the stranded unit is reclaimed");

    let balance = store.load_balance(&lucy()).await.unwrap().expect("account");
    assert_eq!(
        balance.reserved_units, 1,
        "only the new hold stands — the stranded one was released, not doubled"
    );

    // The expiry is AUDITED: a Release row names the reclaimed intent, so the
    // ledger stays a self-consistent append-only log (no dangling Reserve).
    let released: i64 = sqlx::query(
        "SELECT COUNT(*) AS n FROM usage_ledger WHERE kind = 'Release' AND usage_intent_id = ?",
    )
    .bind(intent("crashed").as_str())
    .fetch_one(&pool)
    .await
    .unwrap()
    .try_get("n")
    .unwrap();
    assert_eq!(
        released, 1,
        "the expiry sweep logs a Release for the reclaimed hold"
    );
}
