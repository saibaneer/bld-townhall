//! The meter's small service layer: the policy (how many units a turn costs, the
//! quota a new account gets, how long a reservation lives) and the four
//! primitives the HTTP surface calls — reserve, debit, release, balance.
//!
//! It names concrete UNITS and £0 prices; it never names a booking, a grant or a
//! socket. A successful reserve or debit returns nothing an authority check reads
//! (§16: metering grants no authority).

use crate::store::{Balance, RateLimits, StoreError, UsageStore};
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
    // M8-2 (ADR-028) — the three per-window rate ceilings. A `*_max` at or above
    // any reachable per-window count is "effectively off"; the generous defaults
    // are exactly that, so metering behaves as M8-1 did until a deployment tightens
    // them. Windows default long so a per-window ceiling reads as a de-facto cap
    // for a demo run.
    pub principal_rate_max: i64,
    pub principal_rate_window_ms: u64,
    pub channel_rate_max: i64,
    pub channel_rate_window_ms: u64,
    pub global_budget_max: i64,
    pub global_budget_window_ms: u64,
}

impl Default for UsagePolicy {
    fn default() -> Self {
        let hour = 60 * 60 * 1_000;
        Self {
            // Generous for the demo — a real deployment would size these per plan.
            default_limit_units: 1_000,
            units_per_turn: 1,
            reservation_ttl_ms: 10 * 60 * 1_000,
            pricing: PricingSchedule::default(),
            principal_rate_max: 10_000,
            principal_rate_window_ms: hour,
            channel_rate_max: 100_000,
            channel_rate_window_ms: hour,
            global_budget_max: 1_000_000,
            global_budget_window_ms: hour,
        }
    }
}

impl UsagePolicy {
    /// The rate ceilings, bundled for the store — the M8-2 guard inputs.
    #[must_use]
    pub const fn rate_limits(&self) -> RateLimits {
        RateLimits {
            principal_max: self.principal_rate_max,
            principal_window_ms: self.principal_rate_window_ms,
            channel_max: self.channel_rate_max,
            channel_window_ms: self.channel_rate_window_ms,
            global_max: self.global_budget_max,
            global_window_ms: self.global_budget_window_ms,
        }
    }

    /// Reject a policy that cannot meter. Each ceiling must admit at least one
    /// turn per window (`max >= units_per_turn` — a lower max would deny every
    /// turn forever, and it is also what keeps the counter's fresh-INSERT path
    /// safe), and each window must be positive (the window floor divides by it).
    ///
    /// # Errors
    /// A one-line description of the first violated invariant.
    pub fn validated(self) -> Result<Self, String> {
        let per_turn = self.units_per_turn;
        for (name, max, window) in [
            (
                "principal rate",
                self.principal_rate_max,
                self.principal_rate_window_ms,
            ),
            (
                "channel rate",
                self.channel_rate_max,
                self.channel_rate_window_ms,
            ),
            (
                "global budget",
                self.global_budget_max,
                self.global_budget_window_ms,
            ),
        ] {
            if max < per_turn {
                return Err(format!(
                    "{name} max ({max}) must be at least units_per_turn ({per_turn})"
                ));
            }
            if window == 0 {
                return Err(format!("{name} window must be positive"));
            }
        }
        Ok(self)
    }
}

/// Why a metering call was refused. Each resource denial is distinct so the gate
/// can prove it in isolation, even though all map to HTTP 429 (ADR-028).
#[derive(Debug, thiserror::Error, PartialEq, Eq)]
pub enum UsageDenied {
    /// The account is out of total quota (M8-1's stock ceiling).
    #[error("usage quota exhausted")]
    QuotaExhausted,
    /// This principal hit its per-window turn allowance (M8-2 rate).
    #[error("principal rate limit exceeded")]
    PrincipalRateLimited,
    /// This channel hit its per-window turn allowance (M8-2 rate).
    #[error("channel rate limit exceeded")]
    ChannelRateLimited,
    /// The global per-window provider ceiling is spent (M8-2).
    #[error("provider budget exhausted")]
    ProviderBudgetExhausted,
    /// The store could not be reached — a 503, never read as "quota spent".
    #[error("the usage store could not be reached: {0}")]
    Unavailable(String),
}

impl UsageDenied {
    /// The stable audit/wire code (ADR-028): all resource denials are 429, so this
    /// is how a client tells them apart. Aligned with `AuditEvent.denial_code`.
    #[must_use]
    pub const fn denial_code(&self) -> &'static str {
        match self {
            Self::QuotaExhausted => "quota_exhausted",
            Self::PrincipalRateLimited => "rate_limited_principal",
            Self::ChannelRateLimited => "rate_limited_channel",
            Self::ProviderBudgetExhausted => "provider_budget_exhausted",
            Self::Unavailable(_) => "unavailable",
        }
    }
}

impl From<StoreError> for UsageDenied {
    fn from(error: StoreError) -> Self {
        match error {
            StoreError::QuotaExhausted => Self::QuotaExhausted,
            StoreError::PrincipalRateLimited => Self::PrincipalRateLimited,
            StoreError::ChannelRateLimited => Self::ChannelRateLimited,
            StoreError::ProviderBudgetExhausted => Self::ProviderBudgetExhausted,
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
        channel: &str,
        now_ms: u64,
    ) -> Result<(), UsageDenied> {
        self.ensure_account(principal, now_ms).await?;
        self.store
            .reserve(
                principal,
                intent,
                channel,
                self.policy.units_per_turn,
                now_ms,
                now_ms.saturating_add(self.policy.reservation_ttl_ms),
                self.policy.rate_limits(),
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
