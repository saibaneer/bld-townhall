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
use townhall_domain::{
    BookingAggregate, BookingPlan, BookingState, Draft, EffectIntent, EffectStatus, OperationKind,
    SelectedVenueRef, VenueFacts,
};

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
    #[error("effect intent {0} was not found")]
    EffectNotFound(EffectIntentId),
    #[error(
        "effect identity disagrees: {where_} carries {found}, but this operation is {expected}"
    )]
    InconsistentEffectIdentity {
        where_: &'static str,
        found: EffectIntentId,
        expected: EffectIntentId,
    },
    #[error(
        "an effect intent already exists for booking {booking_id} operation {operation_kind} \
         at version {source_version}, with a different canonical plan"
    )]
    ConflictingPlan {
        booking_id: BookingId,
        operation_kind: &'static str,
        source_version: u64,
    },
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

    /// Commit a state transition and durably record the external effect it
    /// intends, atomically. Returns *committed* state, so no capability can be
    /// invoked while the transaction is open.
    ///
    /// Idempotent on `(booking_id, operation_kind, source_version)`: a retry
    /// after a lost acknowledgement returns the committed intent with
    /// `replayed: true` and nothing written.
    ///
    /// The aggregate returned on a replay is the **current** one, not a
    /// snapshot of what was committed alongside the intent. That is deliberate:
    /// by the time a retry arrives the booking may legitimately have advanced
    /// or finalised, and handing back a stale snapshot would be more dangerous
    /// than handing back the truth. A coordinator must therefore re-check both
    /// the current state and the intent's status before executing anything.
    ///
    /// # Errors
    /// [`StoreError::ConflictingPlan`] if an intent already exists for that key
    /// with a different canonical plan — that is a boundary violation, not a
    /// retry, and it fails closed.
    async fn prepare_effect(&self, request: PrepareEffect) -> Result<PreparedEffect, StoreError>;

    /// Read one effect intent.
    ///
    /// # Errors
    /// [`StoreError::EffectNotFound`] if no such intent exists.
    async fn load_effect(&self, id: &EffectIntentId) -> Result<EffectIntent, StoreError>;
}

#[derive(Clone, Debug)]
pub struct SqliteBookingRepository {
    pool: SqlitePool,
    effect_ttl_ms: i64,
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
        Ok(Self {
            pool,
            effect_ttl_ms: DEFAULT_EFFECT_TTL_MS,
        })
    }

    /// Open with a non-default effect TTL. For tests that need a deadline they
    /// can actually reach.
    ///
    /// # Errors
    /// As [`Self::open`].
    pub async fn open_with_ttl(
        path: impl AsRef<Path>,
        effect_ttl_ms: i64,
    ) -> Result<Self, StoreError> {
        let mut repo = Self::open(path).await?;
        repo.effect_ttl_ms = effect_ttl_ms;
        Ok(repo)
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
        let now = now_ms()?;

        let mut tx = self.pool.begin_with("BEGIN IMMEDIATE").await?;
        let aggregate = commit_in_tx(&mut tx, id, expected_version, &next, &audit, now).await?;
        tx.commit().await?;
        Ok(aggregate)
    }

    async fn prepare_effect(&self, request: PrepareEffect) -> Result<PreparedEffect, StoreError> {
        let plan_json = serde_json::to_string(&request.canonical_plan)?;
        let source_db = version_to_i64(request.source_version)?;

        // ADR-016: sampled once, immediately before the transaction opens. Not
        // at commit - the commit instant is unknowable from inside the
        // transaction that must persist the value. Sampling early is also the
        // safe direction: the deadline lands marginally earlier than a
        // commit-time reading would, never later, so the council can never act
        // on an intent we already consider dead.
        let prepared_at_ms = now_ms()?;
        let expires_at_ms = prepared_at_ms
            .checked_add(self.effect_ttl_ms)
            .ok_or(StoreError::ClockOutOfRange)?;
        let now = prepared_at_ms;

        // The repository owns the effect identity. If the caller supplied
        // `active_effect` on `next`, the aggregate and the intent row could
        // name different effects - and recovery, which reads `active_effect`,
        // would then look up an intent that does not exist. ADR-014's stable
        // identity requires these to be the same value by construction.
        let effect_intent_id = derive_effect_intent_id(
            &request.booking_id,
            request.operation_kind,
            request.source_version,
        );
        verify_effect_identity(&request, &effect_intent_id)?;

        let next = BookingWrite {
            active_effect: Some(effect_intent_id.clone()),
            ..request.next.clone()
        };

        let mut tx = self.pool.begin_with("BEGIN IMMEDIATE").await?;

        // Look for an existing intent for this operation FIRST. A retry after a
        // lost acknowledgement must return what was already committed rather
        // than attempt the CAS again - the version has already advanced, so the
        // CAS would fail and recovery would strand with one intent it cannot
        // resume.
        let existing = sqlx::query(
            r"
            SELECT effect_intent_id, booking_id, operation_kind, source_version,
                   canonical_plan_json, status, expires_at_ms, provider_reference,
                   created_at_ms, updated_at_ms
            FROM effect_intents
            WHERE booking_id = ? AND operation_kind = ? AND source_version = ?
            ",
        )
        .bind(request.booking_id.as_str())
        .bind(request.operation_kind.name())
        .bind(source_db)
        .fetch_optional(&mut *tx)
        .await?;

        if let Some(row) = existing {
            let replayed = replay_existing(&mut tx, &request, &row, &plan_json).await?;
            tx.commit().await?;
            return Ok(replayed);
        }

        // No intent yet: commit the transition and record the effect together.
        let aggregate = commit_in_tx(
            &mut tx,
            &request.booking_id,
            request.source_version,
            &next,
            &request.audit,
            now,
        )
        .await?;

        let effect_intent_id = derive_effect_intent_id(
            &request.booking_id,
            request.operation_kind,
            request.source_version,
        );

        sqlx::query(
            r"
            INSERT INTO effect_intents (
                effect_intent_id, booking_id, operation_kind, source_version,
                canonical_plan_json, status, expires_at_ms, provider_reference,
                last_error, created_at_ms, updated_at_ms
            ) VALUES (?, ?, ?, ?, ?, ?, ?, NULL, NULL, ?, ?)
            ",
        )
        .bind(effect_intent_id.as_str())
        .bind(request.booking_id.as_str())
        .bind(request.operation_kind.name())
        .bind(source_db)
        .bind(&plan_json)
        .bind(EffectStatus::Prepared.name())
        .bind(expires_at_ms)
        .bind(now)
        .bind(now)
        .execute(&mut *tx)
        .await?;

        tx.commit().await?;

        Ok(PreparedEffect {
            aggregate,
            intent: EffectIntent {
                effect_intent_id,
                booking_id: request.booking_id,
                operation_kind: request.operation_kind,
                source_version: request.source_version,
                canonical_plan: request.canonical_plan,
                status: EffectStatus::Prepared,
                expires_at_ms,
                provider_reference: None,
                created_at_ms: now,
                updated_at_ms: now,
            },
            replayed: false,
        })
    }

    async fn load_effect(&self, id: &EffectIntentId) -> Result<EffectIntent, StoreError> {
        let row = sqlx::query(
            r"
            SELECT effect_intent_id, booking_id, operation_kind, source_version,
                   canonical_plan_json, status, expires_at_ms, provider_reference,
                   created_at_ms, updated_at_ms
            FROM effect_intents
            WHERE effect_intent_id = ?
            ",
        )
        .bind(id.as_str())
        .fetch_optional(&self.pool)
        .await?
        .ok_or_else(|| StoreError::EffectNotFound(id.clone()))?;

        decode_effect_row(&row)
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

/// The compare-and-set, the audit row, and nothing else — no `BEGIN`, no
/// `COMMIT`. Callers own the transaction so a state change and the effect
/// intent it implies can be written together.
///
/// # The caller must open with `BEGIN IMMEDIATE`
///
/// This function unconditionally writes, so a *deferred* transaction buys
/// nothing: it takes no lock, the version `SELECT` opens a read transaction,
/// and the `UPDATE` then has to promote that read to a write. Under WAL a
/// deferred transaction cannot promote once anyone has written anywhere in the
/// database — and because `inTransaction` is already `TRANS_READ`, `SQLite`
/// skips the busy handler, so `busy_timeout` never applies and the call fails
/// immediately.
///
/// Measured before the fix: ~52 of 60 concurrent commits to *completely
/// unrelated* bookings failed with "database is locked", and a genuine CAS
/// loser got `SQLITE_BUSY` rather than `StaleVersion` ~99.7% of the time.
/// See ADR-015.
async fn commit_in_tx(
    tx: &mut sqlx::Transaction<'_, sqlx::Sqlite>,
    id: &BookingId,
    expected_version: u64,
    next: &BookingWrite,
    audit: &TransitionAudit,
    now: i64,
) -> Result<BookingAggregate, StoreError> {
    let expected_db = version_to_i64(expected_version)?;
    let next_version = expected_version
        .checked_add(1)
        .ok_or(StoreError::VersionOutOfRange)?;
    let next_db = version_to_i64(next_version)?;

    let current =
        sqlx::query("SELECT version, state_name, created_at_ms FROM bookings WHERE id = ?")
            .bind(id.as_str())
            .fetch_optional(&mut **tx)
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
    .execute(&mut **tx)
    .await?;

    if result.rows_affected() != 1 {
        let actual = current_version_in_tx(tx, id).await?;
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
    .execute(&mut **tx)
    .await?;

    Ok(BookingAggregate {
        id: id.clone(),
        version: next_version,
        state: next.state.clone(),
        requirements: next.requirements.clone(),
        selected_venue: next.selected_venue.clone(),
        availability: next.availability.clone(),
        booking_ref: next.booking_ref.clone(),
        active_effect: next.active_effect.clone(),
        created_at_ms,
        updated_at_ms: now,
    })
}

/// Read the aggregate from inside an open transaction.
async fn load_booking_in_tx(
    tx: &mut sqlx::Transaction<'_, sqlx::Sqlite>,
    id: &BookingId,
) -> Result<BookingAggregate, StoreError> {
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
    .fetch_optional(&mut **tx)
    .await?
    .ok_or_else(|| StoreError::NotFound(id.clone()))?;

    decode_booking_row(&row)
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

// ---------------------------------------------------------------- M4 slice A

/// A request to durably record an intended external consequence *and* commit
/// the state transition that creates it, in one transaction.
///
/// Deliberately one operation rather than `insert_intent` + `commit_booking`.
/// Two calls leave a crash window with an orphan intent or an in-flight state
/// with no identity to reconcile against — and they leave a signature through
/// which a capability could be invoked while a transaction is open, which
/// ADR-014 forbids.
#[derive(Clone, Debug)]
pub struct PrepareEffect {
    pub booking_id: BookingId,
    pub operation_kind: OperationKind,
    /// The aggregate version this effect is derived from. Also the CAS
    /// expectation, and part of the uniqueness key.
    pub source_version: u64,
    pub canonical_plan: BookingPlan,
    /// The state to commit alongside the intent.
    ///
    /// `active_effect` on this write is **ignored**: the repository owns the
    /// effect identity and sets it, so the aggregate and the intent row cannot
    /// disagree. See [`BookingRepository::prepare_effect`].
    pub next: BookingWrite,
    pub audit: TransitionAudit,
}

/// How long an effect intent may be acted on, from the instant Phase A is
/// prepared.
///
/// ADR-016: the deadline is *derived*, never supplied by a caller. A caller
/// that could choose it could set it in the past and have the council tombstone
/// the effect immediately, or far in the future and extend the creation window
/// past policy.
pub const DEFAULT_EFFECT_TTL_MS: i64 = 30_000;

/// The committed result of [`BookingRepository::prepare_effect`].
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct PreparedEffect {
    pub aggregate: BookingAggregate,
    pub intent: EffectIntent,
    /// True when this call found an existing intent for the same operation
    /// rather than creating one — a retry after a lost acknowledgement.
    /// Nothing was written.
    pub replayed: bool,
}

impl StoreError {
    fn corrupt(what: impl Into<String>) -> Self {
        Self::CorruptRow(what.into())
    }
}

fn decode_effect_row(row: &sqlx::sqlite::SqliteRow) -> Result<EffectIntent, StoreError> {
    let kind_text: String = row.try_get("operation_kind")?;
    let status_text: String = row.try_get("status")?;
    let plan_json: String = row.try_get("canonical_plan_json")?;
    let provider_reference: Option<String> = row.try_get("provider_reference")?;

    Ok(EffectIntent {
        effect_intent_id: EffectIntentId::new(row.try_get::<String, _>("effect_intent_id")?),
        booking_id: BookingId::new(row.try_get::<String, _>("booking_id")?),
        operation_kind: OperationKind::parse(&kind_text)
            .map_err(|bad| StoreError::corrupt(format!("unknown operation_kind {bad:?}")))?,
        source_version: version_from_i64(row.try_get::<i64, _>("source_version")?)?,
        canonical_plan: serde_json::from_str(&plan_json)?,
        status: EffectStatus::parse(&status_text)
            .map_err(|bad| StoreError::corrupt(format!("unknown effect status {bad:?}")))?,
        expires_at_ms: row.try_get("expires_at_ms")?,
        provider_reference: provider_reference.map(CouncilBookingRef::new),
        created_at_ms: row.try_get("created_at_ms")?,
        updated_at_ms: row.try_get("updated_at_ms")?,
    })
}

/// Resume an operation whose intent is already committed.
///
/// Reached when a retry arrives after a lost acknowledgement. Nothing is
/// written: the CAS would fail anyway, because the version already advanced
/// when the intent was first committed — and if this returned an error instead,
/// the coordinator would hold one intent it could never resume.
async fn replay_existing(
    tx: &mut sqlx::Transaction<'_, sqlx::Sqlite>,
    request: &PrepareEffect,
    row: &sqlx::sqlite::SqliteRow,
    plan_json: &str,
) -> Result<PreparedEffect, StoreError> {
    let intent = decode_effect_row(row)?;

    // Same operation key, different plan, is not a retry: two different
    // consequences are competing for one identity. Fail closed rather than
    // pick one.
    let stored_plan: String = row.try_get("canonical_plan_json")?;
    if stored_plan != plan_json {
        return Err(StoreError::ConflictingPlan {
            booking_id: request.booking_id.clone(),
            operation_kind: request.operation_kind.name(),
            source_version: request.source_version,
        });
    }

    let aggregate = load_booking_in_tx(tx, &request.booking_id).await?;
    Ok(PreparedEffect {
        aggregate,
        intent,
        replayed: true,
    })
}

/// Every place that carries an effect id must carry *this* one.
///
/// The id is currently duplicated across the canonical plan, the in-flight
/// state and the aggregate's `active_effect`; slice B removes that
/// duplication. Until then:
///
/// - silently **rewriting** the caller's values would hide a coordinator bug;
/// - silently **accepting** them would let the plan name one effect while the
///   intent row names another — and fact binding later compares the plan's id
///   against provider evidence, so the *real* provider result would be
///   rejected as a mismatch.
///
/// So disagreement fails closed.
fn verify_effect_identity(
    request: &PrepareEffect,
    expected: &EffectIntentId,
) -> Result<(), StoreError> {
    let sites = [
        ("canonical plan", request.canonical_plan.effect_intent_id()),
        ("in-flight state", request.next.state.effect_intent_id()),
    ];
    for (where_, found) in sites {
        if let Some(found) = found
            && found != expected
        {
            return Err(StoreError::InconsistentEffectIdentity {
                where_,
                found: found.clone(),
                expected: expected.clone(),
            });
        }
    }
    Ok(())
}

/// Derive the identifier for one intended consequence.
///
/// **Public deliberately.** The coordinator must build its canonical plan and
/// its in-flight state carrying the *same* id the repository will persist, and
/// the only safe way to guarantee that is for both to call this one function.
/// `prepare_effect` then verifies they agree and fails closed if they do not.
///
/// Derived from the *operation identity* — resource, kind, source version —
/// not from the plan's contents. That distinction matters: hashing plan
/// content would give two legitimate identical operations the same id, whereas
/// two operations differing in resource or version can never collide here, and
/// a retry of the same operation reproduces the same id.
///
/// The value is written once and read back thereafter; the UNIQUE key on
/// `(booking_id, operation_kind, source_version)` is what actually guarantees
/// one intent per operation.
#[must_use]
pub fn derive_effect_intent_id(
    booking_id: &BookingId,
    operation_kind: OperationKind,
    source_version: u64,
) -> EffectIntentId {
    EffectIntentId::new(format!(
        "EFF-{}-{}-{}",
        booking_id.as_str(),
        operation_kind.name().to_uppercase(),
        source_version
    ))
}

#[cfg(test)]
mod effect_identity {
    use super::*;
    use bld_types::{Money, PrincipalId, SlotId, TimeWindow, VenueId};
    use tempfile::TempDir;
    use townhall_domain::{BookingInProgress, BookingState, SelectedVenueRef, VenueFacts};

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

    fn facts(venue: &str) -> VenueFacts {
        VenueFacts {
            venue_id: VenueId::new(venue),
            slot_id: SlotId::new("SLOT-A"),
            capacity: 30,
            wheelchair_accessible: true,
            fee: Money::from_pence(4_500),
            available: true,
        }
    }

    fn plan_for(venue: &str, effect: &EffectIntentId) -> BookingPlan {
        BookingPlan::Book {
            effect_intent_id: effect.clone(),
            principal: PrincipalId::new("lucy"),
            facts: facts(venue),
        }
    }

    fn in_progress_write(effect: &EffectIntentId) -> BookingWrite {
        BookingWrite {
            state: BookingState::BookingInProgress(BookingInProgress {
                effect_intent_id: effect.clone(),
            }),
            requirements: requirements(),
            selected_venue: Some(SelectedVenueRef {
                venue_id: VenueId::new("TH-A"),
                slot_id: SlotId::new("SLOT-A"),
            }),
            availability: Some(facts("TH-A")),
            booking_ref: None,
            active_effect: Some(effect.clone()),
        }
    }

    /// Build a prepare request the way a coordinator must: derive the id once
    /// with the shared function, then use it everywhere.
    fn prepare_at(id: &BookingId, version: u64, venue: &str) -> PrepareEffect {
        let effect = derive_effect_intent_id(id, OperationKind::Book, version);
        PrepareEffect {
            booking_id: id.clone(),
            operation_kind: OperationKind::Book,
            source_version: version,
            canonical_plan: plan_for(venue, &effect),
            next: in_progress_write(&effect),
            audit: TransitionAudit::committed("Book", None),
        }
    }

    async fn repo_in(temp: &TempDir) -> SqliteBookingRepository {
        SqliteBookingRepository::open(temp.path().join("townhall.sqlite"))
            .await
            .expect("repository should open")
    }

    async fn seeded(repo: &SqliteBookingRepository, id: &BookingId) {
        repo.create(NewBooking {
            id: id.clone(),
            requirements: requirements(),
        })
        .await
        .expect("create");
    }

    /// The gate. A lost acknowledgement must not strand recovery.
    ///
    /// It is not enough that a duplicate is *rejected*: if the retry errors,
    /// the coordinator has one committed intent it cannot resume, no duplicate
    /// effect and no way forward. The retry must return exactly what was
    /// committed — same id, same expiry, same plan, same aggregate — and it
    /// must do so after a restart, because that is when it matters.
    #[tokio::test]
    async fn lost_acknowledgement_retry_returns_the_committed_intent() {
        let temp = TempDir::new().expect("temp dir");
        let id = BookingId::new("BKG-RETRY");

        let first = {
            let repo = repo_in(&temp).await;
            seeded(&repo, &id).await;
            repo.prepare_effect(prepare_at(&id, 0, "TH-A"))
                .await
                .expect("first prepare")
        };
        assert!(!first.replayed, "the first call must actually write");
        assert_eq!(first.aggregate.version, 1);
        assert_eq!(first.intent.status, EffectStatus::Prepared);

        // Restart, as if the acknowledgement was lost and the process died.
        let reopened = repo_in(&temp).await;
        let retry = reopened
            .prepare_effect(prepare_at(&id, 0, "TH-A"))
            .await
            .expect("retry must resume, not error");

        assert!(retry.replayed, "the retry must be recognised as a replay");
        assert_eq!(retry.intent.effect_intent_id, first.intent.effect_intent_id);
        assert_eq!(retry.intent.expires_at_ms, first.intent.expires_at_ms);
        assert_eq!(retry.intent.canonical_plan, first.intent.canonical_plan);
        assert_eq!(retry.aggregate, first.aggregate, "same committed aggregate");

        // And nothing was written twice.
        assert_eq!(reopened.load(&id).await.expect("load").version, 1);
        assert_eq!(reopened.audit_events(&id).await.expect("audit").len(), 1);
    }

    /// The same operation key with a *different* canonical plan is not a retry.
    /// Two different consequences are competing for one identity, so fail
    /// closed rather than silently pick one.
    #[tokio::test]
    async fn same_key_with_a_different_plan_fails_closed() {
        let temp = TempDir::new().expect("temp dir");
        let repo = repo_in(&temp).await;
        let id = BookingId::new("BKG-CONFLICT");
        seeded(&repo, &id).await;

        repo.prepare_effect(prepare_at(&id, 0, "TH-A"))
            .await
            .expect("first");

        let error = repo
            .prepare_effect(prepare_at(&id, 0, "TH-B"))
            .await
            .expect_err("a different plan under the same key must be refused");

        assert!(
            matches!(error, StoreError::ConflictingPlan { .. }),
            "expected ConflictingPlan, got {error:?}"
        );
        // The refusal must not have disturbed what was already committed.
        assert_eq!(repo.load(&id).await.expect("load").version, 1);
    }

    /// A stale prepare writes nothing at all — no version bump, no intent.
    ///
    /// # What this does *not* prove
    ///
    /// Mutation-tested honestly: moving the intent `INSERT` off the transaction
    /// onto `&self.pool` — the Phase A violation ADR-014 forbids — does **not**
    /// fail this test. The stale CAS returns before the insert is reached, so
    /// there is nothing to orphan either way, and the test passes for the wrong
    /// reason. (Three sibling tests did fail under that mutation, but only
    /// incidentally, via lock contention against the open `IMMEDIATE`
    /// transaction — that is luck, not coverage.)
    ///
    /// What actually enforces atomicity here is the `&mut *tx` in the insert's
    /// signature. A deterministic failure point between the CAS and the intent
    /// write is what would test it properly, and that needs an injectable
    /// interruption the coordinator provides — slice C.
    #[tokio::test]
    async fn a_stale_prepare_writes_nothing() {
        let temp = TempDir::new().expect("temp dir");
        let repo = repo_in(&temp).await;
        let id = BookingId::new("BKG-ORPHAN");
        seeded(&repo, &id).await;

        // Move the aggregate on, so the prepare below is stale.
        repo.commit(
            &id,
            0,
            BookingWrite {
                state: BookingState::Draft(townhall_domain::Draft),
                requirements: requirements(),
                selected_venue: None,
                availability: None,
                booking_ref: None,
                active_effect: None,
            },
            TransitionAudit::committed("ChangeVenue", None),
        )
        .await
        .expect("advance to v1");

        let error = repo
            .prepare_effect(prepare_at(&id, 0, "TH-A"))
            .await
            .expect_err("a stale prepare must lose");
        assert!(
            matches!(error, StoreError::StaleVersion { .. }),
            "got {error:?}"
        );

        let orphan = repo
            .load_effect(&EffectIntentId::new("EFF-BKG-ORPHAN-BOOK-0"))
            .await;
        assert!(
            matches!(orphan, Err(StoreError::EffectNotFound(_))),
            "a rolled-back prepare must leave no intent, got {orphan:?}"
        );
    }

    /// Two operations on the same booking are two effects with two identities.
    #[tokio::test]
    async fn distinct_operations_get_distinct_identities() {
        let temp = TempDir::new().expect("temp dir");
        let repo = repo_in(&temp).await;
        let id = BookingId::new("BKG-DISTINCT");
        seeded(&repo, &id).await;

        let first = repo
            .prepare_effect(prepare_at(&id, 0, "TH-A"))
            .await
            .expect("v0");
        let second = repo
            .prepare_effect(prepare_at(&id, 1, "TH-A"))
            .await
            .expect("v1");

        assert_ne!(
            first.intent.effect_intent_id,
            second.intent.effect_intent_id
        );
        assert!(!second.replayed);
    }

    /// Every place carrying an effect id must carry the same one, and a
    /// disagreement is refused rather than silently rewritten.
    ///
    /// The id is currently duplicated across the canonical plan, the in-flight
    /// state and the aggregate's `active_effect`. Rewriting the caller's values
    /// would hide a coordinator bug; accepting them would let the plan name one
    /// effect while the intent row names another, and fact binding later
    /// compares the plan's id against provider evidence — so the *real*
    /// provider result would be rejected. Fail closed.
    #[tokio::test]
    async fn a_disagreeing_effect_identity_is_refused() {
        let temp = TempDir::new().expect("temp dir");
        let repo = repo_in(&temp).await;
        let id = BookingId::new("BKG-SAMEID");
        seeded(&repo, &id).await;

        let mut request = prepare_at(&id, 0, "TH-A");
        request.canonical_plan = BookingPlan::Book {
            effect_intent_id: EffectIntentId::new("SOME-OTHER-EFFECT"),
            principal: PrincipalId::new("lucy"),
            facts: facts("TH-A"),
        };

        let error = repo
            .prepare_effect(request)
            .await
            .expect_err("a plan naming a different effect must be refused");
        assert!(
            matches!(error, StoreError::InconsistentEffectIdentity { .. }),
            "got {error:?}"
        );
        assert_eq!(
            repo.load(&id).await.expect("load").version,
            0,
            "nothing committed"
        );
    }

    /// A consistent request commits, and recovery — which only has the
    /// aggregate — can resolve the intent from `active_effect`.
    #[tokio::test]
    async fn recovery_can_resolve_the_intent_from_the_aggregate() {
        let temp = TempDir::new().expect("temp dir");
        let repo = repo_in(&temp).await;
        let id = BookingId::new("BKG-RESOLVE");
        seeded(&repo, &id).await;

        let prepared = repo
            .prepare_effect(prepare_at(&id, 0, "TH-A"))
            .await
            .expect("prepare");
        let active = prepared
            .aggregate
            .active_effect
            .clone()
            .expect("active effect");

        assert_eq!(active, prepared.intent.effect_intent_id);
        assert_eq!(
            prepared.aggregate.state.effect_intent_id(),
            Some(&prepared.intent.effect_intent_id),
            "the in-flight state must name the same effect"
        );
        let found = repo
            .load_effect(&active)
            .await
            .expect("recovery must resolve active_effect");
        assert_eq!(found.effect_intent_id, prepared.intent.effect_intent_id);
    }

    /// The deadline is derived by the repository, never taken from the caller
    /// (ADR-016) — a caller that could choose it could set it in the past and
    /// have the council tombstone the effect immediately.
    #[tokio::test]
    async fn the_deadline_is_derived_not_supplied() {
        let temp = TempDir::new().expect("temp dir");
        let repo = SqliteBookingRepository::open_with_ttl(temp.path().join("t.sqlite"), 5_000)
            .await
            .expect("open");
        let id = BookingId::new("BKG-TTL");
        seeded(&repo, &id).await;

        let before = now_ms().expect("clock");
        let prepared = repo
            .prepare_effect(prepare_at(&id, 0, "TH-A"))
            .await
            .expect("prepare");
        let after = now_ms().expect("clock");

        let expiry = prepared.intent.expires_at_ms;
        assert!(
            expiry >= before + 5_000 && expiry <= after + 5_000,
            "expiry {expiry} should be a sample taken during the call plus the 5s TTL, \
             not anything the caller chose"
        );
    }

    /// The stored expiry is read back verbatim, never recomputed — a restart or
    /// clock change must not produce a different deadline for the same identity
    /// (ADR-016).
    #[tokio::test]
    async fn the_expiry_is_stored_and_read_back_unchanged() {
        let temp = TempDir::new().expect("temp dir");
        let id = BookingId::new("BKG-EXPIRY");
        let expected = {
            let repo = repo_in(&temp).await;
            seeded(&repo, &id).await;
            repo.prepare_effect(prepare_at(&id, 0, "TH-A"))
                .await
                .expect("prepare")
                .intent
                .expires_at_ms
        };

        let reopened = repo_in(&temp).await;
        let loaded = reopened
            .load_effect(&EffectIntentId::new("EFF-BKG-EXPIRY-BOOK-0"))
            .await
            .expect("load after restart");
        assert_eq!(loaded.expires_at_ms, expected);
        assert_eq!(loaded.status, EffectStatus::Prepared);
    }
}
