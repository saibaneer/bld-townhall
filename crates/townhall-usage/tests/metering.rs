//! The meter's semantics, against the in-memory store: meter-once, the quota
//! gate, the reserve/debit/release lifecycle, and the deterministic expiry that
//! keeps a crashed turn from stranding quota. Each test fails a specific wrong
//! implementation (never-fake-tests).

use bld_types::{PrincipalId, UsageIntentId};
use std::sync::Arc;
use townhall_usage::{MemoryUsageStore, PricingSchedule, UsageDenied, UsagePolicy, UsageService};

const T0: u64 = 1_700_000_000_000;

fn service(limit_units: i64, ttl_ms: u64) -> UsageService<MemoryUsageStore> {
    UsageService::new(
        Arc::new(MemoryUsageStore::new()),
        UsagePolicy {
            default_limit_units: limit_units,
            units_per_turn: 1,
            reservation_ttl_ms: ttl_ms,
            pricing: PricingSchedule::default(),
        },
    )
}

fn lucy() -> PrincipalId {
    PrincipalId::new("lucy")
}

fn intent(tag: &str) -> UsageIntentId {
    UsageIntentId::new(format!("usage-{tag}"))
}

/// The gate's first clause: the same `UsageIntentId` meters at most once, even
/// across retries. A second debit for one intent charges nothing more.
#[tokio::test]
async fn the_same_intent_meters_at_most_once() {
    let service = service(10, 60_000);
    let (p, i) = (lucy(), intent("turn-1"));

    service.reserve(&p, &i, T0).await.expect("reserve");
    service.debit(&i, T0 + 1).await.expect("first debit");
    // A retry of the same turn — carrier redelivery, a crash-and-resume — settles
    // nothing more.
    service
        .debit(&i, T0 + 2)
        .await
        .expect("replayed debit is a no-op");

    let balance = service.balance(&p).await.expect("balance");
    assert_eq!(
        balance.debited_units, 1,
        "one intent, one unit — a second debit must not charge again"
    );
    assert_eq!(
        balance.reserved_units, 0,
        "the hold was settled, not left standing"
    );
}

/// The gate's second clause: an exhausted quota is refused BEFORE a metered step,
/// with a typed denial — and it does not partially charge.
#[tokio::test]
async fn an_exhausted_quota_blocks_the_next_reserve() {
    let service = service(1, 60_000);
    let (p, first, second) = (lucy(), intent("a"), intent("b"));

    service
        .reserve(&p, &first, T0)
        .await
        .expect("the first turn fits");
    assert_eq!(
        service.reserve(&p, &second, T0 + 1).await,
        Err(UsageDenied::QuotaExhausted),
        "the second turn is refused — the quota is spent"
    );

    let balance = service.balance(&p).await.expect("balance");
    assert_eq!(
        balance.reserved_units, 1,
        "only the first turn's hold stands"
    );
    assert_eq!(
        balance.debited_units, 0,
        "a refused reserve charges nothing"
    );
    assert_eq!(balance.remaining(), 0, "spent, and never negative");
}

/// Distinct intents meter separately — the dedupe is per intent, not per account.
#[tokio::test]
async fn distinct_intents_meter_separately() {
    let service = service(10, 60_000);
    let p = lucy();

    for tag in ["one", "two", "three"] {
        let i = intent(tag);
        service.reserve(&p, &i, T0).await.expect("reserve");
        service.debit(&i, T0 + 1).await.expect("debit");
    }

    assert_eq!(
        service.balance(&p).await.expect("balance").debited_units,
        3,
        "three distinct turns are three units — not collapsed to one"
    );
}

/// A released reservation returns its held units, and is not double-counted.
#[tokio::test]
async fn a_released_reservation_returns_its_quota() {
    let service = service(1, 60_000);
    let (p, first, second) = (lucy(), intent("a"), intent("b"));

    service.reserve(&p, &first, T0).await.expect("hold");
    service.release(&first, T0 + 1).await.expect("rescind");
    // The single unit is free again, so a fresh turn fits.
    service
        .reserve(&p, &second, T0 + 2)
        .await
        .expect("the released unit is available again");
    // A replayed release does not over-credit.
    service
        .release(&first, T0 + 3)
        .await
        .expect("idempotent release");

    let balance = service.balance(&p).await.expect("balance");
    assert_eq!(balance.reserved_units, 1, "only the second hold stands");
    assert_eq!(balance.debited_units, 0);
}

/// The stranded-reservation fix: a crash between reserve and debit leaves a live
/// hold, and the account's NEXT reserve reclaims it (deterministic expiry) rather
/// than locking the account out with units it never consumed.
#[tokio::test]
async fn a_stranded_reservation_is_reclaimed_by_the_next_reserve() {
    let ttl = 1_000;
    let service = service(1, ttl);
    let (p, crashed, next) = (lucy(), intent("crashed"), intent("next"));

    // A turn reserves, then the process dies before debit or release.
    service.reserve(&p, &crashed, T0).await.expect("hold");
    assert_eq!(service.balance(&p).await.unwrap().reserved_units, 1);

    // Past the TTL, the next reserve reclaims the stranded hold and fits.
    service
        .reserve(&p, &next, T0 + ttl + 1)
        .await
        .expect("the stranded unit is reclaimed, so the new turn fits");

    let balance = service.balance(&p).await.expect("balance");
    assert_eq!(
        balance.reserved_units, 1,
        "the reclaimed hold was released and only the new one stands — not two"
    );
}

/// Quota can never be driven below zero by metering: a debit after the ceiling is
/// reached does not create phantom credit.
#[tokio::test]
async fn quota_never_goes_negative() {
    let service = service(1, 60_000);
    let (p, i) = (lucy(), intent("only"));

    service.reserve(&p, &i, T0).await.expect("hold");
    service.debit(&i, T0 + 1).await.expect("settle");

    let balance = service.balance(&p).await.expect("balance");
    assert_eq!(balance.debited_units, 1);
    assert_eq!(balance.reserved_units, 0);
    assert_eq!(
        balance.remaining(),
        0,
        "at the ceiling, remaining is zero, not below"
    );
    // A further turn is refused rather than driving remaining negative.
    assert_eq!(
        service.reserve(&p, &intent("more"), T0 + 2).await,
        Err(UsageDenied::QuotaExhausted),
    );
}
