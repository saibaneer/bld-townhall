//! The meter's small service layer: the policy (how many units a turn costs, the
//! quota a new account gets, how long a reservation lives) and the four
//! primitives the HTTP surface calls — reserve, debit, release, balance.
//!
//! It names concrete UNITS and £0 prices; it never names a booking, a grant or a
//! socket. A successful reserve or debit returns nothing an authority check reads
//! (§16: metering grants no authority).

use crate::store::{Balance, StoreError, UsageStore};
use bld_types::{PrincipalId, UsageAccountId, UsageIntentId};
use std::sync::Arc;

/// The versioned, deterministic pricing schedule (§16.2). For the POC every unit
/// is priced at £0 — the schedule exists so the units≠money separation is
/// explicit and a real price can drop in later without touching the meter.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct PricingSchedule {
    pub version: u32,
}

impl PricingSchedule {
    /// The monetary price of `units`, in pence. Zero, at every version, in the
    /// POC — a unit bounds resource consumption, it does not bill.
    #[must_use]
    pub const fn price_pence(&self, _units: i64) -> u64 {
        0
    }
}

impl Default for PricingSchedule {
    fn default() -> Self {
        Self { version: 1 }
    }
}

/// How the meter is configured.
#[derive(Clone, Copy, Debug)]
pub struct UsagePolicy {
    /// The quota a freshly-opened account is given, in units.
    pub default_limit_units: i64,
    /// Units a single proposer turn reserves and debits. One, in the POC.
    pub units_per_turn: i64,
    /// How long a reservation lives before the next reserve may reclaim it — the
    /// deterministic release policy's TTL. Generous relative to a turn, so a
    /// real turn never expires mid-flight; it exists for the crashed turn.
    pub reservation_ttl_ms: u64,
    pub pricing: PricingSchedule,
}

impl Default for UsagePolicy {
    fn default() -> Self {
        Self {
            // Generous for the demo — a real deployment would size this per plan.
            default_limit_units: 1_000,
            units_per_turn: 1,
            reservation_ttl_ms: 10 * 60 * 1_000,
            pricing: PricingSchedule::default(),
        }
    }
}

/// Why a metering call was refused.
#[derive(Debug, thiserror::Error, PartialEq, Eq)]
pub enum UsageDenied {
    /// The account is out of quota. The typed resource denial the gate surfaces
    /// before a metered step — an HTTP 429.
    #[error("usage quota exhausted")]
    QuotaExhausted,
    /// The store could not be reached — a 503, never read as "quota spent".
    #[error("the usage store could not be reached: {0}")]
    Unavailable(String),
}

impl From<StoreError> for UsageDenied {
    fn from(error: StoreError) -> Self {
        match error {
            StoreError::QuotaExhausted => Self::QuotaExhausted,
            // The service opens the account before every reserve, so a missing
            // account here is an invariant break, not a user error.
            StoreError::UnknownAccount => Self::Unavailable("no usage account".to_owned()),
            StoreError::Unavailable(why) => Self::Unavailable(why),
        }
    }
}

/// The meter, over some [`UsageStore`].
pub struct UsageService<S> {
    store: Arc<S>,
    policy: UsagePolicy,
}

impl<S: UsageStore> UsageService<S> {
    pub fn new(store: Arc<S>, policy: UsagePolicy) -> Self {
        Self { store, policy }
    }

    /// One account per principal, its id derived from the principal — no entropy
    /// needed, because the id names a row and grants nothing, and `open_account`
    /// is idempotent on the principal.
    fn account_id(principal: &PrincipalId) -> UsageAccountId {
        UsageAccountId::new(format!("usage-{}", principal.as_str()))
    }

    async fn ensure_account(
        &self,
        principal: &PrincipalId,
        now_ms: u64,
    ) -> Result<(), UsageDenied> {
        self.store
            .open_account(
                &Self::account_id(principal),
                principal,
                self.policy.default_limit_units,
                now_ms,
            )
            .await?;
        Ok(())
    }

    /// Reserve a turn's units — the quota gate. Opens the account on first use.
    ///
    /// # Errors
    /// [`UsageDenied::QuotaExhausted`] before any metered step, or the store is
    /// unreachable.
    pub async fn reserve(
        &self,
        principal: &PrincipalId,
        intent: &UsageIntentId,
        now_ms: u64,
    ) -> Result<(), UsageDenied> {
        self.ensure_account(principal, now_ms).await?;
        self.store
            .reserve(
                principal,
                intent,
                self.policy.units_per_turn,
                now_ms,
                now_ms.saturating_add(self.policy.reservation_ttl_ms),
            )
            .await?;
        Ok(())
    }

    /// Settle a turn — the meter-once op. Idempotent on the intent.
    ///
    /// # Errors
    /// The store is unreachable.
    pub async fn debit(&self, intent: &UsageIntentId, now_ms: u64) -> Result<(), UsageDenied> {
        self.store
            .debit(intent, self.policy.units_per_turn, now_ms)
            .await?;
        Ok(())
    }

    /// Rescind a turn's reservation — failure before consumption. Idempotent.
    ///
    /// # Errors
    /// The store is unreachable.
    pub async fn release(&self, intent: &UsageIntentId, now_ms: u64) -> Result<(), UsageDenied> {
        self.store.release(intent, now_ms).await?;
        Ok(())
    }

    /// The account's balance — a zero-unit read. A principal with no account yet
    /// reports the default allowance unused, so BALANCE never has to open one.
    ///
    /// # Errors
    /// The store is unreachable.
    pub async fn balance(&self, principal: &PrincipalId) -> Result<Balance, UsageDenied> {
        Ok(self
            .store
            .load_balance(principal)
            .await?
            .unwrap_or(Balance {
                limit_units: self.policy.default_limit_units,
                reserved_units: 0,
                debited_units: 0,
            }))
    }

    #[must_use]
    pub const fn policy(&self) -> &UsagePolicy {
        &self.policy
    }
}
