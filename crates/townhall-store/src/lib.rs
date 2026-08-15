#![forbid(unsafe_code)]

use async_trait::async_trait;
use bld_types::{BookingId, BookingRequirements, CouncilBookingRef, EffectIntentId};
use sqlx::{
    Row, SqlitePool,
    migrate::Migrator,
    sqlite::{SqliteConnectOptions, SqliteJournalMode, SqlitePoolOptions},
};
use std::{
    path::Path,
    time::{Duration, SystemTime, UNIX_EPOCH},
};
use thiserror::Error;
use townhall_domain::{BookingAggregate, BookingState, Draft, SelectedVenueRef, VenueFacts};

static MIGRATOR: Migrator = sqlx::migrate!("./migrations");

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct NewBooking {
    pub id: BookingId,
    pub requirements: BookingRequirements,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct BookingWrite {
    pub state: BookingState,
    pub requirements: BookingRequirements,
    pub selected_venue: Option<SelectedVenueRef>,
    pub availability: Option<VenueFacts>,
    pub booking_ref: Option<CouncilBookingRef>,
    pub active_effect: Option<EffectIntentId>,
}

impl From<&BookingAggregate> for BookingWrite {
    fn from(value: &BookingAggregate) -> Self {
        Self {
            state: value.state.clone(),
            requirements: value.requirements.clone(),
            selected_venue: value.selected_venue.clone(),
            availability: value.availability.clone(),
            booking_ref: value.booking_ref.clone(),
            active_effect: value.active_effect.clone(),
        }
    }
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct TransitionAudit {
    pub proposal: String,
    pub outcome: String,
    pub evidence_summary: Option<String>,
}

impl TransitionAudit {
    #[must_use]
    pub fn committed(proposal: impl Into<String>, evidence_summary: Option<String>) -> Self {
        Self {
            proposal: proposal.into(),
            outcome: "Committed".to_owned(),
            evidence_summary,
        }
    }
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct AuditEvent {
    pub sequence: i64,
    pub event_id: String,
    pub booking_id: BookingId,
    pub from_version: u64,
    pub to_version: u64,
    pub from_state: String,
    pub to_state: String,
    pub proposal: String,
    pub outcome: String,
    pub evidence_summary: Option<String>,
    pub created_at_ms: i64,
}

#[derive(Debug, Error)]
pub enum StoreError {
    #[error("booking {0} was not found")]
    NotFound(BookingId),
    #[error("booking {0} already exists")]
    AlreadyExists(BookingId),
    #[error("stale booking version: expected {expected}, actual {actual}")]
    StaleVersion { expected: u64, actual: u64 },
    #[error("version value is outside SQLite INTEGER range")]
    VersionOutOfRange,
    #[error("system clock is outside supported range")]
    ClockOutOfRange,
    #[error("persisted booking row is corrupt: {0}")]
    CorruptRow(String),
    #[error(transparent)]
    Sqlx(#[from] sqlx::Error),
    #[error(transparent)]
    Migration(#[from] sqlx::migrate::MigrateError),
    #[error(transparent)]
    Json(#[from] serde_json::Error),
}

#[async_trait]
pub trait BookingRepository: Send + Sync {
    async fn create(&self, booking: NewBooking) -> Result<BookingAggregate, StoreError>;

    async fn load(&self, id: &BookingId) -> Result<BookingAggregate, StoreError>;

    async fn commit(
        &self,
        id: &BookingId,
        expected_version: u64,
        next: BookingWrite,
        audit: TransitionAudit,
    ) -> Result<BookingAggregate, StoreError>;

    async fn audit_events(&self, id: &BookingId) -> Result<Vec<AuditEvent>, StoreError>;
}

#[derive(Clone, Debug)]
pub struct SqliteBookingRepository {
    pool: SqlitePool,
}

impl SqliteBookingRepository {
    /// Open (creating if absent) the `SQLite` database at `path` and run migrations.
    ///
    /// # Errors
    /// Returns [`StoreError::Sqlx`] if the file cannot be opened or the pool
    /// cannot connect, and [`StoreError::Migration`] if migrations fail to apply.
    pub async fn open(path: impl AsRef<Path>) -> Result<Self, StoreError> {
        let options = SqliteConnectOptions::new()
            .filename(path)
            .create_if_missing(true)
            .foreign_keys(true)
            .journal_mode(SqliteJournalMode::Wal)
            .busy_timeout(Duration::from_secs(5));

        let pool = SqlitePoolOptions::new()
            .max_connections(5)
            .connect_with(options)
            .await?;

        MIGRATOR.run(&pool).await?;
        Ok(Self { pool })
    }

    #[must_use]
    pub fn pool(&self) -> &SqlitePool {
        &self.pool
    }
}

#[async_trait]
impl BookingRepository for SqliteBookingRepository {
    async fn create(&self, booking: NewBooking) -> Result<BookingAggregate, StoreError> {
        let now = now_ms()?;
        let state = BookingState::Draft(Draft);
        let state_json = serde_json::to_string(&state)?;
        let requirements_json = serde_json::to_string(&booking.requirements)?;

        let result = sqlx::query(
            r"
            INSERT OR IGNORE INTO bookings (
                id, version, state_name, state_json, requirements_json,
                selected_venue_json, availability_json, booking_ref, active_effect,
                created_at_ms, updated_at_ms
            ) VALUES (?, 0, ?, ?, ?, NULL, NULL, NULL, NULL, ?, ?)
            ",
        )
        .bind(booking.id.as_str())
        .bind(state.name())
        .bind(state_json)
        .bind(requirements_json)
        .bind(now)
        .bind(now)
        .execute(&self.pool)
        .await?;

        if result.rows_affected() != 1 {
            return Err(StoreError::AlreadyExists(booking.id));
        }

        Ok(BookingAggregate {
            id: booking.id,
            version: 0,
            state,
            requirements: booking.requirements,
            selected_venue: None,
            availability: None,
            booking_ref: None,
            active_effect: None,
            created_at_ms: now,
            updated_at_ms: now,
        })
    }

    async fn load(&self, id: &BookingId) -> Result<BookingAggregate, StoreError> {
        let row = sqlx::query(
            r"
            SELECT id, version, state_name, state_json, requirements_json,
                   selected_venue_json, availability_json, booking_ref, active_effect,
                   created_at_ms, updated_at_ms
            FROM bookings
            WHERE id = ?
            ",
        )
        .bind(id.as_str())
        .fetch_optional(&self.pool)
        .await?
        .ok_or_else(|| StoreError::NotFound(id.clone()))?;

        decode_booking_row(&row)
    }

    async fn commit(
        &self,
        id: &BookingId,
        expected_version: u64,
        next: BookingWrite,
        audit: TransitionAudit,
    ) -> Result<BookingAggregate, StoreError> {
        let expected_db = version_to_i64(expected_version)?;
        let next_version = expected_version
            .checked_add(1)
            .ok_or(StoreError::VersionOutOfRange)?;
        let next_db = version_to_i64(next_version)?;
        let now = now_ms()?;

        // `BEGIN IMMEDIATE`, not the default `BEGIN` (deferred).
        //
        // `commit` unconditionally writes, so a deferred begin buys nothing: it
        // takes no lock, the version `SELECT` opens a read transaction, and the
        // `UPDATE` then has to promote that read to a write. Under WAL a
        // deferred transaction cannot promote once anyone has written anywhere
        // in the database, and because `inTransaction` is already `TRANS_READ`
        // the busy handler is skipped entirely - so `busy_timeout` never
        // applies and the call fails immediately with SQLITE_BUSY.
        //
        // Measured on this code: ~52 of 60 concurrent commits to *completely
        // unrelated* bookings failed with "database is locked", with no version
        // contention between them at all. A genuine CAS loser got SQLITE_BUSY
        // rather than StaleVersion ~99.7% of the time.
        //
        // IMMEDIATE takes the write lock at BEGIN, when `inTransaction` is
        // still `TRANS_NONE`, so the busy handler does engage: a second writer
        // waits (microseconds for a local write), gets a *fresh* snapshot, and
        // its `SELECT` then reports the truth. SQLite permits only one writer
        // regardless, so this costs no real concurrency - it moves the
        // serialisation point from mid-transaction, where it failed, to BEGIN,
        // where it waits.
        //
        // See ADR-015 for the tradeoff this carries into M4.
        let mut tx = self.pool.begin_with("BEGIN IMMEDIATE").await?;

        let current =
            sqlx::query("SELECT version, state_name, created_at_ms FROM bookings WHERE id = ?")
                .bind(id.as_str())
                .fetch_optional(&mut *tx)
                .await?
                .ok_or_else(|| StoreError::NotFound(id.clone()))?;

        let actual_version = version_from_i64(current.try_get::<i64, _>("version")?)?;
        if actual_version != expected_version {
            return Err(StoreError::StaleVersion {
                expected: expected_version,
                actual: actual_version,
            });
        }

        let from_state: String = current.try_get("state_name")?;
        let created_at_ms: i64 = current.try_get("created_at_ms")?;

        let state_json = serde_json::to_string(&next.state)?;
        let requirements_json = serde_json::to_string(&next.requirements)?;
        let selected_venue_json = serialize_optional(next.selected_venue.as_ref())?;
        let availability_json = serialize_optional(next.availability.as_ref())?;
        let booking_ref = next.booking_ref.as_ref().map(ToString::to_string);
        let active_effect = next.active_effect.as_ref().map(ToString::to_string);

        let result = sqlx::query(
            r"
            UPDATE bookings
            SET version = ?, state_name = ?, state_json = ?, requirements_json = ?,
                selected_venue_json = ?, availability_json = ?, booking_ref = ?,
                active_effect = ?, updated_at_ms = ?
            WHERE id = ? AND version = ?
            ",
        )
        .bind(next_db)
        .bind(next.state.name())
        .bind(&state_json)
        .bind(&requirements_json)
        .bind(&selected_venue_json)
        .bind(&availability_json)
        .bind(&booking_ref)
        .bind(&active_effect)
        .bind(now)
        .bind(id.as_str())
        .bind(expected_db)
        .execute(&mut *tx)
        .await?;

        if result.rows_affected() != 1 {
            let actual = current_version_in_tx(&mut tx, id).await?;
            return Err(StoreError::StaleVersion {
                expected: expected_version,
                actual,
            });
        }

        let event_id = format!("AUDIT-{id}-{next_version}");
        sqlx::query(
            r"
            INSERT INTO audit_events (
                event_id, booking_id, from_version, to_version,
                from_state, to_state, proposal, outcome, evidence_summary, created_at_ms
            ) VALUES (?, ?, ?, ?, ?, ?, ?, ?, ?, ?)
            ",
        )
        .bind(&event_id)
        .bind(id.as_str())
        .bind(expected_db)
        .bind(next_db)
        .bind(&from_state)
        .bind(next.state.name())
        .bind(&audit.proposal)
        .bind(&audit.outcome)
        .bind(&audit.evidence_summary)
        .bind(now)
        .execute(&mut *tx)
        .await?;

        tx.commit().await?;

        Ok(BookingAggregate {
            id: id.clone(),
            version: next_version,
            state: next.state,
            requirements: next.requirements,
            selected_venue: next.selected_venue,
            availability: next.availability,
            booking_ref: next.booking_ref,
            active_effect: next.active_effect,
            created_at_ms,
            updated_at_ms: now,
        })
    }

    async fn audit_events(&self, id: &BookingId) -> Result<Vec<AuditEvent>, StoreError> {
        let rows = sqlx::query(
            r"
            SELECT sequence, event_id, booking_id, from_version, to_version,
                   from_state, to_state, proposal, outcome, evidence_summary, created_at_ms
            FROM audit_events
            WHERE booking_id = ?
            ORDER BY sequence ASC
            ",
        )
        .bind(id.as_str())
        .fetch_all(&self.pool)
        .await?;

        rows.iter().map(decode_audit_row).collect()
    }
}

fn serialize_optional<T: serde::Serialize>(
    value: Option<&T>,
) -> Result<Option<String>, StoreError> {
    value
        .map(serde_json::to_string)
        .transpose()
        .map_err(StoreError::from)
}

fn decode_booking_row(row: &sqlx::sqlite::SqliteRow) -> Result<BookingAggregate, StoreError> {
    let id_string: String = row.try_get("id")?;
    let version = version_from_i64(row.try_get::<i64, _>("version")?)?;
    let state_name: String = row.try_get("state_name")?;
    let state_json: String = row.try_get("state_json")?;
    let requirements_json: String = row.try_get("requirements_json")?;
    let selected_venue_json: Option<String> = row.try_get("selected_venue_json")?;
    let availability_json: Option<String> = row.try_get("availability_json")?;
    let booking_ref: Option<String> = row.try_get("booking_ref")?;
    let active_effect: Option<String> = row.try_get("active_effect")?;

    let state: BookingState = serde_json::from_str(&state_json)?;
    if state.name() != state_name {
        return Err(StoreError::CorruptRow(format!(
            "state discriminator {state_name:?} does not match payload {:?}",
            state.name()
        )));
    }

    Ok(BookingAggregate {
        id: BookingId::new(id_string),
        version,
        state,
        requirements: serde_json::from_str(&requirements_json)?,
        selected_venue: selected_venue_json
            .map(|json| serde_json::from_str(&json))
            .transpose()?,
        availability: availability_json
            .map(|json| serde_json::from_str(&json))
            .transpose()?,
        booking_ref: booking_ref.map(CouncilBookingRef::new),
        active_effect: active_effect.map(EffectIntentId::new),
        created_at_ms: row.try_get("created_at_ms")?,
        updated_at_ms: row.try_get("updated_at_ms")?,
    })
}

fn decode_audit_row(row: &sqlx::sqlite::SqliteRow) -> Result<AuditEvent, StoreError> {
    Ok(AuditEvent {
        sequence: row.try_get("sequence")?,
        event_id: row.try_get("event_id")?,
        booking_id: BookingId::new(row.try_get::<String, _>("booking_id")?),
        from_version: version_from_i64(row.try_get::<i64, _>("from_version")?)?,
        to_version: version_from_i64(row.try_get::<i64, _>("to_version")?)?,
        from_state: row.try_get("from_state")?,
        to_state: row.try_get("to_state")?,
        proposal: row.try_get("proposal")?,
        outcome: row.try_get("outcome")?,
        evidence_summary: row.try_get("evidence_summary")?,
        created_at_ms: row.try_get("created_at_ms")?,
    })
}

async fn current_version_in_tx(
    tx: &mut sqlx::Transaction<'_, sqlx::Sqlite>,
    id: &BookingId,
) -> Result<u64, StoreError> {
    let row = sqlx::query("SELECT version FROM bookings WHERE id = ?")
        .bind(id.as_str())
        .fetch_optional(&mut **tx)
        .await?
        .ok_or_else(|| StoreError::NotFound(id.clone()))?;

    version_from_i64(row.try_get::<i64, _>("version")?)
}

fn version_to_i64(value: u64) -> Result<i64, StoreError> {
    i64::try_from(value).map_err(|_| StoreError::VersionOutOfRange)
}

fn version_from_i64(value: i64) -> Result<u64, StoreError> {
    u64::try_from(value).map_err(|_| StoreError::CorruptRow("negative version".to_owned()))
}

fn now_ms() -> Result<i64, StoreError> {
    let duration = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map_err(|_| StoreError::ClockOutOfRange)?;
    i64::try_from(duration.as_millis()).map_err(|_| StoreError::ClockOutOfRange)
}

#[cfg(test)]
mod tests {
    use super::*;
    use bld_types::{Money, SlotId, TimeWindow, VenueId};
    use tempfile::TempDir;
    use townhall_domain::{BookingState, SelectedVenueRef, VenueSelected};

    fn requirements() -> BookingRequirements {
        BookingRequirements {
            purpose: "community meeting".to_owned(),
            requested_date: "2026-08-20".to_owned(),
            time_window: TimeWindow {
                from: "13:00".to_owned(),
                to: "17:00".to_owned(),
            },
            attendees: 20,
            wheelchair_accessible: true,
            max_fee: Money::from_pence(5_000),
        }
    }

    async fn repo_in(temp: &TempDir) -> SqliteBookingRepository {
        SqliteBookingRepository::open(temp.path().join("townhall.sqlite"))
            .await
            .expect("repository should open")
    }

    #[tokio::test]
    async fn create_and_reload_survives_repository_restart() {
        let temp = TempDir::new().expect("temp dir");
        let id = BookingId::new("BKG-M3-RESTART");

        {
            let repo = repo_in(&temp).await;
            let created = repo
                .create(NewBooking {
                    id: id.clone(),
                    requirements: requirements(),
                })
                .await
                .expect("create should succeed");

            assert_eq!(created.version, 0);
            assert!(matches!(created.state, BookingState::Draft(_)));
        }

        let reopened = repo_in(&temp).await;
        let loaded = reopened.load(&id).await.expect("load after restart");
        assert_eq!(loaded.id, id);
        assert_eq!(loaded.version, 0);
        assert_eq!(loaded.requirements.attendees, 20);
        assert!(matches!(loaded.state, BookingState::Draft(_)));
    }

    #[tokio::test]
    async fn compare_and_set_allows_one_writer_and_rejects_stale_copy() {
        let temp = TempDir::new().expect("temp dir");
        let repo = repo_in(&temp).await;
        let id = BookingId::new("BKG-M3-CAS");
        let created = repo
            .create(NewBooking {
                id: id.clone(),
                requirements: requirements(),
            })
            .await
            .expect("create should succeed");

        let stale_copy_a = created.clone();
        let stale_copy_b = created;

        let selected = SelectedVenueRef {
            venue_id: VenueId::new("TH-A"),
            slot_id: SlotId::new("SLOT-A-1400-1700"),
        };

        let committed = repo
            .commit(
                &id,
                stale_copy_a.version,
                BookingWrite {
                    state: BookingState::VenueSelected(VenueSelected {
                        venue_id: selected.venue_id.clone(),
                        slot_id: selected.slot_id.clone(),
                    }),
                    requirements: stale_copy_a.requirements,
                    selected_venue: Some(selected),
                    availability: None,
                    booking_ref: None,
                    active_effect: None,
                },
                TransitionAudit::committed("SelectVenue", None),
            )
            .await
            .expect("first CAS should win");

        assert_eq!(committed.version, 1);

        let error = repo
            .commit(
                &id,
                stale_copy_b.version,
                BookingWrite::from(&stale_copy_b),
                TransitionAudit::committed("Cancel", None),
            )
            .await
            .expect_err("stale copy must lose");

        assert!(matches!(
            error,
            StoreError::StaleVersion {
                expected: 0,
                actual: 1
            }
        ));

        let current = repo.load(&id).await.expect("current state");
        assert_eq!(current.version, 1);
        assert!(matches!(current.state, BookingState::VenueSelected(_)));
    }

    #[tokio::test]
    async fn audit_event_is_committed_with_state_change() {
        let temp = TempDir::new().expect("temp dir");
        let repo = repo_in(&temp).await;
        let id = BookingId::new("BKG-M3-AUDIT");
        let created = repo
            .create(NewBooking {
                id: id.clone(),
                requirements: requirements(),
            })
            .await
            .expect("create should succeed");

        let next = BookingWrite {
            state: BookingState::VenueSelected(VenueSelected {
                venue_id: VenueId::new("TH-A"),
                slot_id: SlotId::new("SLOT-A-1400-1700"),
            }),
            requirements: created.requirements,
            selected_venue: Some(SelectedVenueRef {
                venue_id: VenueId::new("TH-A"),
                slot_id: SlotId::new("SLOT-A-1400-1700"),
            }),
            availability: None,
            booking_ref: None,
            active_effect: None,
        };

        repo.commit(
            &id,
            0,
            next,
            TransitionAudit::committed("SelectVenue", Some("no external effect".to_owned())),
        )
        .await
        .expect("commit should succeed");

        let audit = repo.audit_events(&id).await.expect("audit read");
        assert_eq!(audit.len(), 1);
        assert_eq!(audit[0].from_version, 0);
        assert_eq!(audit[0].to_version, 1);
        assert_eq!(audit[0].from_state, "Draft");
        assert_eq!(audit[0].to_state, "VenueSelected");
        assert_eq!(audit[0].proposal, "SelectVenue");
    }

    #[tokio::test]
    async fn duplicate_create_is_rejected_without_overwriting_resource() {
        let temp = TempDir::new().expect("temp dir");
        let repo = repo_in(&temp).await;
        let id = BookingId::new("BKG-M3-DUP");

        repo.create(NewBooking {
            id: id.clone(),
            requirements: requirements(),
        })
        .await
        .expect("first create");

        let error = repo
            .create(NewBooking {
                id: id.clone(),
                requirements: requirements(),
            })
            .await
            .expect_err("duplicate create must fail");

        assert!(matches!(error, StoreError::AlreadyExists(actual) if actual == id));
    }
}

#[cfg(test)]
mod concurrency {
    use super::*;
    use bld_types::{Money, SlotId, TimeWindow, VenueId};
    use std::sync::Arc;
    use tempfile::TempDir;
    use tokio::sync::Barrier;
    use townhall_domain::{BookingState, SelectedVenueRef, VenueSelected};

    /// Enough rounds that both interleavings (loser's SELECT before vs. after
    /// the winner's commit) are exercised, without making the suite slow.
    const RACE_ROUNDS: usize = 32;

    fn requirements() -> BookingRequirements {
        BookingRequirements {
            purpose: "community meeting".to_owned(),
            requested_date: "2026-08-20".to_owned(),
            time_window: TimeWindow {
                from: "13:00".to_owned(),
                to: "17:00".to_owned(),
            },
            attendees: 20,
            wheelchair_accessible: true,
            max_fee: Money::from_pence(5_000),
        }
    }

    fn write_for(venue: &str, slot: &str, requirements: BookingRequirements) -> BookingWrite {
        BookingWrite {
            state: BookingState::VenueSelected(VenueSelected {
                venue_id: VenueId::new(venue),
                slot_id: SlotId::new(slot),
            }),
            requirements,
            selected_venue: Some(SelectedVenueRef {
                venue_id: VenueId::new(venue),
                slot_id: SlotId::new(slot),
            }),
            availability: None,
            booking_ref: None,
            active_effect: None,
        }
    }

    /// Two writers race the same version. Exactly one may commit.
    ///
    /// The existing sequential test commits and *then* tries a stale write,
    /// which proves the version arithmetic but never exercises two writers
    /// overlapping. This one genuinely races them: two `tokio::spawn`ed tasks
    /// on separate worker threads, aligned by a `Barrier` released immediately
    /// before `commit`, over a pool with enough connections that they are not
    /// serialised by the pool semaphore.
    #[tokio::test(flavor = "multi_thread", worker_threads = 4)]
    async fn concurrent_writers_on_one_version_produce_exactly_one_commit() {
        for round in 0..RACE_ROUNDS {
            let temp = TempDir::new().expect("temp dir");
            let repo = SqliteBookingRepository::open(temp.path().join("townhall.sqlite"))
                .await
                .expect("repository should open");
            let id = BookingId::new(format!("BKG-RACE-{round}"));

            let created = repo
                .create(NewBooking {
                    id: id.clone(),
                    requirements: requirements(),
                })
                .await
                .expect("create should succeed");

            let barrier = Arc::new(Barrier::new(2));
            let mut handles = Vec::new();

            // Distinct payloads so the persisted row identifies the winner.
            for venue in ["TH-A", "TH-B"] {
                let repo = repo.clone();
                let id = id.clone();
                let barrier = Arc::clone(&barrier);
                let write = write_for(venue, "SLOT-1", created.requirements.clone());
                let expected = created.version;

                handles.push(tokio::spawn(async move {
                    // All setup is done; align the two tasks precisely here.
                    barrier.wait().await;
                    repo.commit(
                        &id,
                        expected,
                        write,
                        TransitionAudit::committed("SelectVenue", None),
                    )
                    .await
                }));
            }

            let mut winners = Vec::new();
            let mut losers = Vec::new();
            for handle in handles {
                match handle.await.expect("task should not panic") {
                    Ok(aggregate) => winners.push(aggregate),
                    Err(error) => losers.push(error),
                }
            }

            assert_eq!(
                winners.len(),
                1,
                "round {round}: expected exactly one commit, got {} (losers: {losers:?})",
                winners.len()
            );

            // The loser must be refused for the right reason. `StaleVersion` is
            // what the boundary owes its caller: an honest `Denied`, which M5
            // maps to 412 and M4's reconciliation workers key their retry on.
            // A `Sqlx(SQLITE_BUSY)` here means the write lock was not taken at
            // BEGIN, which is the defect ADR-015 fixes.
            let loser = losers.first().expect("one writer must lose");
            assert!(
                matches!(
                    loser,
                    StoreError::StaleVersion {
                        expected: 0,
                        actual: 1
                    }
                ),
                "round {round}: loser should be refused with StaleVersion, got {loser:?}. \
                 A BUSY error here means a writer held the lock past the busy timeout, \
                 which is a real bug rather than test noise."
            );

            let current = repo.load(&id).await.expect("load after race");
            assert_eq!(
                current.version, 1,
                "round {round}: exactly one version bump"
            );

            let audit = repo.audit_events(&id).await.expect("audit read");
            assert_eq!(audit.len(), 1, "round {round}: exactly one audit event");

            // The row must be one writer's write, not a blend of both.
            let BookingState::VenueSelected(selected) = &current.state else {
                panic!("round {round}: unexpected state {:?}", current.state);
            };
            let venue = selected.venue_id.as_str();
            assert!(
                venue == "TH-A" || venue == "TH-B",
                "round {round}: persisted venue {venue} is neither writer's write"
            );
            assert_eq!(
                current.selected_venue.as_ref().map(|s| s.venue_id.as_str()),
                Some(venue),
                "round {round}: state and selected_venue disagree - the row is a blend"
            );
        }
    }

    /// Concurrent commits to *unrelated* bookings must not interfere.
    ///
    /// There is no version contention here at all - different resources,
    /// different rows. Under a deferred transaction roughly half of these fail
    /// with "database is locked", because a deferred read transaction cannot
    /// promote to a write once anyone has written anywhere in the database, and
    /// the busy handler is bypassed on that path.
    #[tokio::test(flavor = "multi_thread", worker_threads = 4)]
    async fn concurrent_commits_to_disjoint_bookings_all_succeed() {
        const PAIRS: usize = 60;

        let temp = TempDir::new().expect("temp dir");
        let repo = SqliteBookingRepository::open(temp.path().join("townhall.sqlite"))
            .await
            .expect("repository should open");

        let mut ids = Vec::new();
        for index in 0..PAIRS {
            let id = BookingId::new(format!("BKG-DISJOINT-{index}"));
            repo.create(NewBooking {
                id: id.clone(),
                requirements: requirements(),
            })
            .await
            .expect("create should succeed");
            ids.push(id);
        }

        let barrier = Arc::new(Barrier::new(PAIRS));
        let mut handles = Vec::new();
        for id in ids {
            let repo = repo.clone();
            let barrier = Arc::clone(&barrier);
            let write = write_for("TH-A", "SLOT-1", requirements());
            handles.push(tokio::spawn(async move {
                barrier.wait().await;
                repo.commit(
                    &id,
                    0,
                    write,
                    TransitionAudit::committed("SelectVenue", None),
                )
                .await
                .map(|_| ())
                .map_err(|error| format!("{id}: {error}"))
            }));
        }

        let mut failures = Vec::new();
        for handle in handles {
            if let Err(error) = handle.await.expect("task should not panic") {
                failures.push(error);
            }
        }

        assert!(
            failures.is_empty(),
            "{} of {PAIRS} commits to unrelated bookings failed; there is no version \
             contention between them, so every one should succeed. First few: {:?}",
            failures.len(),
            &failures[..failures.len().min(3)]
        );
    }
}
