//! The denial log: every "no" the boundary says, written down (ADR-017 point 2).
//!
//! In plain terms: when the system refuses something, that refusal must be
//! provable from the database later — "we said no, here's when, here's why, and
//! here's how many times." Without this, the project's central claim ("the
//! boundary refuses correctly") rests on nothing anyone can check.
//!
//! # Its own database file, deliberately
//!
//! Two designs died in review before this one. Writing denials into the main
//! database queues real work (bookings, effect commits) behind a flood of
//! refusal records — an attacker who spams cheap denials can make a legitimate
//! booking wait until its deadline passes. Buffering them in memory instead
//! hands the attacker control of *retention*: flood the buffer and the one
//! refusal that mattered is the one that gets dropped. A separate file with its
//! own writer removes the contention instead of budgeting it.
//!
//! # A flood of identical "no"s is one row and a counter
//!
//! Rows are deduplicated per (booking, door, input, reason, who, hour). Five
//! thousand identical refusals in one hour: one row, `occurrences = 5000`.
//! The same refusal next hour: a second row — so "4,000 times between 02:00 and
//! 03:00, twice in August" stays answerable. A *different* refusal is a
//! different row, always.
//!
//! # The answer is never held up
//!
//! The caller gets their typed refusal whether or not this write succeeds. A
//! lost denial record strands nothing — no state changed, nothing waits on it —
//! so a failed write here is logged and dropped (ADR-017 says so in as many
//! words). `Undefined` refusals — "that button doesn't exist here", forgeable
//! from pure garbage — are only counted in memory, never rowed: a durable row
//! per garbage request is a disk-filling attack.

use crate::{StoreClock, StoreError};
use bld_types::Provenance;
use sqlx::{
    Row as _, SqlitePool,
    sqlite::{SqliteConnectOptions, SqliteJournalMode, SqlitePoolOptions},
};
use std::{
    collections::HashMap,
    path::Path,
    sync::{Arc, Mutex},
    time::Duration,
};

/// One hour. A row per (refusal, hour), so floods compress and history stays.
const WINDOW_MS: i64 = 60 * 60 * 1000;

/// One recorded refusal, ready to write.
#[derive(Clone, Debug)]
pub struct Denial {
    pub booking_id: String,
    /// Which door said no: proposal, fact, or system event.
    pub driver_kind: Provenance,
    /// What was being asked: `Book`, `BookingExists`, `ReconciliationExhausted`.
    pub driver_detail: &'static str,
    /// The refusal's stable name — never its display text, which interpolates
    /// data and would split identical refusals into distinct rows.
    pub reason: &'static str,
    /// Who was refused, where anyone knows. The empty string means **explicitly
    /// unattributed** — "someone was refused and no principal is recoverable" —
    /// which is different from unknown-but-derivable, and the amended ADR-017
    /// says so. (Not NULL: `SQLite` treats NULLs as distinct in `UNIQUE` indexes,
    /// and every unattributed denial would get its own row — a dedup key that
    /// silently stops deduplicating.)
    pub principal: String,
}

/// A row as a test or an operator reads it back.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct DenialRow {
    pub booking_id: String,
    pub driver_kind: String,
    pub driver_detail: String,
    pub reason: String,
    pub principal: String,
    pub window_start_ms: i64,
    pub occurrences: i64,
}

#[derive(Debug)]
pub struct DenialLog {
    pool: SqlitePool,
    clock: Arc<dyn StoreClock>,
    /// `Undefined` counts, in memory, crash-lossy by design (ADR-017): keyed by
    /// (state, input). Constructible from garbage, so never worth a row.
    undefined: Mutex<HashMap<(String, String), u64>>,
}

impl DenialLog {
    /// Open (creating if absent) the denial log at `path`.
    ///
    /// # Errors
    /// [`StoreError::Sqlx`] if the file cannot be opened.
    pub async fn open(
        path: impl AsRef<Path>,
        clock: Arc<dyn StoreClock>,
    ) -> Result<Self, StoreError> {
        let options = SqliteConnectOptions::new()
            .filename(path)
            .create_if_missing(true)
            .journal_mode(SqliteJournalMode::Wal)
            .busy_timeout(Duration::from_secs(5));
        let pool = SqlitePoolOptions::new()
            .max_connections(2)
            .connect_with(options)
            .await?;

        sqlx::query(
            r"
            CREATE TABLE IF NOT EXISTS denials (
                booking_id      TEXT    NOT NULL,
                driver_kind     TEXT    NOT NULL,
                driver_detail   TEXT    NOT NULL,
                reason          TEXT    NOT NULL,
                principal       TEXT    NOT NULL,
                window_start_ms INTEGER NOT NULL,
                occurrences     INTEGER NOT NULL,
                first_seen_ms   INTEGER NOT NULL,
                last_seen_ms    INTEGER NOT NULL,
                UNIQUE (booking_id, driver_kind, driver_detail, reason, principal,
                        window_start_ms)
            )
            ",
        )
        .execute(&pool)
        .await?;

        Ok(Self {
            pool,
            clock,
            undefined: Mutex::new(HashMap::new()),
        })
    }

    /// Write one refusal down. Best-effort: the boundary's answer must never
    /// depend on this landing, so a failure is logged and dropped here — the
    /// row is an audit convenience the moment it fails, and a hard error the
    /// moment it would block a caller (ADR-017, in as many words).
    pub async fn record_denied(&self, denial: Denial) {
        let now = self.clock.now_ms();
        let window = now - now.rem_euclid(WINDOW_MS);
        let written = sqlx::query(
            r"
            INSERT INTO denials (booking_id, driver_kind, driver_detail, reason,
                                 principal, window_start_ms, occurrences,
                                 first_seen_ms, last_seen_ms)
            VALUES (?, ?, ?, ?, ?, ?, 1, ?, ?)
            ON CONFLICT (booking_id, driver_kind, driver_detail, reason, principal,
                         window_start_ms)
            DO UPDATE SET occurrences = occurrences + 1, last_seen_ms = excluded.last_seen_ms
            ",
        )
        .bind(&denial.booking_id)
        .bind(denial.driver_kind.name())
        .bind(denial.driver_detail)
        .bind(denial.reason)
        .bind(&denial.principal)
        .bind(window)
        .bind(now)
        .bind(now)
        .execute(&self.pool)
        .await;
        if let Err(error) = written {
            // "Logged and dropped" means BOTH halves: the caller is never made
            // to wait, and the loss is not silent.
            eprintln!(
                "denial record dropped (ADR-017: the answer never waits on this): \
                 booking={} reason={} error={error}",
                denial.booking_id, denial.reason
            );
        }
    }

    /// Count an `Undefined` — "that behaviour doesn't exist here". Memory only.
    pub fn note_undefined(&self, state: &str, input: &str) {
        let mut counts = self
            .undefined
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        *counts
            .entry((state.to_owned(), input.to_owned()))
            .or_insert(0) += 1;
    }

    /// How many times a nonexistent behaviour was asked for. For tests and
    /// operators; resets on restart, by design.
    #[must_use]
    pub fn undefined_count(&self, state: &str, input: &str) -> u64 {
        self.undefined
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .get(&(state.to_owned(), input.to_owned()))
            .copied()
            .unwrap_or(0)
    }

    /// Every durable denial row, oldest window first. For tests and operators.
    ///
    /// # Errors
    /// [`StoreError::Sqlx`] on a read failure.
    pub async fn rows(&self) -> Result<Vec<DenialRow>, StoreError> {
        let rows = sqlx::query(
            r"
            SELECT booking_id, driver_kind, driver_detail, reason, principal,
                   window_start_ms, occurrences
              FROM denials
             ORDER BY window_start_ms ASC, booking_id ASC, reason ASC
            ",
        )
        .fetch_all(&self.pool)
        .await?;

        Ok(rows
            .iter()
            .map(|row| DenialRow {
                booking_id: row.get("booking_id"),
                driver_kind: row.get("driver_kind"),
                driver_detail: row.get("driver_detail"),
                reason: row.get("reason"),
                principal: row.get("principal"),
                window_start_ms: row.get("window_start_ms"),
                occurrences: row.get("occurrences"),
            })
            .collect())
    }
}
