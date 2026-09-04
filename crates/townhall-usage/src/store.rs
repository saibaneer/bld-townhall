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

#[derive(Debug, thiserror::Error)]
pub enum StoreError {
    /// A reserve for a principal that has no account. Not a normal path — the
    /// service opens the account first — so it signals an invariant break.
    #[error("no usage account for that principal")]
    UnknownAccount,
    /// The reservation would take the account past its quota ceiling. The typed
    /// resource denial the gate turns on, surfaced before any metered step.
    #[error("usage quota exhausted")]
    QuotaExhausted,
    #[error("the usage store could not be reached: {0}")]
    Unavailable(String),
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
    /// units are returned by the next reserve rather than stranded. Then the guard
    /// commits the hold ONLY if it fits under the ceiling. Idempotent on `intent`:
    /// a retry recovers the existing reservation rather than holding twice.
    ///
    /// # Errors
    /// [`StoreError::QuotaExhausted`] if the hold would exceed the ceiling,
    /// [`StoreError::UnknownAccount`] if the principal has none, or the store is
    /// unreachable.
    async fn reserve(
        &self,
        principal: &PrincipalId,
        intent: &UsageIntentId,
        units: i64,
        now_ms: u64,
        expires_at_ms: u64,
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
        units: i64,
        now_ms: u64,
        expires_at_ms: u64,
    ) -> Result<(), StoreError> {
        let mut held = self.locked();
        Self::reclaim_expired(&mut held, principal.as_str(), now_ms);

        // Idempotent: a reservation already exists for this intent (a retry). The
        // hold is not taken twice; the eventual debit settles it once.
        if held.reservations.contains_key(intent.as_str()) {
            return Ok(());
        }

        let account = held
            .accounts
            .get_mut(principal.as_str())
            .ok_or(StoreError::UnknownAccount)?;
        // The guard: hold only if it fits under the ceiling. This is the memory
        // stand-in for the conditional UPDATE — the check and the write are one
        // critical section under the single lock.
        if account.debited_units + account.reserved_units + units > account.limit_units {
            return Err(StoreError::QuotaExhausted);
        }
        account.reserved_units += units;
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
