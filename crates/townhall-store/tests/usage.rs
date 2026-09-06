//! The usage meter over real SQLite (migration 0009, ADR-027). The in-memory
//! store proves the logic under a `Mutex`; this proves the actual SQL — the
//! conditional reserve guard, the unique-index meter-once, and the reservation
//! state transitions — including the property a `HashMap` cannot show: two
//! concurrent debits inside real transactions settle exactly once.

use bld_types::{PrincipalId, UsageAccountId, UsageIntentId};
use sqlx::Row;
use townhall_store::SqliteBookingRepository;
use townhall_store::usage::SqlUsageStore;
use townhall_usage::store::{RateLimits, StoreError, UsageStore};

const NOW: u64 = 1_700_000_000_000;
const TTL: u64 = 600_000;
/// A channel key for the M8-1 quota/expiry tests.
const CH: &str = "sim|townhall";
/// Generous ceilings, so only the quota under test can refuse in these M8-1 tests.
/// The M8-2 rate/budget tests live in their own file with tight ceilings.
const LIMITS: RateLimits = RateLimits {
    principal_max: 1_000_000,
    principal_window_ms: 3_600_000,
    channel_max: 1_000_000,
    channel_window_ms: 3_600_000,
    global_max: 1_000_000,
    global_window_ms: 3_600_000,
};

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

    store
        .reserve(&lucy(), &i, CH, 1, NOW, NOW + TTL, LIMITS)
        .await
        .unwrap();
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
        .reserve(&lucy(), &intent("a"), CH, 1, NOW, NOW + TTL, LIMITS)
        .await
        .unwrap();
    let refused = store
        .reserve(&lucy(), &intent("b"), CH, 1, NOW + 1, NOW + 1 + TTL, LIMITS)
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
        store
            .reserve(&lucy(), &i, CH, 1, NOW, NOW + TTL, LIMITS)
            .await
            .unwrap();

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
        .reserve(&lucy(), &intent("crashed"), CH, 1, NOW, NOW + ttl, LIMITS)
        .await
        .unwrap();

    // Past the TTL, the next reserve reclaims the stranded hold and fits.
    store
        .reserve(
            &lucy(),
            &intent("next"),
            CH,
            1,
            NOW + ttl + 1,
            NOW + ttl + 1 + ttl,
            LIMITS,
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

// ---------------------------------------------------------------- M8-2 (ADR-028)

/// Rate ceilings for the M8-2 tests: one dimension tight, the rest generous, a
/// shared `window_ms`.
fn rate(principal_max: i64, channel_max: i64, global_max: i64, window_ms: u64) -> RateLimits {
    RateLimits {
        principal_max,
        principal_window_ms: window_ms,
        channel_max,
        channel_window_ms: window_ms,
        global_max,
        global_window_ms: window_ms,
    }
}

/// Reservation idempotency over real SQLite (M13): a REDELIVERED reserve for one
/// intent holds once AND spends no second rate token. With a principal ceiling of
/// 2 per window: reserve A, reserve A AGAIN (a carrier redelivery), then reserve a
/// distinct B — B must still be admitted, which is only true if the duplicate A
/// consumed no second token. A wrong impl that re-incremented the rate counter on
/// the retry would exhaust the ceiling and refuse B. Complements the in-memory
/// witness with the durable rate-counter path.
#[tokio::test]
async fn a_redelivered_reserve_over_sqlite_holds_and_spends_once() {
    let (store, _pool, _dir) = store().await;
    store
        .open_account(&account(), &lucy(), 1_000_000, NOW)
        .await
        .expect("account");
    let limits = rate(2, 1_000_000, 1_000_000, TTL);

    let a = intent("a");
    store
        .reserve(&lucy(), &a, CH, 1, NOW, NOW + TTL, limits)
        .await
        .expect("first reserve of A");
    // The SAME intent again — a redelivery. Idempotent: no second hold, no token.
    store
        .reserve(&lucy(), &a, CH, 1, NOW + 1, NOW + 1 + TTL, limits)
        .await
        .expect("a redelivered reserve of A is a no-op, not an error");
    assert_eq!(
        store
            .load_balance(&lucy())
            .await
            .unwrap()
            .expect("account")
            .reserved_units,
        1,
        "one hold for A, not two, after the redelivery"
    );

    // A distinct B still fits under the 2/window ceiling — proof the duplicate A
    // burned no second rate token.
    store
        .reserve(&lucy(), &intent("b"), CH, 1, NOW + 2, NOW + 2 + TTL, limits)
        .await
        .expect("B is still admitted; the redelivery did not double-spend the rate token");
    assert_eq!(
        store
            .load_balance(&lucy())
            .await
            .unwrap()
            .expect("account")
            .reserved_units,
        2,
        "A and B hold; the redelivery of A added nothing"
    );
}

/// The rate gate over real SQLite: the (N+1)th turn in a window is refused as a
/// rate limit, and the next window recovers. The account quota is generous, so
/// only the rate can refuse.
#[tokio::test]
async fn a_principal_rate_over_sqlite_refuses_then_recovers() {
    let (store, _pool, _dir) = store().await;
    store
        .open_account(&account(), &lucy(), 1_000_000, NOW)
        .await
        .unwrap();
    let window = 1_000;
    let limits = rate(2, 1_000_000, 1_000_000, window);

    store
        .reserve(&lucy(), &intent("a"), CH, 1, NOW, NOW + TTL, limits)
        .await
        .unwrap();
    store
        .reserve(&lucy(), &intent("b"), CH, 1, NOW + 1, NOW + 1 + TTL, limits)
        .await
        .unwrap();
    let refused = store
        .reserve(&lucy(), &intent("c"), CH, 1, NOW + 2, NOW + 2 + TTL, limits)
        .await;
    assert!(
        matches!(refused, Err(StoreError::PrincipalRateLimited)),
        "the window's allowance is spent: {refused:?}"
    );
    // Next window.
    store
        .reserve(
            &lucy(),
            &intent("d"),
            CH,
            1,
            NOW + window,
            NOW + window + TTL,
            limits,
        )
        .await
        .expect("a new window is a fresh allowance");
}

/// The rate counter is race-safe: two turns racing at `principal_max = 1` inside
/// real transactions yield exactly one Ok and one rate denial — never two Oks.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn two_racing_turns_at_rate_one_admit_exactly_one() {
    use std::sync::Arc;
    use std::sync::atomic::{AtomicUsize, Ordering};

    for round in 0..8 {
        let (store, _pool, _dir) = store().await;
        store
            .open_account(&account(), &lucy(), 1_000_000, NOW)
            .await
            .unwrap();
        let store = Arc::new(store);
        let limits = rate(1, 1_000_000, 1_000_000, 1_000);

        let ok = Arc::new(AtomicUsize::new(0));
        let denied = Arc::new(AtomicUsize::new(0));
        let barrier = Arc::new(std::sync::Barrier::new(2));
        let handles: Vec<_> = (0..2)
            .map(|n| {
                let (store, ok, denied, barrier) = (
                    Arc::clone(&store),
                    Arc::clone(&ok),
                    Arc::clone(&denied),
                    Arc::clone(&barrier),
                );
                let i = intent(&format!("r{round}-{n}"));
                tokio::spawn(async move {
                    barrier.wait();
                    match store
                        .reserve(&lucy(), &i, CH, 1, NOW, NOW + TTL, limits)
                        .await
                    {
                        Ok(()) => ok.fetch_add(1, Ordering::SeqCst),
                        Err(StoreError::PrincipalRateLimited) => {
                            denied.fetch_add(1, Ordering::SeqCst)
                        }
                        Err(other) => panic!("round {round}: unexpected {other:?}"),
                    };
                })
            })
            .collect();
        for h in handles {
            h.await.expect("no task panicked");
        }
        assert_eq!(
            ok.load(Ordering::SeqCst),
            1,
            "round {round}: exactly one admitted"
        );
        assert_eq!(
            denied.load(Ordering::SeqCst),
            1,
            "round {round}: exactly one refused"
        );
    }
}

/// The global budget over SQLite refuses a DIFFERENT principal once the shared
/// window is spent — even with that principal's own quota and rate untouched —
/// and recovers next window. This is the cross-principal property a per-account
/// guard cannot show.
#[tokio::test]
async fn the_global_budget_over_sqlite_refuses_a_second_principal() {
    let (store, _pool, _dir) = store().await;
    let marco = PrincipalId::new("marco");
    store
        .open_account(&account(), &lucy(), 1_000_000, NOW)
        .await
        .unwrap();
    store
        .open_account(&UsageAccountId::new("usage-marco"), &marco, 1_000_000, NOW)
        .await
        .unwrap();
    let window = 1_000;
    let limits = rate(1_000_000, 1_000_000, 1, window);

    // Lucy takes the one global turn this window.
    store
        .reserve(&lucy(), &intent("l"), "sim|a", 1, NOW, NOW + TTL, limits)
        .await
        .unwrap();
    // Marco — untouched quota, untouched personal rate — is still refused.
    let refused = store
        .reserve(
            &marco,
            &intent("m"),
            "sim|b",
            1,
            NOW + 1,
            NOW + 1 + TTL,
            limits,
        )
        .await;
    assert!(
        matches!(refused, Err(StoreError::ProviderBudgetExhausted)),
        "the global window is spent for everyone: {refused:?}"
    );
    // Next window: Marco recovers.
    store
        .reserve(
            &marco,
            &intent("m2"),
            "sim|b",
            1,
            NOW + window,
            NOW + window + TTL,
            limits,
        )
        .await
        .expect("a new global window");
}
