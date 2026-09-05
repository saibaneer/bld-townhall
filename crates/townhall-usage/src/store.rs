//! What the usage meter needs persisted, as a port someone else answers.
//!
//! The SQL implementation lives in `townhall-store` (which owns `sqlx`); this
//! crate must not name a pool (ADR-025). The in-memory implementation here
//! re-implements every guard under a single lock, so its atomicity is not a
//! lock-ordering question — the same discipline `MemoryApprovalStore` keeps.

use bld_types::{PrincipalId, UsageAccountId, UsageIntentId};
use std::collections::HashMap;
use std::sync::Mutex;

/// An account's folded state: the quota ceiling and the two totals the meter
/// moves. Remaining is derived, never stored, so it cannot drift from its parts.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Balance {
    pub limit_units: i64,
    pub reserved_units: i64,
    pub debited_units: i64,
}

impl Balance {
    /// Units still available to reserve. Saturating at zero: a momentary overrun
    /// (a debit that settled after its reservation expired and was re-lent) is
    /// reported as "none left", never as a negative that reads like credit.
    #[must_use]
    pub fn remaining(&self) -> i64 {
        (self.limit_units - self.debited_units - self.reserved_units).max(0)
    }
}

#[derive(Clone, Debug, thiserror::Error, PartialEq, Eq)]
pub enum StoreError {
    /// A reserve for a principal that has no account. Not a normal path — the
    /// service opens the account first — so it signals an invariant break.
    #[error("no usage account for that principal")]
    UnknownAccount,
    /// The reservation would take the account past its quota ceiling — the M8-1
    /// total-stock denial.
    #[error("usage quota exhausted")]
    QuotaExhausted,
    /// This principal has consumed its per-window turn allowance (M8-2 rate).
    #[error("principal rate limit exceeded")]
    PrincipalRateLimited,
    /// This channel has consumed its per-window turn allowance (M8-2 rate).
    #[error("channel rate limit exceeded")]
    ChannelRateLimited,
    /// The global per-window provider ceiling is spent — every principal's
    /// chargeable turn is refused until the window rolls (M8-2).
    #[error("provider budget exhausted")]
    ProviderBudgetExhausted,
    #[error("the usage store could not be reached: {0}")]
    Unavailable(String),
}

/// The three per-window ceilings a reserve is checked against (M8-2, ADR-028).
/// Passed as primitives so the store stays policy-free, exactly as `units` and
/// the TTL already are. A window is `now_ms - now_ms % window_ms`; a `max` at or
/// above any reachable per-window count is "effectively off".
#[derive(Clone, Copy, Debug)]
pub struct RateLimits {
    pub principal_max: i64,
    pub principal_window_ms: u64,
    pub channel_max: i64,
    pub channel_window_ms: u64,
    pub global_max: i64,
    pub global_window_ms: u64,
}

/// Everything the meter needs to persist.
///
/// `reserve`/`debit`/`release` are keyed by [`UsageIntentId`], which IS the
/// metered turn's identity (derived from the inbound message), so each is
/// idempotent on a retry: a re-sent turn recovers its reservation, settles once,
/// and releases once.
#[async_trait::async_trait]
pub trait UsageStore: Send + Sync {
    /// Open an account for `principal` with a quota ceiling, if none exists.
    /// Idempotent — an existing account keeps its limit and its totals; the
    /// offered id and limit are used only when the row is first created.
    ///
    /// # Errors
    /// The store is unreachable.
    async fn open_account(
        &self,
        account: &UsageAccountId,
        principal: &PrincipalId,
        limit_units: i64,
        now_ms: u64,
    ) -> Result<(), StoreError>;

    /// The account's balance, or `None` if the principal has no account.
    ///
    /// # Errors
    /// The store is unreachable.
    async fn load_balance(&self, principal: &PrincipalId) -> Result<Option<Balance>, StoreError>;

    /// Hold `units` for `intent` against `principal`'s account — the quota gate.
    ///
    /// First reclaims that account's own `live` reservations whose `expires_at_ms`
    /// has passed (the deterministic release policy, §16.2), so a crashed turn's
    /// units are returned by the next reserve rather than stranded. Then, in one
    /// transaction: the three per-window rate ceilings (principal, `channel`,
    /// global) — each a conditional counter upsert — and finally the quota guard.
    /// The hold commits ONLY if every guard passes; any failure rolls the whole
    /// transaction back, so a denied turn burns no rate token. Idempotent on
    /// `intent`: a retry recovers the existing reservation, touching no counter.
    ///
    /// # Errors
    /// [`StoreError::PrincipalRateLimited`] / [`StoreError::ChannelRateLimited`] /
    /// [`StoreError::ProviderBudgetExhausted`] if a per-window ceiling is hit;
    /// [`StoreError::QuotaExhausted`] if the total ceiling is; [`StoreError::UnknownAccount`]
    /// if the principal has none; or the store is unreachable. Checked in that
    /// order, so the FIRST ceiling hit names the denial.
    #[allow(clippy::too_many_arguments)] // the service passes each derived primitive once
    async fn reserve(
        &self,
        principal: &PrincipalId,
        intent: &UsageIntentId,
        channel: &str,
        units: i64,
        now_ms: u64,
        expires_at_ms: u64,
        limits: RateLimits,
    ) -> Result<(), StoreError>;

    /// Settle `intent`'s reservation at `actual_units` — the meter-once op. Draws
    /// the held units down and records the actual as debited. Idempotent: a second
    /// settle for the same intent is a no-op (the account is charged once).
    ///
    /// # Errors
    /// The store is unreachable.
    async fn debit(
        &self,
        intent: &UsageIntentId,
        actual_units: i64,
        now_ms: u64,
    ) -> Result<(), StoreError>;

    /// Rescind `intent`'s reservation — failure before consumption (§2). Returns
    /// the held units to the account. Idempotent: a second release, or a release
    /// of an already-settled reservation, is a no-op.
    ///
    /// # Errors
    /// The store is unreachable.
    async fn release(&self, intent: &UsageIntentId, now_ms: u64) -> Result<(), StoreError>;
}

/// The reservation state machine (§16.1): a hold is `Live`, then exactly one of
/// `Settled` (debited) or `Released` (rescinded or expired).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum ResState {
    Live,
    Settled,
    Released,
}

#[derive(Clone, Debug)]
struct Reservation {
    principal: String,
    units: i64,
    state: ResState,
    expires_at_ms: u64,
}

// The `_units` suffix is load-bearing, not noise: these are UNITS, never pence
// (§16's units-are-not-money separation), so the suffix stays and the lint is
// silenced rather than obeyed.
#[allow(clippy::struct_field_names)]
#[derive(Debug)]
struct Account {
    limit_units: i64,
    reserved_units: i64,
    debited_units: i64,
}

#[derive(Debug, Default)]
struct Held {
    /// Principal -> account.
    accounts: HashMap<String, Account>,
    /// Intent -> reservation.
    reservations: HashMap<String, Reservation>,
    /// Intents that have a settled `Debit` — the in-memory analogue of the unique
    /// `Debit` index, so meter-once holds even against a direct double-debit.
    debited_intents: std::collections::HashSet<String>,
    /// (`counter_key`, `window_start_ms`) -> `used_units` — the M8-2 windowed rate
    /// counters, the analogue of the `usage_rate_counters` table.
    rate_counters: HashMap<(String, u64), i64>,
}

/// The window a timestamp falls in: `now_ms - now_ms % window_ms`. `checked_rem`
/// makes a `window_ms` of 0 total (it yields `now_ms`, one all-of-time window)
/// rather than a panic — `UsagePolicy::validated` already refuses 0 for the
/// service, and this keeps a direct store caller safe too.
fn window_floor(now_ms: u64, window_ms: u64) -> u64 {
    now_ms - now_ms.checked_rem(window_ms).unwrap_or(0)
}

/// The in-memory store: this crate's tests and the composition roots' doubles.
///
/// One `Mutex` over the whole thing, deliberately — the reserve guard, the lazy
/// expiry and the settle all touch several maps, and nothing may interleave
/// between them.
#[derive(Debug, Default)]
pub struct MemoryUsageStore {
    held: Mutex<Held>,
}

impl MemoryUsageStore {
    #[must_use]
    pub fn new() -> Self {
        Self::default()
    }

    fn locked(&self) -> std::sync::MutexGuard<'_, Held> {
        self.held
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
    }

    /// Reclaim `principal`'s expired live reservations, drawing their held units
    /// back down. The deterministic release policy, run before every reserve.
    fn reclaim_expired(held: &mut Held, principal: &str, now_ms: u64) {
        let expired: Vec<String> = held
            .reservations
            .iter()
            .filter(|(_, r)| {
                r.principal == principal && r.state == ResState::Live && r.expires_at_ms < now_ms
            })
            .map(|(intent, _)| intent.clone())
            .collect();
        for intent in expired {
            if let Some(reservation) = held.reservations.get_mut(&intent) {
                reservation.state = ResState::Released;
                if let Some(account) = held.accounts.get_mut(principal) {
                    account.reserved_units -= reservation.units;
                }
            }
        }
    }
}

#[async_trait::async_trait]
impl UsageStore for MemoryUsageStore {
    async fn open_account(
        &self,
        _account: &UsageAccountId,
        principal: &PrincipalId,
        limit_units: i64,
        _now_ms: u64,
    ) -> Result<(), StoreError> {
        let mut held = self.locked();
        held.accounts
            .entry(principal.as_str().to_owned())
            .or_insert(Account {
                limit_units,
                reserved_units: 0,
                debited_units: 0,
            });
        Ok(())
    }

    async fn load_balance(&self, principal: &PrincipalId) -> Result<Option<Balance>, StoreError> {
        Ok(self
            .locked()
            .accounts
            .get(principal.as_str())
            .map(|a| Balance {
                limit_units: a.limit_units,
                reserved_units: a.reserved_units,
                debited_units: a.debited_units,
            }))
    }

    async fn reserve(
        &self,
        principal: &PrincipalId,
        intent: &UsageIntentId,
        channel: &str,
        units: i64,
        now_ms: u64,
        expires_at_ms: u64,
        limits: RateLimits,
    ) -> Result<(), StoreError> {
        let mut held = self.locked();
        Self::reclaim_expired(&mut held, principal.as_str(), now_ms);

        // Idempotent: a reservation already exists for this intent (a retry). The
        // hold is not taken twice, and no counter is touched.
        if held.reservations.contains_key(intent.as_str()) {
            return Ok(());
        }

        // The three per-window ceilings, in order (principal, channel, global) —
        // the FIRST hit names the denial. Checked all-then-incremented, so a later
        // guard (or the quota guard below) that fails leaves every counter
        // untouched: all-or-nothing, the memory stand-in for the SQL rollback.
        let checks = [
            (
                format!("principal:{}", principal.as_str()),
                limits.principal_window_ms,
                limits.principal_max,
                StoreError::PrincipalRateLimited,
            ),
            (
                format!("channel:{channel}"),
                limits.channel_window_ms,
                limits.channel_max,
                StoreError::ChannelRateLimited,
            ),
            (
                "global".to_owned(),
                limits.global_window_ms,
                limits.global_max,
                StoreError::ProviderBudgetExhausted,
            ),
        ];
        for (key, window_ms, max, denial) in &checks {
            let window = window_floor(now_ms, *window_ms);
            let used = held
                .rate_counters
                .get(&(key.clone(), window))
                .copied()
                .unwrap_or(0);
            if used + units > *max {
                return Err(denial.clone());
            }
        }

        let account = held
            .accounts
            .get_mut(principal.as_str())
            .ok_or(StoreError::UnknownAccount)?;
        // The quota guard: hold only if it fits under the TOTAL ceiling. This is
        // the memory stand-in for the conditional UPDATE — the check and the write
        // are one critical section under the single lock.
        if account.debited_units + account.reserved_units + units > account.limit_units {
            return Err(StoreError::QuotaExhausted);
        }
        account.reserved_units += units;
        // Every guard passed — now spend the rate tokens (all-or-nothing).
        for (key, window_ms, _max, _denial) in &checks {
            let window = window_floor(now_ms, *window_ms);
            *held.rate_counters.entry((key.clone(), window)).or_insert(0) += units;
        }
        held.reservations.insert(
            intent.as_str().to_owned(),
            Reservation {
                principal: principal.as_str().to_owned(),
                units,
                state: ResState::Live,
                expires_at_ms,
            },
        );
        Ok(())
    }

    async fn debit(
        &self,
        intent: &UsageIntentId,
        actual_units: i64,
        _now_ms: u64,
    ) -> Result<(), StoreError> {
        let mut held = self.locked();
        // Meter-once: a settled Debit for this intent already stands — charge
        // nothing more (the SQL unique-index analogue).
        if held.debited_intents.contains(intent.as_str()) {
            return Ok(());
        }
        let Some(reservation) = held.reservations.get(intent.as_str()).cloned() else {
            return Ok(()); // nothing reserved — defensive no-op
        };
        // Draw the held units down ONLY if the reservation is still live; if it
        // was already expired-and-released, its units were reclaimed at expiry.
        if reservation.state == ResState::Live {
            if let Some(account) = held.accounts.get_mut(&reservation.principal) {
                account.reserved_units -= reservation.units;
            }
        }
        if let Some(account) = held.accounts.get_mut(&reservation.principal) {
            account.debited_units += actual_units;
        }
        if let Some(row) = held.reservations.get_mut(intent.as_str()) {
            row.state = ResState::Settled;
        }
        held.debited_intents.insert(intent.as_str().to_owned());
        Ok(())
    }

    async fn release(&self, intent: &UsageIntentId, _now_ms: u64) -> Result<(), StoreError> {
        let mut held = self.locked();
        let Some(reservation) = held.reservations.get(intent.as_str()).cloned() else {
            return Ok(());
        };
        // Only a live hold has units to return; a settled or already-released one
        // is a no-op (idempotent).
        if reservation.state == ResState::Live {
            if let Some(account) = held.accounts.get_mut(&reservation.principal) {
                account.reserved_units -= reservation.units;
            }
            if let Some(row) = held.reservations.get_mut(intent.as_str()) {
                row.state = ResState::Released;
            }
        }
        Ok(())
    }
}
