//! The meter's semantics, against the in-memory store: meter-once, the quota
//! gate, the reserve/debit/release lifecycle, and the deterministic expiry that
//! keeps a crashed turn from stranding quota. Each test fails a specific wrong
//! implementation (never-fake-tests).

use bld_types::{PrincipalId, UsageIntentId};
use std::sync::Arc;
use townhall_usage::{MemoryUsageStore, UsageDenied, UsagePolicy, UsageService};

const T0: u64 = 1_700_000_000_000;
/// A channel key for the quota/expiry tests, whose rate ceilings are the generous
/// defaults — so only the quota under test can refuse.
const CH: &str = "sim|townhall";

fn service(limit_units: i64, ttl_ms: u64) -> UsageService<MemoryUsageStore> {
    UsageService::new(
        Arc::new(MemoryUsageStore::new()),
        UsagePolicy {
            default_limit_units: limit_units,
            units_per_turn: 1,
            reservation_ttl_ms: ttl_ms,
            // The rate ceilings stay at their generous defaults for these tests.
            ..UsagePolicy::default()
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

    service.reserve(&p, &i, CH, T0).await.expect("reserve");
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
        .reserve(&p, &first, CH, T0)
        .await
        .expect("the first turn fits");
    assert_eq!(
        service.reserve(&p, &second, CH, T0 + 1).await,
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
        service.reserve(&p, &i, CH, T0).await.expect("reserve");
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

    service.reserve(&p, &first, CH, T0).await.expect("hold");
    service.release(&first, T0 + 1).await.expect("rescind");
    // The single unit is free again, so a fresh turn fits.
    service
        .reserve(&p, &second, CH, T0 + 2)
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
    service.reserve(&p, &crashed, CH, T0).await.expect("hold");
    assert_eq!(service.balance(&p).await.unwrap().reserved_units, 1);

    // Past the TTL, the next reserve reclaims the stranded hold and fits.
    service
        .reserve(&p, &next, CH, T0 + ttl + 1)
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

    service.reserve(&p, &i, CH, T0).await.expect("hold");
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
        service.reserve(&p, &intent("more"), CH, T0 + 2).await,
        Err(UsageDenied::QuotaExhausted),
    );
}

// ---------------------------------------------------------------- M8-2 (ADR-028)

/// A policy with a tight PRINCIPAL rate and everything else generous, so only the
/// per-principal window can refuse.
fn principal_rate(max: i64, window_ms: u64) -> UsageService<MemoryUsageStore> {
    UsageService::new(
        Arc::new(MemoryUsageStore::new()),
        UsagePolicy {
            default_limit_units: 1_000_000,
            principal_rate_max: max,
            principal_rate_window_ms: window_ms,
            ..UsagePolicy::default()
        },
    )
}

/// The rate gate: N turns per window are allowed, the (N+1)th in the SAME window
/// is refused as a rate limit (not the quota), and a turn in the NEXT window
/// recovers. Distinct intents throughout — a reused intent hits the idempotent
/// early-return, not the window.
#[tokio::test]
async fn a_principal_rate_refuses_the_next_turn_then_recovers_next_window() {
    let window = 1_000;
    let service = principal_rate(2, window);
    let p = lucy();

    service
        .reserve(&p, &intent("a"), CH, T0)
        .await
        .expect("first fits");
    service
        .reserve(&p, &intent("b"), CH, T0 + 1)
        .await
        .expect("second fits");
    // The third in the same window is refused — the rate, not the quota.
    assert_eq!(
        service.reserve(&p, &intent("c"), CH, T0 + 2).await,
        Err(UsageDenied::PrincipalRateLimited),
        "the window's allowance is spent"
    );
    // The window rolls; a turn in the next one is allowed again.
    service
        .reserve(&p, &intent("d"), CH, T0 + window)
        .await
        .expect("a new window is a fresh allowance");
}

/// The channel rate is its own ceiling: two principals sharing a channel share
/// the channel window, even though each has personal headroom.
#[tokio::test]
async fn a_channel_rate_bounds_a_shared_channel_across_principals() {
    let service = UsageService::new(
        Arc::new(MemoryUsageStore::new()),
        UsagePolicy {
            default_limit_units: 1_000_000,
            channel_rate_max: 1,
            channel_rate_window_ms: 1_000,
            ..UsagePolicy::default()
        },
    );
    let (lucy, priya) = (PrincipalId::new("lucy"), PrincipalId::new("priya"));

    // Lucy takes the channel's one turn this window.
    service
        .reserve(&lucy, &intent("l"), CH, T0)
        .await
        .expect("lucy fits");
    // Priya, on the SAME channel, is refused despite her own untouched quota.
    assert_eq!(
        service.reserve(&priya, &intent("p"), CH, T0 + 1).await,
        Err(UsageDenied::ChannelRateLimited),
        "the channel window is shared, and spent"
    );
}

/// The global budget refuses EVERY principal's turn once the shared window is
/// spent, regardless of personal quota or rate — and across DIFFERENT channels.
#[tokio::test]
async fn the_global_budget_refuses_across_principals_and_channels() {
    let service = UsageService::new(
        Arc::new(MemoryUsageStore::new()),
        UsagePolicy {
            default_limit_units: 1_000_000,
            global_budget_max: 1,
            global_budget_window_ms: 1_000,
            ..UsagePolicy::default()
        },
    );
    let (lucy, priya) = (PrincipalId::new("lucy"), PrincipalId::new("priya"));

    service
        .reserve(&lucy, &intent("l"), "sim|acct-a", T0)
        .await
        .expect("first fits");
    // Priya, a different principal on a different channel, is still refused: the
    // ceiling is global.
    assert_eq!(
        service
            .reserve(&priya, &intent("p"), "sim|acct-b", T0 + 1)
            .await,
        Err(UsageDenied::ProviderBudgetExhausted),
        "the global window is spent for everyone"
    );
    // Next window: recovered.
    service
        .reserve(&priya, &intent("p2"), "sim|acct-b", T0 + 1_000)
        .await
        .expect("a new global window");
}

/// The three resource denials are DISTINCT — each tight in exactly one dimension
/// yields its own variant, so the gate can prove each alone. Their `denial_code`s
/// are pairwise distinct too (the wire discriminator).
#[tokio::test]
async fn the_resource_denials_are_distinct() {
    // Principal rate = 1, one turn taken → next is PrincipalRateLimited.
    let s = principal_rate(1, 1_000);
    let p = lucy();
    s.reserve(&p, &intent("a"), CH, T0).await.unwrap();
    let principal = s.reserve(&p, &intent("b"), CH, T0).await.unwrap_err();

    // Quota = 1, one held → next is QuotaExhausted.
    let s = service(1, 60_000);
    s.reserve(&p, &intent("a"), CH, T0).await.unwrap();
    let quota = s.reserve(&p, &intent("b"), CH, T0).await.unwrap_err();

    // Global = 1, one taken → next is ProviderBudgetExhausted.
    let s = UsageService::new(
        Arc::new(MemoryUsageStore::new()),
        UsagePolicy {
            default_limit_units: 1_000_000,
            global_budget_max: 1,
            global_budget_window_ms: 1_000,
            ..UsagePolicy::default()
        },
    );
    s.reserve(&p, &intent("a"), CH, T0).await.unwrap();
    let global = s.reserve(&p, &intent("b"), CH, T0).await.unwrap_err();

    assert_eq!(principal, UsageDenied::PrincipalRateLimited);
    assert_eq!(quota, UsageDenied::QuotaExhausted);
    assert_eq!(global, UsageDenied::ProviderBudgetExhausted);
    // Pairwise-distinct wire codes.
    let (pc, qc, gc) = (
        principal.denial_code(),
        quota.denial_code(),
        global.denial_code(),
    );
    assert!(
        pc != qc && qc != gc && pc != gc,
        "the three denials carry distinct wire codes: {pc}, {qc}, {gc}"
    );
}
