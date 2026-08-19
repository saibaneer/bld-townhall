#![forbid(unsafe_code)]

use async_trait::async_trait;
use bld_types::{
    BookingId, BookingRequirements, BoundedString, CouncilBookingRef, EffectIntentId, Provenance,
    TransitionDriver,
};
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
    Booking, BookingAggregate, BookingEffect, BookingState, Draft, EffectIntent, EffectStatus,
    IncoherentBooking, OperationKind,
};

static MIGRATOR: Migrator = sqlx::migrate!("./migrations");

/// The only outcome an `audit_events` row can carry.
///
/// A row exists because a version advanced, so every one of them is a commit.
/// `Converged` advances nothing and denials live in `denial_events` (ADR-017), so
/// there is no second value to write — and a column that can only say one thing
/// is better than an enum pretending otherwise.
const COMMITTED_OUTCOME: &str = "Committed";

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct NewBooking {
    pub id: BookingId,
    pub requirements: BookingRequirements,
}

/// One committed transition's audit record.
///
/// # Fields are private, and that is the point
///
/// ADR-017 requires the trail's provenance to be **derived from the decision, not
/// asserted by whoever writes the row**. Public fields — or a public enum of
/// driver classes — would let a caller label a proposal-driven commit as
/// fact-driven, which is the same defect one layer along. So there is exactly one
/// constructor, and it takes the *thing that drove the transition*: the type
/// answers which door it came through, and there is no argument through which to
/// lie.
///
/// A row exists here only because a version advanced — `audit_events` has
/// `CHECK (to_version > from_version)` and mints its id from the version bump — so
/// there is no `Converged`, `Denied` or `Undefined` outcome to record. Convergence
/// advances nothing, and denials live in their own table (ADR-017).
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct TransitionAudit {
    provenance: Provenance,
    driver_name: &'static str,
}

impl TransitionAudit {
    /// Record a transition as driven by `driver`.
    #[must_use]
    pub fn driven_by(driver: &impl TransitionDriver) -> Self {
        Self {
            provenance: driver.provenance(),
            driver_name: driver.driver_name(),
        }
    }

    #[must_use]
    pub const fn provenance(&self) -> Provenance {
        self.provenance
    }

    #[must_use]
    pub const fn driver_name(&self) -> &'static str {
        self.driver_name
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
    /// Which provenance class drove this transition (ADR-017).
    pub driver_kind: Provenance,
    /// Which member of that vocabulary.
    pub driver_detail: String,
    pub outcome: String,
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
    #[error("a transition tried to change booking identity from {expected} to {actual}")]
    IdentityChanged {
        expected: BookingId,
        actual: BookingId,
    },
    #[error("{where_} booking contradicts itself: {reason}")]
    IncoherentBooking {
        where_: &'static str,
        reason: IncoherentBooking,
    },
    #[error(
        "effect {effect_intent_id} belongs to booking {actual_booking}, not {expected_booking}"
    )]
    EffectMismatch {
        effect_intent_id: EffectIntentId,
        expected_booking: BookingId,
        actual_booking: BookingId,
    },
    #[error(
        "the next booking still names effect {effect_intent_id}, which this operation is ending"
    )]
    EffectStillActive { effect_intent_id: EffectIntentId },
    #[error(
        "effect {effect_intent_id} is already {recorded} while the aggregate still waits on it;          a handoff cannot complete from that state"
    )]
    HandoffPredecessorAlreadyFinal {
        effect_intent_id: EffectIntentId,
        recorded: &'static str,
    },
    #[error(
        "a handoff must leave the aggregate naming its successor {expected}, but it names          {actual:?}"
    )]
    SuccessorNotAdopted {
        expected: EffectIntentId,
        actual: Option<EffectIntentId>,
    },
    #[error("{0} is not a terminal outcome; an effect can only be finalised to one that is")]
    NotATerminalStatus(&'static str),
    #[error(
        "effect outcome {status} may not carry {}a provider reference",
        if *.has_reference { "" } else { "no " }
    )]
    InvalidEffectOutcome {
        status: &'static str,
        has_reference: bool,
    },
    #[error(
        "effect {effect_intent_id} is already {recorded}; refusing to record {attempted} — one          identity cannot have two outcomes"
    )]
    ContradictoryFinalisation {
        effect_intent_id: EffectIntentId,
        recorded: &'static str,
        attempted: &'static str,
    },
    #[error("effect intent {0} was not found")]
    EffectNotFound(EffectIntentId),
    #[error("state {state} is waiting on a {state_kind} effect, but the plan is a {plan_kind}")]
    EffectKindMismatch {
        state: &'static str,
        state_kind: &'static str,
        plan_kind: &'static str,
    },
    #[error(
        "an effect prepare must commit a state that is waiting on it, but {state} carries no \
         effect identity (expected {expected})"
    )]
    NotAnInFlightState {
        state: &'static str,
        expected: EffectIntentId,
    },
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
        next: Booking,
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
    /// Record an effect's terminal outcome and commit the resulting state,
    /// atomically. Phase C's ending half.
    ///
    /// Idempotent: recording the *same* outcome again returns the committed
    /// result with `replayed: true` and writes nothing.
    ///
    /// # Errors
    /// [`StoreError::NotATerminalStatus`] for a non-outcome;
    /// [`StoreError::InvalidEffectOutcome`] for a status/reference shape the fact
    /// door would refuse to read; [`StoreError::ContradictoryFinalisation`] when a
    /// different outcome is already recorded — one identity cannot have two;
    /// [`StoreError::NotAnInFlightState`] when the aggregate is not waiting on
    /// this identity, which is what stops a stale intent being finalised while
    /// the live one is orphaned.
    async fn finalize_effect(&self, request: FinalizeEffect)
    -> Result<FinalizedEffect, StoreError>;

    /// Finalise one effect and start its successor, atomically. Phase C's
    /// handoff half.
    ///
    /// Idempotent on the successor's uniqueness key **and** its predecessor: a
    /// same-key successor recorded against a *different* predecessor is a
    /// conflict, not a replay.
    ///
    /// # Errors
    /// Everything `finalize_effect` can return for the finalising half, plus
    /// [`StoreError::ConflictingPlan`] when a same-key successor exists with a
    /// different plan or predecessor.
    async fn handoff_effect(&self, request: HandoffEffect) -> Result<HandedOffEffect, StoreError>;

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
        next: Booking,
        audit: TransitionAudit,
    ) -> Result<BookingAggregate, StoreError> {
        let now = now_ms()?;

        let mut tx = self.pool.begin_with("BEGIN IMMEDIATE").await?;
        let aggregate = commit_in_tx(&mut tx, id, expected_version, &next, &audit, now).await?;
        tx.commit().await?;
        Ok(aggregate)
    }

    async fn prepare_effect(&self, request: PrepareEffect) -> Result<PreparedEffect, StoreError> {
        // Before anything is read or written, and before the replay path can
        // return early. `commit_in_tx` performs the same check, but a replay
        // never reaches it — so without this a retry carrying another booking's
        // value would be answered as though it were valid.
        if request.next.id != request.booking_id {
            return Err(StoreError::IdentityChanged {
                expected: request.booking_id.clone(),
                actual: request.next.id.clone(),
            });
        }

        // Read off the plan, never supplied separately - see PrepareEffect.
        let operation_kind = request.operation_kind();

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
        let effect_intent_id =
            derive_effect_intent_id(&request.booking_id, operation_kind, request.source_version);
        verify_effect_identity(&request, &effect_intent_id, operation_kind)?;

        let next = Booking {
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
                   outcome_detail, supersedes, created_at_ms, updated_at_ms
            FROM effect_intents
            WHERE booking_id = ? AND operation_kind = ? AND source_version = ?
            ",
        )
        .bind(request.booking_id.as_str())
        .bind(operation_kind.name())
        .bind(source_db)
        .fetch_optional(&mut *tx)
        .await?;

        if let Some(row) = existing {
            let replayed = replay_existing(&mut tx, &request, &row).await?;
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

        let effect_intent_id =
            derive_effect_intent_id(&request.booking_id, operation_kind, request.source_version);

        sqlx::query(
            r"
            INSERT INTO effect_intents (
                effect_intent_id, booking_id, operation_kind, source_version,
                canonical_plan_json, status, expires_at_ms, provider_reference,
                last_error, outcome_detail, supersedes, created_at_ms, updated_at_ms
            ) VALUES (?, ?, ?, ?, ?, ?, ?, NULL, NULL, NULL, NULL, ?, ?)
            ",
        )
        .bind(effect_intent_id.as_str())
        .bind(request.booking_id.as_str())
        .bind(operation_kind.name())
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
                operation_kind,
                source_version: request.source_version,
                canonical_plan: request.canonical_plan,
                status: EffectStatus::Prepared,
                expires_at_ms,
                provider_reference: None,
                outcome_detail: None,
                supersedes: None,
                created_at_ms: now,
                updated_at_ms: now,
            },
            replayed: false,
        })
    }

    async fn finalize_effect(
        &self,
        request: FinalizeEffect,
    ) -> Result<FinalizedEffect, StoreError> {
        // Before the transaction, and before the replay path can return early —
        // the same reasoning as `prepare_effect`'s gate.
        if request.next.id != request.booking_id {
            return Err(StoreError::IdentityChanged {
                expected: request.booking_id.clone(),
                actual: request.next.id.clone(),
            });
        }
        // Finalising is what clears the pointer. An aggregate still naming the
        // effect its intent row says has completed is exactly the disagreement
        // `Booking::coherent` exists to prevent, on the one path that could
        // create it.
        if request.next.active_effect.as_ref() == Some(&request.effect_intent_id) {
            return Err(StoreError::EffectStillActive {
                effect_intent_id: request.effect_intent_id.clone(),
            });
        }

        let now = now_ms()?;
        let mut tx = self.pool.begin_with("BEGIN IMMEDIATE").await?;

        let current = load_booking_in_tx(&mut tx, &request.booking_id).await?;
        let classified = classify_finalisation(
            &mut tx,
            &request.booking_id,
            &current,
            &request.effect_intent_id,
            request.status,
            request.provider_reference.as_ref(),
            request.outcome_detail.as_ref(),
        )
        .await?;

        if let FinalisationState::AlreadyRecorded(intent) = classified {
            // The version already advanced when the outcome was first recorded, so
            // the CAS would fail; returning the truth is what lets a coordinator
            // retry safely after a lost acknowledgement.
            tx.commit().await?;
            return Ok(FinalizedEffect {
                aggregate: current,
                intent: *intent,
                replayed: true,
            });
        }

        record_outcome(
            &mut tx,
            &request.effect_intent_id,
            request.status,
            request.provider_reference.as_ref(),
            request.outcome_detail.as_ref(),
            now,
        )
        .await?;

        let aggregate = commit_in_tx(
            &mut tx,
            &request.booking_id,
            request.source_version,
            &request.next,
            &request.audit,
            now,
        )
        .await?;

        let intent = load_effect_in_tx(&mut tx, &request.effect_intent_id).await?;
        tx.commit().await?;

        Ok(FinalizedEffect {
            aggregate,
            intent,
            replayed: false,
        })
    }

    // Four writes plus a replay path share one transaction, and that is the whole
    // point of the operation — splitting it into helpers would scatter the
    // atomicity a reader needs to verify in one place.
    #[allow(clippy::too_many_lines)]
    async fn handoff_effect(&self, request: HandoffEffect) -> Result<HandedOffEffect, StoreError> {
        if request.next.id != request.booking_id {
            return Err(StoreError::IdentityChanged {
                expected: request.booking_id.clone(),
                actual: request.next.id.clone(),
            });
        }

        let successor_kind = request.successor_plan.operation_kind();
        let successor_id =
            derive_effect_intent_id(&request.booking_id, successor_kind, request.source_version);

        // The successor must not reuse the identity it replaces. The domain
        // refuses this too (B3b); defence in depth is cheap and this is the layer
        // that actually writes both rows.
        if successor_id == request.finalising {
            return Err(StoreError::EffectStillActive {
                effect_intent_id: request.finalising.clone(),
            });
        }
        // And the aggregate must name the successor — not merely stop naming the
        // predecessor. An aggregate pointing at neither effect is the
        // unrecoverable shape this operation exists to prevent, because recovery
        // looks up effects by the identity the aggregate names.
        if request.next.active_effect.as_ref() != Some(&successor_id) {
            return Err(StoreError::SuccessorNotAdopted {
                expected: successor_id,
                actual: request.next.active_effect.clone(),
            });
        }

        let plan_json = serde_json::to_string(&request.successor_plan)?;
        let source_db = version_to_i64(request.source_version)?;

        // ADR-016, same discipline as `prepare_effect`: sampled once, immediately
        // before the transaction, never chosen by a caller.
        let prepared_at_ms = now_ms()?;
        let expires_at_ms = prepared_at_ms
            .checked_add(self.effect_ttl_ms)
            .ok_or(StoreError::ClockOutOfRange)?;
        let now = prepared_at_ms;

        let mut tx = self.pool.begin_with("BEGIN IMMEDIATE").await?;

        // Replay first, exactly as `prepare_effect` does: if the successor already
        // exists the version has advanced and the CAS would fail, stranding a
        // coordinator with a handoff it cannot resume.
        let existing = sqlx::query(
            r"
            SELECT effect_intent_id, booking_id, operation_kind, source_version,
                   canonical_plan_json, status, expires_at_ms, provider_reference,
                   outcome_detail, supersedes, created_at_ms, updated_at_ms
            FROM effect_intents
            WHERE effect_intent_id = ?
            ",
        )
        .bind(successor_id.as_str())
        .fetch_optional(&mut *tx)
        .await?;

        if let Some(row) = existing {
            let successor = decode_effect_row(&row)?;
            // The uniqueness key names the successor only, so it cannot on its own
            // distinguish "this exact handoff already happened" from "a different
            // predecessor produced a same-key successor". `supersedes` is what
            // makes the whole tuple checkable.
            if successor.canonical_plan != request.successor_plan
                || successor.supersedes.as_ref() != Some(&request.finalising)
            {
                return Err(StoreError::ConflictingPlan {
                    booking_id: request.booking_id.clone(),
                    operation_kind: successor_kind.name(),
                    source_version: request.source_version,
                });
            }
            let finalised = load_effect_in_tx(&mut tx, &request.finalising).await?;
            let aggregate = load_booking_in_tx(&mut tx, &request.booking_id).await?;
            tx.commit().await?;
            return Ok(HandedOffEffect {
                aggregate,
                finalised,
                successor,
                replayed: true,
            });
        }

        let current = load_booking_in_tx(&mut tx, &request.booking_id).await?;
        let classified = classify_finalisation(
            &mut tx,
            &request.booking_id,
            &current,
            &request.finalising,
            request.finalising_status,
            request.finalising_reference.as_ref(),
            request.finalising_detail.as_ref(),
        )
        .await?;

        // The successor branch above is the only replay path. Reaching here with
        // an already-terminal predecessor means it was finalised while the
        // aggregate stayed pointed at it — which this operation and
        // `finalize_effect` both commit atomically, so it cannot happen honestly.
        // Refuse with a reason rather than proceed on a broken premise.
        if let FinalisationState::AlreadyRecorded(intent) = classified {
            return Err(StoreError::HandoffPredecessorAlreadyFinal {
                effect_intent_id: request.finalising.clone(),
                recorded: intent.status.name(),
            });
        }

        record_outcome(
            &mut tx,
            &request.finalising,
            request.finalising_status,
            request.finalising_reference.as_ref(),
            request.finalising_detail.as_ref(),
            now,
        )
        .await?;

        let aggregate = commit_in_tx(
            &mut tx,
            &request.booking_id,
            request.source_version,
            &request.next,
            &request.audit,
            now,
        )
        .await?;

        sqlx::query(
            r"
            INSERT INTO effect_intents (
                effect_intent_id, booking_id, operation_kind, source_version,
                canonical_plan_json, status, expires_at_ms, provider_reference,
                last_error, outcome_detail, supersedes, created_at_ms, updated_at_ms
            ) VALUES (?, ?, ?, ?, ?, ?, ?, NULL, NULL, NULL, ?, ?, ?)
            ",
        )
        .bind(successor_id.as_str())
        .bind(request.booking_id.as_str())
        .bind(successor_kind.name())
        .bind(source_db)
        .bind(&plan_json)
        .bind(EffectStatus::Prepared.name())
        .bind(expires_at_ms)
        .bind(request.finalising.as_str())
        .bind(now)
        .bind(now)
        .execute(&mut *tx)
        .await?;

        let finalised = load_effect_in_tx(&mut tx, &request.finalising).await?;
        let successor = load_effect_in_tx(&mut tx, &successor_id).await?;
        tx.commit().await?;

        Ok(HandedOffEffect {
            aggregate,
            finalised,
            successor,
            replayed: false,
        })
    }

    async fn load_effect(&self, id: &EffectIntentId) -> Result<EffectIntent, StoreError> {
        let row = sqlx::query(
            r"
            SELECT effect_intent_id, booking_id, operation_kind, source_version,
                   canonical_plan_json, status, expires_at_ms, provider_reference,
                   outcome_detail, supersedes, created_at_ms, updated_at_ms
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
                   from_state, to_state, driver_kind, driver_detail, outcome, created_at_ms
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

    let aggregate = BookingAggregate {
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
    };

    // A row written before this check existed, or edited outside the
    // repository, must not be handed to the domain as if it were sound. Refusing
    // on read is what makes the write-side check an invariant rather than a
    // filter — otherwise every reader would have to re-check.
    if let Err(reason) = Booking::from(&aggregate).coherent() {
        return Err(StoreError::IncoherentBooking {
            where_: "a persisted",
            reason,
        });
    }

    Ok(aggregate)
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
        driver_kind: Provenance::parse(&row.try_get::<String, _>("driver_kind")?)
            .map_err(|text| StoreError::CorruptRow(format!("unknown driver_kind {text:?}")))?,
        driver_detail: row.try_get("driver_detail")?,
        outcome: row.try_get("outcome")?,
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
/// Everything a value must satisfy before any part of it is written.
///
/// Both checks are about the value itself rather than the transition, so they
/// run before the transaction does any work — a refusal here has touched
/// nothing.
///
/// A transition changes a booking; it never changes *which* booking. The domain
/// carries `id` so evidence can be bound to a resource (ADR-012), and a carried
/// field is one a future arm could rebuild wrongly, so the value that arrives is
/// verified against the row being written rather than trusted.
///
/// Coherence is the domain's judgement, not this layer's: some facts live in two
/// places — `active_effect` beside the in-flight state's own copy, and so on —
/// and the domain says whether they agree. Refusing here is what lets every
/// transition arm carry those fields through instead of defensively re-deriving
/// them in each of eight places.
fn admissible(id: &BookingId, next: &Booking) -> Result<(), StoreError> {
    if next.id != *id {
        return Err(StoreError::IdentityChanged {
            expected: id.clone(),
            actual: next.id.clone(),
        });
    }
    next.coherent()
        .map_err(|reason| StoreError::IncoherentBooking {
            where_: "a committed",
            reason,
        })
}

async fn commit_in_tx(
    tx: &mut sqlx::Transaction<'_, sqlx::Sqlite>,
    id: &BookingId,
    expected_version: u64,
    next: &Booking,
    audit: &TransitionAudit,
    now: i64,
) -> Result<BookingAggregate, StoreError> {
    admissible(id, next)?;

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
            from_state, to_state, driver_kind, driver_detail, outcome, created_at_ms
        ) VALUES (?, ?, ?, ?, ?, ?, ?, ?, ?, ?)
        ",
    )
    .bind(&event_id)
    .bind(id.as_str())
    .bind(expected_db)
    .bind(next_db)
    .bind(&from_state)
    .bind(next.state.name())
    .bind(audit.provenance().name())
    .bind(audit.driver_name())
    .bind(COMMITTED_OUTCOME)
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
    use bld_types::{Money, PrincipalId, Provenance, SlotId, TimeWindow, VenueId};
    use tempfile::TempDir;
    use townhall_domain::{
        BookingProposal, BookingState, SelectedVenueRef, VenueFacts, VenueSelected,
    };

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
                Booking {
                    id: id.clone(),
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
                TransitionAudit::driven_by(&BookingProposal::SelectVenue {
                    venue_id: VenueId::new("TH-A"),
                    slot_id: SlotId::new("SLOT-A"),
                }),
            )
            .await
            .expect("first CAS should win");

        assert_eq!(committed.version, 1);

        let error = repo
            .commit(
                &id,
                stale_copy_b.version,
                Booking::from(&stale_copy_b),
                TransitionAudit::driven_by(&BookingProposal::Cancel {
                    reason: "changed mind".to_owned(),
                }),
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

        let next = Booking {
            id: id.clone(),
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
            TransitionAudit::driven_by(&BookingProposal::SelectVenue {
                venue_id: VenueId::new("TH-A"),
                slot_id: SlotId::new("SLOT-A"),
            }),
        )
        .await
        .expect("commit should succeed");

        let audit = repo.audit_events(&id).await.expect("audit read");
        assert_eq!(audit.len(), 1);
        assert_eq!(audit[0].from_version, 0);
        assert_eq!(audit[0].to_version, 1);
        assert_eq!(audit[0].from_state, "Draft");
        assert_eq!(audit[0].to_state, "VenueSelected");
        // The provenance is DERIVED: the row says a proposal drove this because a
        // `BookingProposal` was what built the record, not because a caller typed it.
        assert_eq!(audit[0].driver_kind, Provenance::Proposal);
        assert_eq!(audit[0].driver_detail, "SelectVenue");
    }

    /// B3a put `id` on the domain's `Booking` so evidence can be bound to a
    /// resource. A carried field is one a future transition could rebuild
    /// wrongly, so the repository verifies it rather than trusting it — without
    /// this check the field would be decorative, and a mis-assembled transition
    /// could write one booking's state over another's row.
    #[tokio::test]
    async fn a_transition_may_not_change_which_booking_it_is() {
        let temp = TempDir::new().expect("temp dir");
        let repo = repo_in(&temp).await;
        let id = BookingId::new("BKG-IDENTITY");
        let created = repo
            .create(NewBooking {
                id: id.clone(),
                requirements: requirements(),
            })
            .await
            .expect("create should succeed");

        // Coherent in every other respect, so the identity is the only thing
        // that can refuse it. A fixture with two defects proves whichever check
        // happens to run first, not the one it names.
        let impostor = Booking {
            id: BookingId::new("BKG-SOMEONE-ELSE"),
            state: BookingState::VenueSelected(VenueSelected {
                venue_id: VenueId::new("TH-A"),
                slot_id: SlotId::new("SLOT-A"),
            }),
            selected_venue: Some(SelectedVenueRef {
                venue_id: VenueId::new("TH-A"),
                slot_id: SlotId::new("SLOT-A"),
            }),
            ..Booking::from(&created)
        };
        impostor
            .coherent()
            .expect("the fixture must carry exactly one defect");

        let error = repo
            .commit(
                &id,
                0,
                impostor,
                TransitionAudit::driven_by(&BookingProposal::SelectVenue {
                    venue_id: VenueId::new("TH-A"),
                    slot_id: SlotId::new("SLOT-A"),
                }),
            )
            .await
            .expect_err("a transition that changes identity must be refused");

        assert!(
            matches!(
                error,
                StoreError::IdentityChanged {
                    ref expected,
                    ref actual,
                } if expected == &id && actual == &BookingId::new("BKG-SOMEONE-ELSE")
            ),
            "expected IdentityChanged, got {error:?}"
        );

        let untouched = repo.load(&id).await.expect("the row must survive");
        assert_eq!(untouched.version, 0, "nothing may have been written");
        assert_eq!(untouched.state.name(), "Draft");
    }

    /// The aggregate records the in-flight effect twice: inside the state, and
    /// in `active_effect`. B3a created that second copy, so B3a owes the
    /// invariant that they cannot be persisted disagreeing — otherwise recovery
    /// reads one value while the state means another.
    #[tokio::test]
    async fn a_booking_whose_two_effect_pointers_disagree_cannot_be_committed() {
        let temp = TempDir::new().expect("temp dir");
        let repo = repo_in(&temp).await;
        let id = BookingId::new("BKG-INCOHERENT");
        let created = repo
            .create(NewBooking {
                id: id.clone(),
                requirements: requirements(),
            })
            .await
            .expect("create should succeed");

        let contradictory = Booking {
            state: BookingState::BookingInProgress(townhall_domain::BookingInProgress {
                effect_intent_id: EffectIntentId::new("EFF-ONE"),
            }),
            active_effect: Some(EffectIntentId::new("EFF-TWO")),
            ..Booking::from(&created)
        };

        let error = repo
            .commit(
                &id,
                0,
                contradictory,
                TransitionAudit::driven_by(&BookingProposal::Book),
            )
            .await
            .expect_err("a self-contradictory booking must not be persisted");

        assert!(
            matches!(
                error,
                StoreError::IncoherentBooking {
                    reason: IncoherentBooking::EffectIdentity { .. },
                    ..
                }
            ),
            "expected an effect-identity disagreement, got {error:?}"
        );

        let untouched = repo.load(&id).await.expect("the row must survive");
        assert_eq!(untouched.version, 0, "nothing may have been written");
    }

    /// Refusing on write is only half of it. A row edited outside the repository
    /// — or written before the check existed — must not be handed to the domain
    /// as though it were sound, or every reader would have to re-check.
    #[tokio::test]
    async fn a_persisted_booking_that_contradicts_itself_fails_to_load() {
        let temp = TempDir::new().expect("temp dir");
        let repo = repo_in(&temp).await;
        let id = BookingId::new("BKG-CORRUPT");
        repo.create(NewBooking {
            id: id.clone(),
            requirements: requirements(),
        })
        .await
        .expect("create should succeed");

        // Straight past the repository's own API, which is the only way such a
        // row can come into being now.
        let state = BookingState::BookingInProgress(townhall_domain::BookingInProgress {
            effect_intent_id: EffectIntentId::new("EFF-ONE"),
        });
        sqlx::query(
            "UPDATE bookings SET state_name = ?, state_json = ?, active_effect = ? WHERE id = ?",
        )
        .bind(state.name())
        .bind(serde_json::to_string(&state).expect("state should serialise"))
        .bind("EFF-TWO")
        .bind(id.as_str())
        .execute(repo.pool())
        .await
        .expect("the corrupt row should be written");

        let error = repo
            .load(&id)
            .await
            .expect_err("a self-contradictory row must not load");

        assert!(
            matches!(
                error,
                StoreError::IncoherentBooking {
                    reason: IncoherentBooking::EffectIdentity { .. },
                    ..
                }
            ),
            "expected an effect-identity disagreement, got {error:?}"
        );
    }

    /// `BookingEffect::Book` gained `attendees` in B3b (ADR-012: fee and
    /// headcount are not optional in the evidence binding). Old rows in
    /// `effect_intents.canonical_plan_json` lack the field and must FAIL to
    /// decode — a `#[serde(default)]` would hide the change behind an
    /// `attendees: 0` that binds against nothing. Any dev database written
    /// before this commit must be recreated; the failure mode is a loud decode
    /// error at load, not a silent mis-bind.
    #[test]
    fn a_book_plan_without_attendees_is_deliberately_rejected() {
        let legacy = r#"{"Book":{"principal":"lucy","facts":{"venue_id":"TH-A","slot_id":"SLOT-1","capacity":30,"wheelchair_accessible":true,"fee":{"pence":4500},"available":true}}}"#;
        assert!(
            serde_json::from_str::<BookingEffect>(legacy).is_err(),
            "the pre-attendees wire shape must fail closed"
        );

        // And the current shape round-trips, so this test cannot rot into
        // rejecting everything.
        let current = BookingEffect::Book {
            principal: PrincipalId::new("lucy"),
            attendees: 20,
            facts: VenueFacts {
                venue_id: VenueId::new("TH-A"),
                slot_id: SlotId::new("SLOT-1"),
                capacity: 30,
                wheelchair_accessible: true,
                fee: Money::from_pence(4_500),
                available: true,
            },
        };
        let json = serde_json::to_string(&current).expect("serialise");
        assert_eq!(
            serde_json::from_str::<BookingEffect>(&json).expect("decode"),
            current
        );
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
    use townhall_domain::{BookingProposal, BookingState, SelectedVenueRef, VenueSelected};

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

    fn write_for(
        id: &BookingId,
        venue: &str,
        slot: &str,
        requirements: BookingRequirements,
    ) -> Booking {
        Booking {
            id: id.clone(),
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
                let write = write_for(&id, venue, "SLOT-1", created.requirements.clone());
                let expected = created.version;

                handles.push(tokio::spawn(async move {
                    // All setup is done; align the two tasks precisely here.
                    barrier.wait().await;
                    repo.commit(
                        &id,
                        expected,
                        write,
                        TransitionAudit::driven_by(&BookingProposal::SelectVenue {
                            venue_id: VenueId::new("TH-A"),
                            slot_id: SlotId::new("SLOT-A"),
                        }),
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
            let write = write_for(&id, "TH-A", "SLOT-1", requirements());
            handles.push(tokio::spawn(async move {
                barrier.wait().await;
                repo.commit(
                    &id,
                    0,
                    write,
                    TransitionAudit::driven_by(&BookingProposal::SelectVenue {
                        venue_id: VenueId::new("TH-A"),
                        slot_id: SlotId::new("SLOT-A"),
                    }),
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
    /// The aggregate version this effect is derived from. Also the CAS
    /// expectation, and part of the uniqueness key.
    pub source_version: u64,
    pub canonical_plan: BookingEffect,
    /// The state to commit alongside the intent.
    ///
    /// `active_effect` on this write is **ignored**: the repository owns the
    /// effect identity and sets it, so the aggregate and the intent row cannot
    /// disagree. See [`BookingRepository::prepare_effect`].
    pub next: Booking,
    pub audit: TransitionAudit,
}

impl PrepareEffect {
    /// Which kind of consequence this is — read off the plan, never supplied
    /// separately.
    ///
    /// It used to be its own field, which meant a coordinator bug could persist
    /// a `CancelBooking` plan under `OperationKind::Book`: wrong uniqueness key,
    /// wrong persisted kind, and recovery later dispatching the effect as the
    /// wrong class. Deriving it removes the possibility rather than guarding
    /// against it.
    fn operation_kind(&self) -> OperationKind {
        self.canonical_plan.operation_kind()
    }
}

/// How long an effect intent may be acted on, from the instant Phase A is
/// prepared.
///
/// ADR-016: the deadline is *derived*, never supplied by a caller. A caller
/// that could choose it could set it in the past and have the council tombstone
/// the effect immediately, or far in the future and extend the creation window
/// past policy.
pub const DEFAULT_EFFECT_TTL_MS: i64 = 30_000;

/// Record an effect's terminal outcome and commit the state it produces.
///
/// Phase C's ending half (`docs/m4-effects-guidance.md`). One transaction: the
/// aggregate CAS, the effect's terminal status, and the audit row. Returns
/// *committed* state, so — like [`PrepareEffect`] — there is no signature through
/// which a capability could be invoked while a transaction is open.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct FinalizeEffect {
    pub booking_id: BookingId,
    /// The version the classification was derived from. Also the CAS expectation.
    pub source_version: u64,
    /// The effect ending.
    pub effect_intent_id: EffectIntentId,
    pub status: EffectStatus,
    pub provider_reference: Option<CouncilBookingRef>,
    /// Why, where the outcome has a reason worth keeping.
    pub outcome_detail: Option<BoundedString>,
    /// The complete next booking the domain decided.
    pub next: Booking,
    pub audit: TransitionAudit,
}

/// Replace one effect with another, atomically.
///
/// For the single transition that hands off rather than ends:
/// `CancellationRequested + BookingExists -> CancellingBooking` finalises the
/// booking intent *and* starts a cancellation.
///
/// This exists because doing it as finalise-then-prepare leaves a crash window
/// whose halves are both unrecoverable: either a booking intent nobody will
/// finalise, or a `CancellingBooking` naming a cancellation intent that does not
/// exist — and recovery looks up effects by the identity the aggregate names.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct HandoffEffect {
    pub booking_id: BookingId,
    pub source_version: u64,
    /// The effect ending.
    pub finalising: EffectIntentId,
    pub finalising_status: EffectStatus,
    pub finalising_reference: Option<CouncilBookingRef>,
    pub finalising_detail: Option<BoundedString>,
    /// The successor's canonical plan. Its **identity is derived here**, exactly
    /// as `prepare_effect` derives one: the repository owns effect identity
    /// because it holds the uniqueness key.
    pub successor_plan: BookingEffect,
    pub next: Booking,
    pub audit: TransitionAudit,
}

/// The committed result of [`BookingRepository::finalize_effect`].
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct FinalizedEffect {
    pub aggregate: BookingAggregate,
    pub intent: EffectIntent,
    /// True when this call found the outcome already recorded and wrote nothing.
    pub replayed: bool,
}

/// The committed result of [`BookingRepository::handoff_effect`].
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct HandedOffEffect {
    pub aggregate: BookingAggregate,
    /// The effect that ended.
    pub finalised: EffectIntent,
    /// The effect that replaced it.
    pub successor: EffectIntent,
    pub replayed: bool,
}

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

/// Read one effect intent inside an open transaction.
async fn load_effect_in_tx(
    tx: &mut sqlx::Transaction<'_, sqlx::Sqlite>,
    id: &EffectIntentId,
) -> Result<EffectIntent, StoreError> {
    let row = sqlx::query(
        r"
        SELECT effect_intent_id, booking_id, operation_kind, source_version,
               canonical_plan_json, status, expires_at_ms, provider_reference,
               outcome_detail, supersedes, created_at_ms, updated_at_ms
        FROM effect_intents
        WHERE effect_intent_id = ?
        ",
    )
    .bind(id.as_str())
    .fetch_optional(&mut **tx)
    .await?
    .ok_or_else(|| StoreError::EffectNotFound(id.clone()))?;
    decode_effect_row(&row)
}

/// A terminal outcome's status and reference must be a shape the fact door would
/// accept when it reads the row back (`resolve_fact`'s B4).
///
/// Checking it on the write path means a malformed record can never exist to be
/// read: a `Confirmed` intent with no reference would otherwise converge against
/// *any* provider reference, and a referenceless outcome carrying one would name
/// an effect that officially does not exist.
fn validate_outcome_shape(
    status: EffectStatus,
    provider_reference: Option<&CouncilBookingRef>,
) -> Result<(), StoreError> {
    if !status.is_terminal() {
        return Err(StoreError::NotATerminalStatus(status.name()));
    }
    let has_reference = provider_reference.is_some();
    let valid = match status {
        EffectStatus::Confirmed => has_reference,
        EffectStatus::Absent | EffectStatus::Rejected => !has_reference,
        // Unreachable: is_terminal() already refused these.
        EffectStatus::Prepared | EffectStatus::Unknown => false,
    };
    if valid {
        Ok(())
    } else {
        Err(StoreError::InvalidEffectOutcome {
            status: status.name(),
            has_reference,
        })
    }
}

/// The outcome of asking whether a finalisation has already happened.
enum FinalisationState {
    /// Not yet recorded; proceed. The intent is re-read after the write, so there
    /// is nothing to carry forward from here.
    Fresh,
    /// This exact outcome is already recorded; write nothing.
    AlreadyRecorded(Box<EffectIntent>),
}

/// Load the effect being finalised and decide whether this is fresh or a replay.
///
/// The aggregate check is the important one, and it is why this takes the
/// *committed* booking rather than trusting the caller's `next`: without it a
/// caller could finalise an older intent that does belong to this booking while
/// the aggregate is waiting on a different, live one — orphaning the live effect
/// atomically and wrongly. The Phase C mirror of `verify_effect_identity`.
async fn classify_finalisation(
    tx: &mut sqlx::Transaction<'_, sqlx::Sqlite>,
    booking_id: &BookingId,
    current: &BookingAggregate,
    effect_intent_id: &EffectIntentId,
    status: EffectStatus,
    provider_reference: Option<&CouncilBookingRef>,
    outcome_detail: Option<&BoundedString>,
) -> Result<FinalisationState, StoreError> {
    validate_outcome_shape(status, provider_reference)?;

    let row = sqlx::query(
        r"
        SELECT effect_intent_id, booking_id, operation_kind, source_version,
               canonical_plan_json, status, expires_at_ms, provider_reference,
               outcome_detail, supersedes, created_at_ms, updated_at_ms
        FROM effect_intents
        WHERE effect_intent_id = ?
        ",
    )
    .bind(effect_intent_id.as_str())
    .fetch_optional(&mut **tx)
    .await?
    .ok_or_else(|| StoreError::EffectNotFound(effect_intent_id.clone()))?;

    let intent = decode_effect_row(&row)?;

    if intent.booking_id != *booking_id {
        return Err(StoreError::EffectMismatch {
            effect_intent_id: effect_intent_id.clone(),
            expected_booking: booking_id.clone(),
            actual_booking: intent.booking_id,
        });
    }

    // Settled effects are decided by their own record, BEFORE the aggregate is
    // consulted — and the order matters. A successful finalisation moves the
    // aggregate off the effect, so checking the aggregate first would refuse the
    // retry that a lost acknowledgement makes necessary. `prepare_effect` puts
    // its replay lookup first for exactly this reason.
    //
    // This does not reopen the orphaning hole below: reaching a terminal record
    // means there is nothing left to finalise, and neither branch here writes
    // state.
    if intent.status.is_terminal() {
        let identical = intent.status == status
            && intent.provider_reference.as_ref() == provider_reference
            && intent.outcome_detail.as_ref() == outcome_detail;
        return if identical {
            Ok(FinalisationState::AlreadyRecorded(Box::new(intent)))
        } else {
            Err(StoreError::ContradictoryFinalisation {
                effect_intent_id: effect_intent_id.clone(),
                recorded: intent.status.name(),
                attempted: status.name(),
            })
        };
    }

    // Still live, so this is a real finalisation — and the aggregate must be
    // waiting on THIS identity, and on this kind of work. Without it a caller
    // could finalise an effect that is merely *stale* while the aggregate waits
    // on a different, live one, orphaning the live effect atomically and wrongly.
    match current.state.effect_intent_id() {
        Some(waiting_on) if waiting_on == effect_intent_id => {}
        _ => {
            return Err(StoreError::NotAnInFlightState {
                state: current.state.name(),
                expected: effect_intent_id.clone(),
            });
        }
    }
    if current.state.in_flight_kind() != Some(intent.operation_kind) {
        return Err(StoreError::EffectKindMismatch {
            state: current.state.name(),
            state_kind: current
                .state
                .in_flight_kind()
                .map_or("nothing", OperationKind::name),
            plan_kind: intent.operation_kind.name(),
        });
    }

    Ok(FinalisationState::Fresh)
}

/// Write an effect's terminal outcome.
async fn record_outcome(
    tx: &mut sqlx::Transaction<'_, sqlx::Sqlite>,
    effect_intent_id: &EffectIntentId,
    status: EffectStatus,
    provider_reference: Option<&CouncilBookingRef>,
    outcome_detail: Option<&BoundedString>,
    now: i64,
) -> Result<(), StoreError> {
    sqlx::query(
        r"
        UPDATE effect_intents
        SET status = ?, provider_reference = ?, outcome_detail = ?, updated_at_ms = ?
        WHERE effect_intent_id = ?
        ",
    )
    .bind(status.name())
    .bind(provider_reference.map(ToString::to_string))
    .bind(outcome_detail.map(|detail| detail.as_str().to_owned()))
    .bind(now)
    .bind(effect_intent_id.as_str())
    .execute(&mut **tx)
    .await?;
    Ok(())
}

fn decode_effect_row(row: &sqlx::sqlite::SqliteRow) -> Result<EffectIntent, StoreError> {
    let kind_text: String = row.try_get("operation_kind")?;
    let status_text: String = row.try_get("status")?;
    let plan_json: String = row.try_get("canonical_plan_json")?;
    let provider_reference: Option<String> = row.try_get("provider_reference")?;
    let outcome_detail: Option<String> = row.try_get("outcome_detail")?;
    let supersedes: Option<String> = row.try_get("supersedes")?;

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
        // Stored text is already bounded; `truncating` is the only constructor
        // and re-applying it to an in-range value is the identity.
        outcome_detail: outcome_detail.map(BoundedString::truncating),
        supersedes: supersedes.map(EffectIntentId::new),
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
) -> Result<PreparedEffect, StoreError> {
    let operation_kind = request.operation_kind();
    let intent = decode_effect_row(row)?;

    // Same operation key, different plan, is not a retry: two different
    // consequences are competing for one identity. Fail closed rather than
    // pick one.
    //
    // Compared as values, not as JSON strings. String equality only ever
    // worked because serde's field order happens to be stable, and it would
    // report ConflictingPlan for a semantically identical plan the moment the
    // shape evolves — decode_effect_row has already parsed the stored plan, so
    // the honest comparison is free.
    if intent.canonical_plan != request.canonical_plan {
        return Err(StoreError::ConflictingPlan {
            booking_id: request.booking_id.clone(),
            operation_kind: operation_kind.name(),
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

/// The in-flight state must carry *this* effect id, and must carry one.
///
/// Slice A had to check two places, because `BookingPlan::Book` carried its own
/// copy of the id. B2 removed that field, so the canonical plan no longer names
/// an effect at all.
///
/// What this checks is the *state's* copy against the identity being prepared.
/// The aggregate's `active_effect` is the third copy, and it is not checked here
/// because [`Booking::coherent`] already refuses any booking whose state and
/// `active_effect` disagree, on write and on read alike.
///
/// Two ways to get this wrong, both closed here:
///
/// - a **different** id means the aggregate would point at one effect while the
///   intent row records another, and recovery reads the aggregate;
/// - **no** id means `active_effect` would say an effect is running beside a
///   state recording nothing in flight, which recovery cannot resolve either way.
///
/// Rewriting the caller's value instead would hide a coordinator bug rather than
/// surface it.
fn verify_effect_identity(
    request: &PrepareEffect,
    expected: &EffectIntentId,
    operation_kind: OperationKind,
) -> Result<(), StoreError> {
    // Identity is not enough. An effect id does not encode its kind, so a `Book`
    // plan committed alongside a `CancellingBooking` state would pass an id
    // comparison while being nonsense — and recovery would then dispatch the
    // effect as the wrong class.
    match request.next.state.in_flight_kind() {
        Some(kind) if kind == operation_kind => {}
        Some(kind) => {
            return Err(StoreError::EffectKindMismatch {
                state: request.next.state.name(),
                state_kind: kind.name(),
                plan_kind: operation_kind.name(),
            });
        }
        None => {}
    }

    // The state's own copy. Slice A also had to check the canonical plan, because
    // `BookingPlan::Book` carried its own copy of the id — B2 removed that field.
    // `active_effect` is covered separately and unconditionally by
    // `Booking::coherent`, so it needs no check here.
    //
    // Absence is a failure, not a pass. `prepare_effect` only ever commits an
    // external effect, so the state it commits *must* be one that is waiting on
    // that effect. Merely verifying the id when present would let a `Booked` or
    // `Draft` through — and then `active_effect` would say an effect is running
    // while the state says nothing is, which is precisely the inconsistency
    // recovery has no way to resolve.
    match request.next.state.effect_intent_id() {
        Some(found) if found == expected => Ok(()),
        Some(found) => Err(StoreError::InconsistentEffectIdentity {
            where_: "in-flight state",
            found: found.clone(),
            expected: expected.clone(),
        }),
        None => Err(StoreError::NotAnInFlightState {
            state: request.next.state.name(),
            expected: expected.clone(),
        }),
    }
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
    use townhall_domain::{BookingProposal, BookingState, SelectedVenueRef, VenueFacts};

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

    fn plan_for(venue: &str) -> BookingEffect {
        BookingEffect::Book {
            principal: PrincipalId::new("lucy"),
            attendees: 20,
            facts: facts(venue),
        }
    }

    fn in_progress_write(id: &BookingId, effect: &EffectIntentId) -> Booking {
        Booking {
            id: id.clone(),
            state: BookingState::BookingInProgress(townhall_domain::BookingInProgress {
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
            source_version: version,
            canonical_plan: plan_for(venue),
            next: in_progress_write(id, &effect),
            audit: TransitionAudit::driven_by(&BookingProposal::Book),
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
    /// The replay path returns before `commit_in_tx`, so it needs its own gate.
    /// Without one, a retry that named a different booking in its write value
    /// would be answered as a successful replay.
    #[tokio::test]
    async fn a_retry_naming_a_different_booking_is_refused_on_the_replay_path() {
        let temp = TempDir::new().expect("temp dir");
        let repo = repo_in(&temp).await;
        let id = BookingId::new("BKG-REPLAYID");
        seeded(&repo, &id).await;

        repo.prepare_effect(prepare_at(&id, 0, "TH-A"))
            .await
            .expect("the first prepare should succeed");

        let mut retry = prepare_at(&id, 0, "TH-A");
        retry.next.id = BookingId::new("BKG-ELSEWHERE");

        let error = repo
            .prepare_effect(retry)
            .await
            .expect_err("a retry naming another booking must be refused");

        assert!(
            matches!(error, StoreError::IdentityChanged { .. }),
            "expected IdentityChanged on the replay path, got {error:?}"
        );
    }

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
            Booking {
                id: id.clone(),
                state: BookingState::Draft(townhall_domain::Draft),
                requirements: requirements(),
                selected_venue: None,
                availability: None,
                booking_ref: None,
                active_effect: None,
            },
            TransitionAudit::driven_by(&BookingProposal::ChangeVenue),
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

    /// The in-flight state must carry the same effect id the repository
    /// derives, and a disagreement is refused rather than silently rewritten.
    ///
    /// Rewriting would hide a coordinator bug; accepting would leave the
    /// aggregate pointing at one effect while the intent row records another,
    /// and recovery reads the aggregate. Fail closed.
    #[tokio::test]
    async fn a_disagreeing_effect_identity_is_refused() {
        let temp = TempDir::new().expect("temp dir");
        let repo = repo_in(&temp).await;
        let id = BookingId::new("BKG-SAMEID");
        seeded(&repo, &id).await;

        let mut request = prepare_at(&id, 0, "TH-A");
        // The plan no longer carries an effect id at all (B2 removed the field),
        // so a disagreement can only come from the in-flight state now.
        request.next.state = BookingState::BookingInProgress(townhall_domain::BookingInProgress {
            effect_intent_id: EffectIntentId::new("SOME-OTHER-EFFECT"),
        });

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

    /// An effect prepare whose state is not waiting on that effect is refused.
    ///
    /// Otherwise the aggregate would persist `active_effect = Some(..)` beside a
    /// state that records nothing in flight — recovery reads the state, finds no
    /// effect to resume, and the intent is stranded.
    #[tokio::test]
    async fn a_prepare_whose_state_is_not_in_flight_is_refused() {
        let temp = TempDir::new().expect("temp dir");
        let repo = repo_in(&temp).await;
        let id = BookingId::new("BKG-NOTINFLIGHT");
        seeded(&repo, &id).await;

        let mut request = prepare_at(&id, 0, "TH-A");
        request.next.state = BookingState::Booked(townhall_domain::Booked {
            booking_ref: CouncilBookingRef::new("TH-92718"),
        });

        let error = repo
            .prepare_effect(request)
            .await
            .expect_err("a Booked state carries no effect identity");
        assert!(
            matches!(error, StoreError::NotAnInFlightState { .. }),
            "got {error:?}"
        );
        assert_eq!(
            repo.load(&id).await.expect("load").version,
            0,
            "nothing committed"
        );
    }

    /// A state waiting on one kind of effect cannot be committed with a plan of
    /// the other kind, even when the identity lines up.
    #[tokio::test]
    async fn a_state_and_plan_of_different_kinds_are_refused() {
        let temp = TempDir::new().expect("temp dir");
        let repo = repo_in(&temp).await;
        let id = BookingId::new("BKG-KINDMIX");
        seeded(&repo, &id).await;

        // A cancellation plan, but committed alongside a state that is waiting
        // for a booking. The id is not even consulted - the kinds disagree.
        let mut request = prepare_at(&id, 0, "TH-A");
        request.canonical_plan = BookingEffect::CancelBooking {
            booking_ref: CouncilBookingRef::new("TH-92718"),
        };

        let error = repo
            .prepare_effect(request)
            .await
            .expect_err("a Cancel plan cannot commit a BookingInProgress state");
        assert!(
            matches!(error, StoreError::EffectKindMismatch { .. }),
            "got {error:?}"
        );
        assert_eq!(
            repo.load(&id).await.expect("load").version,
            0,
            "nothing committed"
        );
    }

    /// A cancellation is a different operation, and must get a different
    /// identity and a different persisted kind.
    ///
    /// Review caught that every test here built a `Book` request, so the whole
    /// cancellation path through `prepare_effect` was unexercised — the kind is
    /// now derived from the plan, and nothing proved that derivation worked for
    /// the other variant.
    #[tokio::test]
    async fn a_cancellation_gets_its_own_kind_and_identity() {
        let temp = TempDir::new().expect("temp dir");
        let repo = repo_in(&temp).await;
        let id = BookingId::new("BKG-CANCELKIND");
        seeded(&repo, &id).await;

        let effect = derive_effect_intent_id(&id, OperationKind::Cancel, 0);
        let prepared = repo
            .prepare_effect(PrepareEffect {
                booking_id: id.clone(),
                source_version: 0,
                canonical_plan: BookingEffect::CancelBooking {
                    booking_ref: CouncilBookingRef::new("TH-92718"),
                },
                next: Booking {
                    id: id.clone(),
                    state: BookingState::CancellingBooking(townhall_domain::CancellingBooking {
                        booking_ref: CouncilBookingRef::new("TH-92718"),
                        effect_intent_id: effect.clone(),
                    }),
                    requirements: requirements(),
                    selected_venue: None,
                    availability: None,
                    booking_ref: Some(CouncilBookingRef::new("TH-92718")),
                    active_effect: Some(effect.clone()),
                },
                audit: TransitionAudit::driven_by(&BookingProposal::Cancel {
                    reason: "changed mind".to_owned(),
                }),
            })
            .await
            .expect("cancellation prepare");

        assert_eq!(prepared.intent.operation_kind, OperationKind::Cancel);
        assert_eq!(prepared.intent.effect_intent_id, effect);
        assert!(
            effect.as_str().contains("CANCEL"),
            "a cancellation identity must be distinguishable from a booking one: {effect}"
        );
        // And a booking on the same resource at the same version is a DIFFERENT
        // effect - two consequences, two identities.
        assert_ne!(effect, derive_effect_intent_id(&id, OperationKind::Book, 0));
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

/// Phase C — the repository half of "observe, validate and converge".
///
/// `prepare_effect` records an intent; these two record what became of it. The
/// pair is what makes `docs/m4-effects-guidance.md`'s Phase C implementable at
/// all: before slice C1 nothing could mark an effect terminal and commit the
/// state it produced in one transaction.
#[cfg(test)]
mod phase_c {
    use super::*;
    use bld_types::{Money, PrincipalId, SlotId, TimeWindow, VenueId};
    use tempfile::TempDir;
    use townhall_domain::{
        AwaitingBooking, Booked, BookingProposal, BookingState, CancellationRequested,
        CancellingBooking, SelectedVenueRef, VenueFacts, VerifiedProviderFact,
    };

    const REF: &str = "TH-92718";

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

    fn facts() -> VenueFacts {
        VenueFacts {
            venue_id: VenueId::new("TH-A"),
            slot_id: SlotId::new("SLOT-A"),
            capacity: 30,
            wheelchair_accessible: true,
            fee: Money::from_pence(4_500),
            available: true,
        }
    }

    fn book_plan() -> BookingEffect {
        BookingEffect::Book {
            principal: PrincipalId::new("lucy"),
            attendees: 20,
            facts: facts(),
        }
    }

    fn selection() -> SelectedVenueRef {
        SelectedVenueRef {
            venue_id: VenueId::new("TH-A"),
            slot_id: SlotId::new("SLOT-A"),
        }
    }

    fn booking_at(
        id: &BookingId,
        state: BookingState,
        booking_ref: Option<&str>,
        active: Option<&EffectIntentId>,
    ) -> Booking {
        let booking = Booking {
            id: id.clone(),
            state,
            requirements: requirements(),
            selected_venue: Some(selection()),
            availability: Some(facts()),
            booking_ref: booking_ref.map(CouncilBookingRef::new),
            active_effect: active.cloned(),
        };
        booking
            .coherent()
            .expect("every fixture must be coherent, or it tests the wrong refusal");
        booking
    }

    fn awaiting(id: &BookingId) -> Booking {
        booking_at(
            id,
            BookingState::AwaitingBooking(AwaitingBooking {
                venue_id: VenueId::new("TH-A"),
                slot_id: SlotId::new("SLOT-A"),
                verified_fee: Money::from_pence(4_500),
            }),
            None,
            None,
        )
    }

    fn in_progress(id: &BookingId, effect: &EffectIntentId) -> Booking {
        booking_at(
            id,
            BookingState::BookingInProgress(townhall_domain::BookingInProgress {
                effect_intent_id: effect.clone(),
            }),
            None,
            Some(effect),
        )
    }

    fn booked(id: &BookingId) -> Booking {
        booking_at(
            id,
            BookingState::Booked(Booked {
                booking_ref: CouncilBookingRef::new(REF),
            }),
            Some(REF),
            None,
        )
    }

    async fn repo_in(temp: &TempDir) -> SqliteBookingRepository {
        SqliteBookingRepository::open(temp.path().join("townhall.sqlite"))
            .await
            .expect("repository should open")
    }

    /// A booking sitting at `BookingInProgress` with its intent durable — the
    /// state every Phase C test starts from.
    async fn in_flight(
        repo: &SqliteBookingRepository,
        id: &BookingId,
    ) -> (BookingAggregate, EffectIntentId) {
        repo.create(NewBooking {
            id: id.clone(),
            requirements: requirements(),
        })
        .await
        .expect("create");

        let effect = derive_effect_intent_id(id, OperationKind::Book, 0);
        let prepared = repo
            .prepare_effect(PrepareEffect {
                booking_id: id.clone(),
                source_version: 0,
                canonical_plan: book_plan(),
                next: in_progress(id, &effect),
                audit: TransitionAudit::driven_by(&BookingProposal::Book),
            })
            .await
            .expect("prepare should succeed");
        (prepared.aggregate, effect)
    }

    fn confirmed_fact(effect: &EffectIntentId) -> VerifiedProviderFact {
        VerifiedProviderFact::BookingExists {
            effect_intent_id: effect.clone(),
            booking_ref: CouncilBookingRef::new(REF),
            venue_id: VenueId::new("TH-A"),
            slot_id: SlotId::new("SLOT-A"),
            attendees: 20,
            fee: Money::from_pence(4_500),
            principal: PrincipalId::new("lucy"),
        }
    }

    fn absent_fact(effect: &EffectIntentId) -> VerifiedProviderFact {
        VerifiedProviderFact::EffectAbsent {
            effect_intent_id: effect.clone(),
        }
    }

    fn finalize(
        id: &BookingId,
        version: u64,
        effect: &EffectIntentId,
        status: EffectStatus,
        reference: Option<&str>,
        next: Booking,
        fact: &VerifiedProviderFact,
    ) -> FinalizeEffect {
        FinalizeEffect {
            booking_id: id.clone(),
            source_version: version,
            effect_intent_id: effect.clone(),
            status,
            provider_reference: reference.map(CouncilBookingRef::new),
            outcome_detail: None,
            next,
            audit: TransitionAudit::driven_by(fact),
        }
    }

    // -------------------------------------------------- finalize_effect

    /// The ordinary confirmation: state, effect status, reference and audit all
    /// committed together, and the audit row records that a FACT drove it — which
    /// before ADR-017 was unrepresentable.
    #[tokio::test]
    async fn a_confirmation_commits_state_status_and_provenance_together() {
        let temp = TempDir::new().expect("temp dir");
        let repo = repo_in(&temp).await;
        let id = BookingId::new("BKG-FINAL");
        let (aggregate, effect) = in_flight(&repo, &id).await;

        let fact = confirmed_fact(&effect);
        let finalised = repo
            .finalize_effect(finalize(
                &id,
                aggregate.version,
                &effect,
                EffectStatus::Confirmed,
                Some(REF),
                booked(&id),
                &fact,
            ))
            .await
            .expect("finalising a confirmed booking should succeed");

        assert!(!finalised.replayed);
        assert_eq!(finalised.aggregate.state.name(), "Booked");
        assert_eq!(
            finalised.aggregate.booking_ref,
            Some(CouncilBookingRef::new(REF))
        );
        assert_eq!(finalised.aggregate.active_effect, None);
        assert_eq!(finalised.intent.status, EffectStatus::Confirmed);
        assert_eq!(
            finalised.intent.provider_reference,
            Some(CouncilBookingRef::new(REF))
        );

        let audit = repo.audit_events(&id).await.expect("audit");
        let last = audit.last().expect("an audit row");
        assert_eq!(last.driver_kind, Provenance::Fact);
        assert_eq!(last.driver_detail, "BookingExists");
        assert_eq!(last.to_state, "Booked");
    }

    /// Recording the same outcome again writes nothing. Asserted by version and
    /// row count, not merely by the flag — a `replayed: true` that still wrote
    /// would pass a flag-only check.
    #[tokio::test]
    async fn re_recording_the_same_outcome_writes_nothing() {
        let temp = TempDir::new().expect("temp dir");
        let repo = repo_in(&temp).await;
        let id = BookingId::new("BKG-REPLAY");
        let (aggregate, effect) = in_flight(&repo, &id).await;
        let fact = confirmed_fact(&effect);

        let first = repo
            .finalize_effect(finalize(
                &id,
                aggregate.version,
                &effect,
                EffectStatus::Confirmed,
                Some(REF),
                booked(&id),
                &fact,
            ))
            .await
            .expect("first finalisation");
        let rows_after_first = repo.audit_events(&id).await.expect("audit").len();

        let again = repo
            .finalize_effect(finalize(
                &id,
                aggregate.version,
                &effect,
                EffectStatus::Confirmed,
                Some(REF),
                booked(&id),
                &fact,
            ))
            .await
            .expect("a retry after a lost acknowledgement must not fail");

        assert!(again.replayed, "the second call must report a replay");
        assert_eq!(again.aggregate.version, first.aggregate.version);
        assert_eq!(
            repo.audit_events(&id).await.expect("audit").len(),
            rows_after_first,
            "a replay must write no audit row"
        );
        assert_eq!(again.intent, first.intent);
    }

    /// One identity cannot have two outcomes. `Confirmed` then `Absent` is not a
    /// retry — it is two contradictory determinations, and picking one would mean
    /// either forgetting a real booking or inventing one.
    #[tokio::test]
    async fn one_identity_cannot_be_finalised_two_ways() {
        let temp = TempDir::new().expect("temp dir");
        let repo = repo_in(&temp).await;
        let id = BookingId::new("BKG-TWOWAYS");
        let (aggregate, effect) = in_flight(&repo, &id).await;

        repo.finalize_effect(finalize(
            &id,
            aggregate.version,
            &effect,
            EffectStatus::Confirmed,
            Some(REF),
            booked(&id),
            &confirmed_fact(&effect),
        ))
        .await
        .expect("first finalisation");

        let error = repo
            .finalize_effect(finalize(
                &id,
                aggregate.version,
                &effect,
                EffectStatus::Absent,
                None,
                awaiting(&id),
                &absent_fact(&effect),
            ))
            .await
            .expect_err("a contradictory outcome must be refused");

        assert!(
            matches!(error, StoreError::ContradictoryFinalisation { .. }),
            "expected ContradictoryFinalisation, got {error:?}"
        );
    }

    /// A non-outcome is not an outcome. `Prepared`/`Unknown` describe an effect
    /// still in play; finalising to one would claim a determination nobody made.
    #[tokio::test]
    async fn a_non_terminal_status_is_not_an_outcome() {
        let temp = TempDir::new().expect("temp dir");
        let repo = repo_in(&temp).await;
        let id = BookingId::new("BKG-NOTTERM");
        let (aggregate, effect) = in_flight(&repo, &id).await;

        for status in [EffectStatus::Prepared, EffectStatus::Unknown] {
            let error = repo
                .finalize_effect(finalize(
                    &id,
                    aggregate.version,
                    &effect,
                    status,
                    None,
                    awaiting(&id),
                    &absent_fact(&effect),
                ))
                .await
                .expect_err("a non-terminal status must be refused");
            assert!(
                matches!(error, StoreError::NotATerminalStatus(_)),
                "expected NotATerminalStatus for {status:?}, got {error:?}"
            );
        }
    }

    /// The status/reference shape must be one the fact door would accept on the
    /// way back in. A `Confirmed` intent with no reference would otherwise
    /// converge against *any* provider reference.
    #[tokio::test]
    async fn a_malformed_outcome_shape_cannot_be_persisted() {
        let temp = TempDir::new().expect("temp dir");
        let repo = repo_in(&temp).await;
        let id = BookingId::new("BKG-SHAPE");
        let (aggregate, effect) = in_flight(&repo, &id).await;

        let cases: &[(EffectStatus, Option<&str>)] = &[
            (EffectStatus::Confirmed, None),
            (EffectStatus::Absent, Some(REF)),
            (EffectStatus::Rejected, Some(REF)),
        ];
        for (status, reference) in cases {
            let error = repo
                .finalize_effect(finalize(
                    &id,
                    aggregate.version,
                    &effect,
                    *status,
                    *reference,
                    awaiting(&id),
                    &absent_fact(&effect),
                ))
                .await
                .expect_err("a malformed outcome shape must be refused");
            assert!(
                matches!(error, StoreError::InvalidEffectOutcome { .. }),
                "expected InvalidEffectOutcome for {status:?}/{reference:?}, got {error:?}"
            );
        }
    }

    /// The orphaning case. A booking waits on E2; a caller finalises the older,
    /// dead E1 — which *does* belong to this booking, and whose `next` correctly
    /// omits it. Without checking the aggregate, the CAS would succeed and E2
    /// would be stranded: nobody left to finalise it, and the council possibly
    /// holding a real booking against it.
    #[tokio::test]
    async fn a_stale_intent_cannot_be_finalised_while_another_is_live() {
        let temp = TempDir::new().expect("temp dir");
        let repo = repo_in(&temp).await;
        let id = BookingId::new("BKG-ORPHANING");
        let (aggregate, e1) = in_flight(&repo, &id).await;

        // E1 fails; the booking returns to AwaitingBooking, then books again as E2.
        let after_absent = repo
            .finalize_effect(finalize(
                &id,
                aggregate.version,
                &e1,
                EffectStatus::Absent,
                None,
                awaiting(&id),
                &absent_fact(&e1),
            ))
            .await
            .expect("first attempt goes absent");

        let e2 = derive_effect_intent_id(&id, OperationKind::Book, after_absent.aggregate.version);
        let second = repo
            .prepare_effect(PrepareEffect {
                booking_id: id.clone(),
                source_version: after_absent.aggregate.version,
                canonical_plan: book_plan(),
                next: in_progress(&id, &e2),
                audit: TransitionAudit::driven_by(&BookingProposal::Book),
            })
            .await
            .expect("second attempt prepares");
        assert_ne!(e1, e2, "a re-proposal must mint a fresh identity");

        // Now finalise E1 again — a different outcome, so the replay path is not
        // what refuses it.
        let error = repo
            .finalize_effect(finalize(
                &id,
                second.aggregate.version,
                &e1,
                EffectStatus::Confirmed,
                Some(REF),
                booked(&id),
                &confirmed_fact(&e1),
            ))
            .await
            .expect_err("finalising a stale intent must be refused");

        assert!(
            matches!(
                error,
                StoreError::ContradictoryFinalisation { .. }
                    | StoreError::NotAnInFlightState { .. }
            ),
            "expected refusal, got {error:?}"
        );

        // E2 must still be live and findable — the whole point.
        let live = repo.load_effect(&e2).await.expect("E2 must still exist");
        assert_eq!(live.status, EffectStatus::Prepared);
        let current = repo.load(&id).await.expect("load");
        assert_eq!(current.active_effect, Some(e2));
    }

    /// The pure form of the orphaning refusal: the stale intent is still **live**,
    /// so the replay and contradiction paths cannot be what refuses it — only the
    /// aggregate check can.
    ///
    /// Reachable because `commit` moves state without consulting effects: a plain
    /// commit off `BookingInProgress` leaves its intent `Prepared` and stale. If
    /// finalising it were then allowed, the aggregate's *live* effect would be
    /// orphaned — no one left to finalise it, and the council possibly holding a
    /// real booking against it.
    #[tokio::test]
    async fn a_live_but_stale_intent_cannot_be_finalised() {
        let temp = TempDir::new().expect("temp dir");
        let repo = repo_in(&temp).await;
        let id = BookingId::new("BKG-LIVESTALE");
        let (aggregate, e1) = in_flight(&repo, &id).await;

        // Move the aggregate off E1 without finalising it, then start E2.
        let moved = repo
            .commit(
                &id,
                aggregate.version,
                awaiting(&id),
                TransitionAudit::driven_by(&BookingProposal::ChangeVenue),
            )
            .await
            .expect("a plain commit does not consult effects");
        let e2 = derive_effect_intent_id(&id, OperationKind::Book, moved.version);
        let second = repo
            .prepare_effect(PrepareEffect {
                booking_id: id.clone(),
                source_version: moved.version,
                canonical_plan: book_plan(),
                next: in_progress(&id, &e2),
                audit: TransitionAudit::driven_by(&BookingProposal::Book),
            })
            .await
            .expect("second attempt prepares");

        assert_eq!(
            repo.load_effect(&e1).await.expect("E1").status,
            EffectStatus::Prepared,
            "E1 must still be live, or this tests the wrong refusal"
        );

        let error = repo
            .finalize_effect(finalize(
                &id,
                second.aggregate.version,
                &e1,
                EffectStatus::Confirmed,
                Some(REF),
                booked(&id),
                &confirmed_fact(&e1),
            ))
            .await
            .expect_err("finalising a live-but-stale intent must be refused");

        assert!(
            matches!(error, StoreError::NotAnInFlightState { .. }),
            "expected NotAnInFlightState, got {error:?}"
        );

        // E2 survives, findable, and still the one the aggregate names.
        assert_eq!(
            repo.load_effect(&e2).await.expect("E2").status,
            EffectStatus::Prepared
        );
        assert_eq!(repo.load(&id).await.expect("load").active_effect, Some(e2));
    }

    /// Finalising is what clears the pointer. A `next` still naming the effect
    /// would leave the aggregate claiming work the intent row says is done.
    #[tokio::test]
    async fn a_next_still_naming_the_finalised_effect_is_refused() {
        let temp = TempDir::new().expect("temp dir");
        let repo = repo_in(&temp).await;
        let id = BookingId::new("BKG-STILLACTIVE");
        let (aggregate, effect) = in_flight(&repo, &id).await;

        let error = repo
            .finalize_effect(finalize(
                &id,
                aggregate.version,
                &effect,
                EffectStatus::Absent,
                None,
                in_progress(&id, &effect),
                &absent_fact(&effect),
            ))
            .await
            .expect_err("a next still naming the effect must be refused");

        assert!(
            matches!(error, StoreError::EffectStillActive { .. }),
            "expected EffectStillActive, got {error:?}"
        );
    }

    /// Two rejections with different reasons must stay distinguishable. Both are
    /// terminal and both are referenceless, so without `outcome_detail` "the hall
    /// is closed" and "the principal is barred" collapse into one row.
    #[tokio::test]
    async fn two_rejections_keep_their_separate_reasons() {
        let temp = TempDir::new().expect("temp dir");
        let repo = repo_in(&temp).await;

        let mut recorded = Vec::new();
        for (index, reason) in ["hall closed for maintenance", "principal is barred"]
            .into_iter()
            .enumerate()
        {
            let id = BookingId::new(format!("BKG-REASON-{index}"));
            let (aggregate, effect) = in_flight(&repo, &id).await;
            let fact = VerifiedProviderFact::ProviderRejected {
                effect_intent_id: effect.clone(),
                reason: BoundedString::truncating(reason),
            };
            repo.finalize_effect(FinalizeEffect {
                booking_id: id.clone(),
                source_version: aggregate.version,
                effect_intent_id: effect.clone(),
                status: EffectStatus::Rejected,
                provider_reference: None,
                outcome_detail: Some(BoundedString::truncating(reason)),
                next: awaiting(&id),
                audit: TransitionAudit::driven_by(&fact),
            })
            .await
            .expect("rejection should finalise");
            recorded.push(
                repo.load_effect(&effect)
                    .await
                    .expect("load")
                    .outcome_detail,
            );
        }

        assert_eq!(
            recorded[0].as_ref().map(BoundedString::as_str),
            Some("hall closed for maintenance")
        );
        assert_eq!(
            recorded[1].as_ref().map(BoundedString::as_str),
            Some("principal is barred")
        );
        assert_ne!(recorded[0], recorded[1]);
    }

    // ---------------------------------------------------- handoff_effect

    /// A cancellation asked for mid-booking, then the booking turns out to
    /// exist. One transaction ends the booking intent and starts the
    /// cancellation — and every part of it lands together.
    #[tokio::test]
    async fn a_handoff_ends_one_effect_and_starts_its_successor_atomically() {
        let temp = TempDir::new().expect("temp dir");
        let repo = repo_in(&temp).await;
        let id = BookingId::new("BKG-HANDOFF");
        let (aggregate, book_effect) = in_flight(&repo, &id).await;

        // Reach CancellationRequested: still waiting on the booking intent.
        let requested = repo
            .commit(
                &id,
                aggregate.version,
                booking_at(
                    &id,
                    BookingState::CancellationRequested(CancellationRequested {
                        effect_intent_id: book_effect.clone(),
                    }),
                    None,
                    Some(&book_effect),
                ),
                TransitionAudit::driven_by(&BookingProposal::Cancel {
                    reason: "changed mind".to_owned(),
                }),
            )
            .await
            .expect("cancellation requested");

        let cancel_effect = derive_effect_intent_id(&id, OperationKind::Cancel, requested.version);
        let fact = confirmed_fact(&book_effect);
        let handed = repo
            .handoff_effect(HandoffEffect {
                booking_id: id.clone(),
                source_version: requested.version,
                finalising: book_effect.clone(),
                finalising_status: EffectStatus::Confirmed,
                finalising_reference: Some(CouncilBookingRef::new(REF)),
                finalising_detail: None,
                successor_plan: BookingEffect::CancelBooking {
                    booking_ref: CouncilBookingRef::new(REF),
                },
                next: booking_at(
                    &id,
                    BookingState::CancellingBooking(CancellingBooking {
                        booking_ref: CouncilBookingRef::new(REF),
                        effect_intent_id: cancel_effect.clone(),
                    }),
                    Some(REF),
                    Some(&cancel_effect),
                ),
                audit: TransitionAudit::driven_by(&fact),
            })
            .await
            .expect("the handoff should succeed");

        assert!(!handed.replayed);
        assert_eq!(handed.aggregate.state.name(), "CancellingBooking");
        assert_eq!(handed.aggregate.active_effect, Some(cancel_effect.clone()));
        // The booking intent ended, with its reference.
        assert_eq!(handed.finalised.status, EffectStatus::Confirmed);
        assert_eq!(
            handed.finalised.provider_reference,
            Some(CouncilBookingRef::new(REF))
        );
        // The cancellation began, and records what it replaced.
        assert_eq!(handed.successor.effect_intent_id, cancel_effect);
        assert_eq!(handed.successor.status, EffectStatus::Prepared);
        assert_eq!(handed.successor.supersedes, Some(book_effect));
        assert_eq!(
            handed.successor.canonical_plan,
            BookingEffect::CancelBooking {
                booking_ref: CouncilBookingRef::new(REF)
            }
        );
        // And the audit row attributes it to the fact, not to Lucy.
        let audit = repo.audit_events(&id).await.expect("audit");
        let last = audit.last().expect("a row");
        assert_eq!(last.driver_kind, Provenance::Fact);
        assert_eq!(last.driver_detail, "BookingExists");
    }

    /// The aggregate must name the successor, not merely stop naming the
    /// predecessor. An aggregate pointing at neither effect is unrecoverable,
    /// because recovery looks effects up by the identity the aggregate names.
    #[tokio::test]
    async fn a_handoff_whose_aggregate_adopts_nothing_is_refused() {
        let temp = TempDir::new().expect("temp dir");
        let repo = repo_in(&temp).await;
        let id = BookingId::new("BKG-ADOPT");
        let (aggregate, book_effect) = in_flight(&repo, &id).await;
        let requested = repo
            .commit(
                &id,
                aggregate.version,
                booking_at(
                    &id,
                    BookingState::CancellationRequested(CancellationRequested {
                        effect_intent_id: book_effect.clone(),
                    }),
                    None,
                    Some(&book_effect),
                ),
                TransitionAudit::driven_by(&BookingProposal::Cancel {
                    reason: "changed mind".to_owned(),
                }),
            )
            .await
            .expect("cancellation requested");

        // A `Booked` next: it clears the predecessor and adopts nothing.
        let error = repo
            .handoff_effect(HandoffEffect {
                booking_id: id.clone(),
                source_version: requested.version,
                finalising: book_effect.clone(),
                finalising_status: EffectStatus::Confirmed,
                finalising_reference: Some(CouncilBookingRef::new(REF)),
                finalising_detail: None,
                successor_plan: BookingEffect::CancelBooking {
                    booking_ref: CouncilBookingRef::new(REF),
                },
                next: booked(&id),
                audit: TransitionAudit::driven_by(&confirmed_fact(&book_effect)),
            })
            .await
            .expect_err("a handoff adopting nothing must be refused");

        assert!(
            matches!(error, StoreError::SuccessorNotAdopted { .. }),
            "expected SuccessorNotAdopted, got {error:?}"
        );
        // Nothing moved.
        let current = repo.load(&id).await.expect("load");
        assert_eq!(current.version, requested.version);
        assert_eq!(current.active_effect, Some(book_effect));
    }
}
