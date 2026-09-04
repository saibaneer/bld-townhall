//! `townhall-usage`'s port, over SQLite (ADR-027, migration 0009).
//!
//! The component crate defines the [`UsageStore`] contract and its in-memory
//! stand-in; this is the implementation that owns a pool. Every guard the meter
//! relies on is a conditional statement inside a transaction — the reserve
//! ceiling, the meter-once Debit, the reservation state transitions — so
//! atomicity is the database's, not the caller's.

use bld_types::{PrincipalId, UsageAccountId, UsageIntentId};
use sqlx::{Row, SqlitePool};
use townhall_usage::store::{Balance, StoreError, UsageStore};

/// `townhall-usage`'s ports, over SQLite.
#[derive(Clone, Debug)]
pub struct SqlUsageStore {
    pool: SqlitePool,
}

impl SqlUsageStore {
    #[must_use]
    pub const fn new(pool: SqlitePool) -> Self {
        Self { pool }
    }
}

#[async_trait::async_trait]
impl UsageStore for SqlUsageStore {
    async fn open_account(
        &self,
        account: &UsageAccountId,
        principal: &PrincipalId,
        limit_units: i64,
        now_ms: u64,
    ) -> Result<(), StoreError> {
        // Idempotent: `principal` is UNIQUE, so a second open for the same
        // principal keeps the existing row — its limit and totals are never reset.
        sqlx::query(
            r"
            INSERT INTO usage_accounts
                (id, principal, status, limit_units, reserved_units, debited_units,
                 created_at_ms, updated_at_ms)
            VALUES (?, ?, 'active', ?, 0, 0, ?, ?)
            ON CONFLICT(principal) DO NOTHING
            ",
        )
        .bind(account.as_str())
        .bind(principal.as_str())
        .bind(limit_units)
        .bind(as_i64(now_ms))
        .bind(as_i64(now_ms))
        .execute(&self.pool)
        .await
        .map_err(unavailable)?;
        Ok(())
    }

    async fn load_balance(&self, principal: &PrincipalId) -> Result<Option<Balance>, StoreError> {
        let row = sqlx::query(
            r"
            SELECT limit_units, reserved_units, debited_units
            FROM usage_accounts WHERE principal = ?
            ",
        )
        .bind(principal.as_str())
        .fetch_optional(&self.pool)
        .await
        .map_err(unavailable)?;

        let Some(row) = row else {
            return Ok(None);
        };
        Ok(Some(Balance {
            limit_units: column_i64(&row, "limit_units")?,
            reserved_units: column_i64(&row, "reserved_units")?,
            debited_units: column_i64(&row, "debited_units")?,
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
        let mut tx = self.pool.begin().await.map_err(unavailable)?;

        // (1) The FIRST statement is a write, so this transaction takes SQLite's
        // write lock immediately and cannot deadlock against a concurrent
        // reserve/debit that also read-then-writes. It LOGS a Release for each of
        // this account's stranded live reservations (naming its intent and units,
        // copied from the row before it is flipped) — the audit half of the
        // deterministic release policy, appended to the ledger like every other
        // event. The state flip follows.
        sqlx::query(
            r"
            INSERT INTO usage_ledger (account_id, kind, units, usage_intent_id, created_at_ms)
            SELECT account_id, 'Release', units, usage_intent_id, ?
            FROM usage_reservations
            WHERE account_id = (SELECT id FROM usage_accounts WHERE principal = ?)
              AND state = 'live' AND expires_at_ms < ?
            ",
        )
        .bind(as_i64(now_ms))
        .bind(principal.as_str())
        .bind(as_i64(now_ms))
        .execute(&mut *tx)
        .await
        .map_err(unavailable)?;
        // Now flip them to released — a crashed turn's held units are reclaimed
        // here, on the next reserve, rather than left to lock the account out.
        sqlx::query(
            r"
            UPDATE usage_reservations SET state = 'released'
            WHERE account_id = (SELECT id FROM usage_accounts WHERE principal = ?)
              AND state = 'live' AND expires_at_ms < ?
            ",
        )
        .bind(principal.as_str())
        .bind(as_i64(now_ms))
        .execute(&mut *tx)
        .await
        .map_err(unavailable)?;

        // Resolve the account — a read, now safely under the held write lock.
        let row = sqlx::query("SELECT id FROM usage_accounts WHERE principal = ?")
            .bind(principal.as_str())
            .fetch_optional(&mut *tx)
            .await
            .map_err(unavailable)?;
        let Some(row) = row else {
            tx.rollback().await.map_err(unavailable)?;
            return Err(StoreError::UnknownAccount);
        };
        let account_id = column_string(&row, "id")?;

        // Recompute reserved_units from the surviving live reservations, so the
        // guard below sees the post-expiry hold. reserved_units is a cache of
        // SUM(live), never an incrementally-drifting counter.
        recompute_reserved(&mut tx, &account_id, now_ms).await?;

        // Idempotent: a reservation for this intent already exists (a retry). The
        // hold is not taken twice.
        let existing = sqlx::query("SELECT 1 FROM usage_reservations WHERE usage_intent_id = ?")
            .bind(intent.as_str())
            .fetch_optional(&mut *tx)
            .await
            .map_err(unavailable)?;
        if existing.is_some() {
            tx.commit().await.map_err(unavailable)?;
            return Ok(());
        }

        // (2) The guard: insert the hold ONLY if it fits under the ceiling. The
        // conditional INSERT-SELECT re-reads the totals in one statement, so two
        // concurrent reserves cannot both pass one ceiling — `rows_affected() == 0`
        // IS the over-quota signal.
        let held = sqlx::query(
            r"
            INSERT INTO usage_reservations
                (usage_intent_id, account_id, units, state, expires_at_ms, created_at_ms)
            SELECT ?, a.id, ?, 'live', ?, ?
            FROM usage_accounts a
            WHERE a.principal = ? AND a.debited_units + a.reserved_units + ? <= a.limit_units
            ",
        )
        .bind(intent.as_str())
        .bind(units)
        .bind(as_i64(expires_at_ms))
        .bind(as_i64(now_ms))
        .bind(principal.as_str())
        .bind(units)
        .execute(&mut *tx)
        .await
        .map_err(unavailable)?
        .rows_affected();

        if held == 0 {
            tx.rollback().await.map_err(unavailable)?;
            return Err(StoreError::QuotaExhausted);
        }

        // Fold the new hold into the cache, and record the Reserve event.
        recompute_reserved(&mut tx, &account_id, now_ms).await?;
        insert_ledger(&mut tx, &account_id, "Reserve", units, Some(intent), now_ms).await?;

        tx.commit().await.map_err(unavailable)?;
        Ok(())
    }

    async fn debit(
        &self,
        intent: &UsageIntentId,
        actual_units: i64,
        now_ms: u64,
    ) -> Result<(), StoreError> {
        let mut tx = self.pool.begin().await.map_err(unavailable)?;

        // (1) The FIRST statement is the meter-once write — it takes the write lock
        // AND dedupes. The account_id and 'Debit' kind come from the reservation
        // via SELECT, so one INSERT both settles and asserts a reservation exists.
        // The unique partial index collapses a redelivered turn's second Debit to
        // a no-op: the account is charged exactly once, even across a restart.
        let metered = sqlx::query(
            r"
            INSERT INTO usage_ledger (account_id, kind, units, usage_intent_id, created_at_ms)
            SELECT account_id, 'Debit', ?, ?, ?
            FROM usage_reservations WHERE usage_intent_id = ?
            ON CONFLICT(usage_intent_id) WHERE kind = 'Debit' AND usage_intent_id IS NOT NULL
            DO NOTHING
            ",
        )
        .bind(actual_units)
        .bind(intent.as_str())
        .bind(as_i64(now_ms))
        .bind(intent.as_str())
        .execute(&mut *tx)
        .await
        .map_err(unavailable)?
        .rows_affected();

        if metered == 0 {
            // Already metered, or nothing was reserved — either way, no (further)
            // charge. Idempotent by outcome.
            tx.commit().await.map_err(unavailable)?;
            return Ok(());
        }

        // First debit: charge the actual, settle the reservation, and recompute
        // the reserved cache (the settled hold drops out of SUM(live)). Whether it
        // was still live or already expired-and-released is handled entirely by the
        // recompute — no branch needed.
        let account_id = {
            let row =
                sqlx::query("SELECT account_id FROM usage_reservations WHERE usage_intent_id = ?")
                    .bind(intent.as_str())
                    .fetch_one(&mut *tx)
                    .await
                    .map_err(unavailable)?;
            column_string(&row, "account_id")?
        };
        sqlx::query(
            "UPDATE usage_accounts SET debited_units = debited_units + ?, updated_at_ms = ? WHERE id = ?",
        )
        .bind(actual_units)
        .bind(as_i64(now_ms))
        .bind(&account_id)
        .execute(&mut *tx)
        .await
        .map_err(unavailable)?;
        // Only a still-live reservation transitions to settled; one already
        // expired-and-released stays `released` (terminal), so the state machine
        // holds. The charge itself is not conditional on this — the Debit row
        // above records it either way, which is the "charged after its hold
        // expired" case.
        sqlx::query(
            "UPDATE usage_reservations SET state = 'settled' WHERE usage_intent_id = ? AND state = 'live'",
        )
        .bind(intent.as_str())
        .execute(&mut *tx)
        .await
        .map_err(unavailable)?;
        recompute_reserved(&mut tx, &account_id, now_ms).await?;

        tx.commit().await.map_err(unavailable)?;
        Ok(())
    }

    async fn release(&self, intent: &UsageIntentId, now_ms: u64) -> Result<(), StoreError> {
        let mut tx = self.pool.begin().await.map_err(unavailable)?;

        // (1) The FIRST statement is the conditional release write — it takes the
        // lock and is the idempotency guard: a reservation already settled or
        // released matches no row, so a repeated release moves nothing.
        let released = sqlx::query(
            "UPDATE usage_reservations SET state = 'released' WHERE usage_intent_id = ? AND state = 'live'",
        )
        .bind(intent.as_str())
        .execute(&mut *tx)
        .await
        .map_err(unavailable)?
        .rows_affected();

        if released == 1 {
            let row = sqlx::query(
                "SELECT account_id, units FROM usage_reservations WHERE usage_intent_id = ?",
            )
            .bind(intent.as_str())
            .fetch_one(&mut *tx)
            .await
            .map_err(unavailable)?;
            let account_id = column_string(&row, "account_id")?;
            let held_units = column_i64(&row, "units")?;
            recompute_reserved(&mut tx, &account_id, now_ms).await?;
            insert_ledger(
                &mut tx,
                &account_id,
                "Release",
                held_units,
                Some(intent),
                now_ms,
            )
            .await?;
        }

        tx.commit().await.map_err(unavailable)?;
        Ok(())
    }
}

/// Recompute an account's `reserved_units` cache as the sum of its LIVE
/// reservations. Called after any statement that changes reservation liveness, so
/// the cache never drifts from the reservations that back it — and so a reserve
/// can lead with a write (the expiry) without pre-reading the units to reclaim.
async fn recompute_reserved(
    tx: &mut sqlx::Transaction<'_, sqlx::Sqlite>,
    account_id: &str,
    now_ms: u64,
) -> Result<(), StoreError> {
    sqlx::query(
        r"
        UPDATE usage_accounts SET
            reserved_units = (
                SELECT COALESCE(SUM(units), 0) FROM usage_reservations
                WHERE account_id = ? AND state = 'live'
            ),
            updated_at_ms = ?
        WHERE id = ?
        ",
    )
    .bind(account_id)
    .bind(as_i64(now_ms))
    .bind(account_id)
    .execute(&mut **tx)
    .await
    .map_err(unavailable)?;
    Ok(())
}

/// Append one event to the ledger — the audit trail. `entry_id` is the rowid,
/// assigned by SQLite.
async fn insert_ledger(
    tx: &mut sqlx::Transaction<'_, sqlx::Sqlite>,
    account_id: &str,
    kind: &str,
    units: i64,
    intent: Option<&UsageIntentId>,
    now_ms: u64,
) -> Result<(), StoreError> {
    sqlx::query(
        r"
        INSERT INTO usage_ledger (account_id, kind, units, usage_intent_id, created_at_ms)
        VALUES (?, ?, ?, ?, ?)
        ",
    )
    .bind(account_id)
    .bind(kind)
    .bind(units)
    .bind(intent.map(UsageIntentId::as_str))
    .bind(as_i64(now_ms))
    .execute(&mut **tx)
    .await
    .map_err(unavailable)?;
    Ok(())
}

fn as_i64(value: u64) -> i64 {
    i64::try_from(value).unwrap_or(i64::MAX)
}

fn column_i64(row: &sqlx::sqlite::SqliteRow, name: &str) -> Result<i64, StoreError> {
    row.try_get::<i64, _>(name).map_err(row_error)
}

fn column_string(row: &sqlx::sqlite::SqliteRow, name: &str) -> Result<String, StoreError> {
    row.try_get::<String, _>(name).map_err(row_error)
}

// Taken by value so they compose as `.map_err(unavailable)`, matching every call
// site; the sqlx error carries no borrow worth preserving past its message.
#[allow(clippy::needless_pass_by_value)]
fn unavailable(error: sqlx::Error) -> StoreError {
    StoreError::Unavailable(error.to_string())
}

#[allow(clippy::needless_pass_by_value)]
fn row_error(error: sqlx::Error) -> StoreError {
    StoreError::Unavailable(error.to_string())
}
