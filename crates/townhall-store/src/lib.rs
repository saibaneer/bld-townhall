#![forbid(unsafe_code)]

use async_trait::async_trait;
use bld_types::{
    BookingId, BookingRequirements, BoundedString, CouncilBookingRef, EffectIntentId, PrincipalId,
    Provenance, TransitionDriver,
};
use sqlx::{
    Row, SqlitePool,
    migrate::Migrator,
    sqlite::{SqliteConnectOptions, SqliteJournalMode, SqlitePoolOptions},
};
use std::{
    path::Path,
    sync::Arc,
    time::{Duration, SystemTime, UNIX_EPOCH},
};
use thiserror::Error;

pub mod authority;
pub mod denials;
use townhall_domain::{
    Booking, BookingAggregate, BookingEffect, BookingState, Draft, EffectIntent, EffectStatus,
    IncoherentBooking, IncoherentIntent, OperationKind,
};

static MIGRATOR: Migrator = sqlx::migrate!("./migrations");

/// The only outcome an `audit_events` row can carry.
///
/// A row exists because a version advanced, so every one of them is a commit.
/// `Converged` advances nothing and denials live in `denial_events` (ADR-017), so
/// there is no second value to write — and a column that can only say one thing
/// is better than an enum pretending otherwise.
const COMMITTED_OUTCOME: &str = "Committed";

/// The menu entry `lookup_cancellable` filters on.
///
/// `proposal_menu()` exports names, not proposals, so matching one means naming
/// it as a string here. That is a drift risk — rename the variant and this
/// silently matches nothing, and every booking becomes uncancellable through the
/// lookup while every test that only checks *exclusion* still passes. Pinned by
/// `the_cancel_menu_name_matches_the_proposal` so the rename fails loudly
/// instead.
const CANCEL_PROPOSAL: &str = "Cancel";

/// The owner every store test's fixture booking belongs to.
///
/// Named rather than inlined so a test that cares about ownership has an obvious
/// second principal to contrast with, and so the fixtures cannot drift apart.
#[cfg(test)]
fn test_owner() -> PrincipalId {
    PrincipalId::new("lucy")
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct NewBooking {
    pub id: BookingId,
    pub requirements: BookingRequirements,
    /// Who the booking belongs to, for visibility.
    ///
    /// Required, not optional, and that is the point: the only way to reach a
    /// NULL `owner_principal` is the migration's backfill of rows that predate
    /// ownership. New rows cannot be written without an owner because this
    /// struct cannot be built without one, so "someone forgot to set it" is not
    /// a reachable state rather than a bug waiting to be found.
    ///
    /// This is the OWNER, which is a different question from the principal an
    /// action is attributed to (ADR-020: booker and canceller need not be the
    /// same person). Nothing here narrows that; attribution still comes from the
    /// authority presented at the proposal.
    pub owner: PrincipalId,
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
    /// The database was written by a pre-ADR-019 build: it holds intents whose
    /// status is `Abandoned`, a value this schema no longer carries. Recovery is
    /// possible by hand — the audit trail's `from_state` on each `NeedsHuman`
    /// transition names the in-flight state exhaustion interrupted — but it is
    /// not automated; see ADR-019 and migration 0004's header.
    #[error(
        "{count} effect intent(s) hold the pre-ADR-019 status 'Abandoned'; this build cannot \
         carry them. Recover the originating states from audit_events (from_state on the \
         NeedsHuman rows) or start from a fresh database. See ADR-019."
    )]
    AbandonedRowsPresent { count: i64 },
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
    #[error("{where_} effect intent contradicts itself: {reason}")]
    IncoherentIntent {
        where_: &'static str,
        reason: IncoherentIntent,
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
    #[error("this is not a coherent handoff: {reason}")]
    IncoherentHandoff { reason: &'static str },
    #[error("{0} is not a terminal outcome; an effect can only be finalised to one that is")]
    NotATerminalStatus(&'static str),
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

    /// Load without regard to ownership.
    ///
    /// For INTERNAL callers only — the reconciler, which runs on a timer and has
    /// no principal to scope by, and the coordinator's own workflow loads, which
    /// happen after admission has already been decided one layer up. Anything
    /// answering an external request wants [`Self::load_visible`].
    async fn load(&self, id: &BookingId) -> Result<BookingAggregate, StoreError>;

    /// Load only if `owner` owns it; otherwise indistinguishable from absent.
    ///
    /// # Why a scoped load rather than a comparison
    ///
    /// The obvious alternative is to `load` and then compare owners at the
    /// facade. That works, and it puts a security decision somewhere a future
    /// edit can forget it, invert it, or short-circuit past it — and the failure
    /// is silent, because a missing check looks exactly like a passing one.
    ///
    /// Here there is no comparison to omit. A row belonging to someone else does
    /// not come back at all, and [`StoreError::NotFound`] already maps to 404 —
    /// so concealment is the default behaviour of the query rather than a step
    /// layered on top of it. Removing the capability beats guarding it.
    ///
    /// The predicate is deliberately positive (`owner_principal = ?`): NULL
    /// legacy rows never match it, for any principal, without needing a
    /// negation that SQL's three-valued logic would quietly get wrong.
    async fn load_visible(
        &self,
        owner: &PrincipalId,
        id: &BookingId,
    ) -> Result<BookingAggregate, StoreError>;

    /// The owner's bookings carrying this council reference.
    ///
    /// Spec §14.1 requires a cancellation to follow an *authoritative resource
    /// lookup*, and the council reference is what a person actually has: it is
    /// the value the confirmation SMS quotes. Conversation memory cannot stand
    /// in for this — it is a routing aid, and it does not survive a restart.
    async fn lookup_by_ref(
        &self,
        owner: &PrincipalId,
        booking_ref: &CouncilBookingRef,
    ) -> Result<Vec<BookingAggregate>, StoreError>;

    /// The owner's bookings that currently offer `Cancel`.
    ///
    /// "Currently offers Cancel" is the authoritative predicate for cancellation
    /// routing — not "is not yet cancelled", which is a different and wronger
    /// question (a booking mid-cancellation is not yet cancelled and must not be
    /// offered again).
    ///
    /// Filtering happens after decode, against the domain's own
    /// `proposal_menu()`. Doing it in SQL would mean hardcoding a list of state
    /// names in the store, which drifts silently the moment the menu changes —
    /// the duplication ADR-018's discipline exists to prevent. The cost is that
    /// every one of a principal's bookings is decoded per lookup, which is
    /// acceptable at the scale a person's own bookings reach.
    async fn lookup_cancellable(
        &self,
        owner: &PrincipalId,
    ) -> Result<Vec<BookingAggregate>, StoreError>;

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
    /// [`StoreError::IncoherentIntent`] when the row the write would produce is one
    /// the read gate would refuse; [`StoreError::ContradictoryFinalisation`] when a
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

    // ------------------------------------------------- the pursuit axis (ADR-019)
    //
    // Facts about OUR chasing of an effect, none of them outcomes: which intents
    // are due attention, who owns a turn, how many calls began and returned, and
    // whether we escalated. The status column is never written here except for
    // the one honest move `Prepared -> Unknown` at the moment a call begins.

    /// Identities due for attention: non-terminal, past their cadence, unleased.
    ///
    /// Identities only — deliberately not intents, so a caller cannot read a
    /// canonical plan out of this and build a fact shaped like it.
    ///
    /// # Errors
    /// [`StoreError::Sqlx`] on a read failure.
    async fn due_effects(&self, limit: u32) -> Result<Vec<EffectIntentId>, StoreError>;

    /// Take exclusive ownership of one intent, re-checking eligibility
    /// atomically — `claim` is the gate, not `due_effects`, so calling this in a
    /// tight loop with a known id cannot pump the budget (`None` when the row is
    /// leased, not yet due, or settled).
    ///
    /// Expiry re-opens a lease rather than locking it away: a crashed owner's
    /// work must be recoverable. What fences the crashed owner is the **token**
    /// — bumped on every claim, carried by every write of the turn — so its late
    /// writes match nothing.
    ///
    /// Every pursuit write below additionally requires the lease to be **held**
    /// (`lease_until_ms` not yet cleared): claiming is the gate in the schema,
    /// not only in this sentence. A never-claimed row refuses its default token,
    /// and a released turn's token writes nothing further. Between a lease's
    /// expiry and the next claim, the token's owner may still finish its own
    /// record — it is the only owner ever issued that token, the counters it
    /// writes are facts about calls that truly happened, and the fence a
    /// takeover needs is the bump, which is atomic with every claim.
    ///
    /// # Errors
    /// [`StoreError::Sqlx`] on a write failure.
    async fn claim_effect(
        &self,
        id: &EffectIntentId,
        lease_ms: i64,
    ) -> Result<Option<ClaimedEffect>, StoreError>;

    /// Record that a provider call is about to begin, under the claimed token.
    ///
    /// This is ADR-014 one level in: the attempt is persisted before it is made,
    /// so a crash mid-call still spent budget and `Prepared` keeps meaning
    /// "never attempted" — the row moves to `Unknown` here, before the wire.
    ///
    /// Returns `false` if the token does not hold the row's lease.
    ///
    /// # Errors
    /// [`StoreError::Sqlx`] on a write failure.
    async fn note_attempt_started(
        &self,
        id: &EffectIntentId,
        token: i64,
    ) -> Result<bool, StoreError>;

    /// Record that the call returned control — answer or not — and how long the
    /// reconciler must wait before asking again. `cadence_ms` is a DURATION;
    /// the store schedules `now + min(cadence_ms, MAX_CADENCE_MS)` under its
    /// own clock. (The first shipped version persisted the duration itself —
    /// a 1970-epoch timestamp, always past, always due — so the retry cadence
    /// never gated anything: one parameter, two meanings, the recurring
    /// defect. ADR-021 records the repair.) Returns `false` if the token does
    /// not hold the row's lease.
    ///
    /// # Errors
    /// [`StoreError::Sqlx`] on a write failure.
    async fn note_attempt_finished(
        &self,
        id: &EffectIntentId,
        token: i64,
        cadence_ms: i64,
    ) -> Result<bool, StoreError>;

    /// Give the row back. A no-op if the token does not hold it — including
    /// when this token already released, so the door closes exactly once.
    ///
    /// # Errors
    /// [`StoreError::Sqlx`] on a write failure.
    async fn release_lease(&self, id: &EffectIntentId, token: i64) -> Result<(), StoreError>;

    /// Push the next attempt a cadence away WITHOUT counting anything — the
    /// back-off for a turn that errored before any call began (PR #18 review):
    /// counting a start that never reached the wire or a finish for a call
    /// that never began would both lie in the ledger, and NOT backing off
    /// leaves the erroring row earliest-due forever, starving the queue.
    /// Fenced like every pursuit write: the token must hold the lease.
    ///
    /// # Errors
    /// [`StoreError::Sqlx`] on a write failure.
    async fn defer_attempt(
        &self,
        id: &EffectIntentId,
        token: i64,
        cadence_ms: i64,
    ) -> Result<bool, StoreError>;

    /// How long until this live intent's next scheduled attempt, under the
    /// STORE's clock — the durable schedule an HTTP `Retry-After` projects
    /// (ADR-021: the service computing `row − now` itself would mint a second
    /// clock). Non-negative; `None` for a settled or unknown identity.
    ///
    /// # Errors
    /// [`StoreError::Sqlx`] on a read failure.
    async fn retry_hint_ms(&self, id: &EffectIntentId) -> Result<Option<i64>, StoreError>;

    /// Record that we gave up chasing at retry cadence (ADR-019).
    ///
    /// Conditional and fenced: once-only (`escalated_at_ms IS NULL`), only on a
    /// live intent, only under the claimed token. `escalation_attempts` is
    /// derived *in the write* from `attempts_started` — asserted by nobody, which
    /// is the point. The booking is untouched: no state, no version, no audit
    /// row. Losing the race to a settling fact is a no-op, not an error.
    ///
    /// # Errors
    /// [`StoreError::Sqlx`] on a write failure.
    async fn mark_escalated(
        &self,
        id: &EffectIntentId,
        token: i64,
        long_cadence_ms: i64,
    ) -> Result<EscalationWrite, StoreError>;

    /// The human queue: escalated, still unresolved. One indexed predicate.
    ///
    /// # Errors
    /// [`StoreError::Sqlx`] on a read failure.
    async fn escalated_unresolved(&self, limit: u32) -> Result<Vec<EffectIntentId>, StoreError>;
}

/// One claimed turn: the intent, the fencing token, and the accounting the
/// caller's decision needs — beside the domain type, not inside it, because
/// pursuit facts are the store's axis and not domain vocabulary.
#[derive(Clone, Debug)]
pub struct ClaimedEffect {
    pub intent: EffectIntent,
    pub token: i64,
    pub attempts_started: u32,
    pub escalated: bool,
}

/// What an escalation write did.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum EscalationWrite {
    /// The marker landed.
    Recorded,
    /// Nothing to do: already escalated, already settled, or the lease moved.
    /// Idempotence, not failure — a replayed exhaustion writes nothing.
    Noop,
}

/// The longest cadence any code may schedule, and the skew clamp's bound: a
/// stored `next_attempt_after_ms` beyond `now + MAX_CADENCE_MS` is evidence the
/// clock moved backwards after the write — no live code could have produced it —
/// and the honest response to skew is to treat the row as due and go ask.
pub const MAX_CADENCE_MS: i64 = 60 * 60 * 1000;

/// The lease clamp's bound, by the identical argument (a crashed owner plus a
/// rollback would otherwise strand its row invisible on the *lease* predicate
/// even after the cadence clamp fires).
pub const MAX_LEASE_MS: i64 = 60 * 1000;

#[derive(Clone, Debug)]
pub struct SqliteBookingRepository {
    pool: SqlitePool,
    effect_ttl_ms: i64,
    clock: Arc<dyn StoreClock>,
}

/// The repository's clock. Exactly one, injectable, never read anywhere else.
///
/// The domain stays clock-free (ADR-013, ADR-016 §2, ADR-018 rule 2): this
/// clock derives deadlines and stamps rows, and it never decides absence — that
/// determination is the council's alone. It is injectable because slice E's
/// test 16 requires our clock deliberately *ahead* of the council's, and a
/// hard-coded `SystemTime::now()` cannot be moved.
pub trait StoreClock: Send + Sync + std::fmt::Debug {
    fn now_ms(&self) -> i64;
}

/// The default: real wall-clock time.
#[derive(Debug, Default, Clone, Copy)]
pub struct SystemStoreClock;

impl StoreClock for SystemStoreClock {
    fn now_ms(&self) -> i64 {
        SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .map_or(0, |elapsed| {
                i64::try_from(elapsed.as_millis()).unwrap_or(i64::MAX)
            })
    }
}

impl SqliteBookingRepository {
    /// Open (creating if absent) the `SQLite` database at `path` and run migrations.
    ///
    /// # Errors
    /// Returns [`StoreError::Sqlx`] if the file cannot be opened or the pool
    /// cannot connect, and [`StoreError::Migration`] if migrations fail to apply.
    pub async fn open(path: impl AsRef<Path>) -> Result<Self, StoreError> {
        Self::open_with(path, DEFAULT_EFFECT_TTL_MS, Arc::new(SystemStoreClock)).await
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
        Self::open_with(path, effect_ttl_ms, Arc::new(SystemStoreClock)).await
    }

    /// Open with everything injectable.
    ///
    /// # Errors
    /// As [`Self::open`], plus [`StoreError::AbandonedRowsPresent`] if the
    /// database was written by a pre-ADR-019 build — see the preflight below.
    pub async fn open_with(
        path: impl AsRef<Path>,
        effect_ttl_ms: i64,
        clock: Arc<dyn StoreClock>,
    ) -> Result<Self, StoreError> {
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

        // The ADR-019 preflight, in Rust because `SELECT RAISE(...)` is not
        // legal SQL outside a trigger (review ran SQLite to prove it). A
        // pre-ADR-019 database can hold intents whose status is `Abandoned` and
        // bookings stranded at `NeedsHuman` with no active identity — a shape
        // this schema cannot carry and this code cannot even parse. Refusing is
        // deliberate: recovering the originating in-flight state is ambiguous
        // for a Book-kind intent (`BookingInProgress` vs
        // `CancellationRequested` — the exact information the old design
        // destroyed), derivable only from the audit trail, and not worth
        // building for a POC with no production databases. Running on every
        // open is harmless — nothing can write the value any more — so this
        // doubles as a corruption tripwire.
        let stale: Option<i64> = sqlx::query_scalar(
            r"
            SELECT COUNT(*) FROM sqlite_master
             WHERE type = 'table' AND name = 'effect_intents'
            ",
        )
        .fetch_optional(&pool)
        .await?;
        if stale.unwrap_or(0) > 0 {
            let abandoned: i64 = sqlx::query_scalar(
                "SELECT COUNT(*) FROM effect_intents WHERE status = 'Abandoned'",
            )
            .fetch_one(&pool)
            .await?;
            if abandoned > 0 {
                return Err(StoreError::AbandonedRowsPresent { count: abandoned });
            }
        }

        MIGRATOR.run(&pool).await?;
        Ok(Self {
            pool,
            effect_ttl_ms,
            clock,
        })
    }

    #[must_use]
    pub fn pool(&self) -> &SqlitePool {
        &self.pool
    }

    fn now(&self) -> i64 {
        self.clock.now_ms()
    }
}

#[async_trait]
impl BookingRepository for SqliteBookingRepository {
    async fn create(&self, booking: NewBooking) -> Result<BookingAggregate, StoreError> {
        let now = self.now();
        let state = BookingState::Draft(Draft);
        let state_json = serde_json::to_string(&state)?;
        let requirements_json = serde_json::to_string(&booking.requirements)?;

        let result = sqlx::query(
            r"
            INSERT OR IGNORE INTO bookings (
                id, version, state_name, state_json, requirements_json,
                selected_venue_json, availability_json, booking_ref, active_effect,
                created_at_ms, updated_at_ms, owner_principal
            ) VALUES (?, 0, ?, ?, ?, NULL, NULL, NULL, NULL, ?, ?, ?)
            ",
        )
        .bind(booking.id.as_str())
        .bind(state.name())
        .bind(state_json)
        .bind(requirements_json)
        .bind(now)
        .bind(now)
        .bind(booking.owner.as_str())
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

    async fn load_visible(
        &self,
        owner: &PrincipalId,
        id: &BookingId,
    ) -> Result<BookingAggregate, StoreError> {
        // The ownership predicate sits beside the id in the same WHERE, so a
        // foreign row is not fetched, not decoded, and not distinguishable from
        // an absent one. A corrupt foreign row therefore cannot surface as a
        // decode error either — which would have been an existence oracle
        // wearing a 503.
        let row = sqlx::query(
            r"
            SELECT id, version, state_name, state_json, requirements_json,
                   selected_venue_json, availability_json, booking_ref, active_effect,
                   created_at_ms, updated_at_ms
            FROM bookings
            WHERE id = ? AND owner_principal = ?
            ",
        )
        .bind(id.as_str())
        .bind(owner.as_str())
        .fetch_optional(&self.pool)
        .await?
        .ok_or_else(|| StoreError::NotFound(id.clone()))?;

        decode_booking_row(&row)
    }

    async fn lookup_by_ref(
        &self,
        owner: &PrincipalId,
        booking_ref: &CouncilBookingRef,
    ) -> Result<Vec<BookingAggregate>, StoreError> {
        let rows = sqlx::query(
            r"
            SELECT id, version, state_name, state_json, requirements_json,
                   selected_venue_json, availability_json, booking_ref, active_effect,
                   created_at_ms, updated_at_ms
            FROM bookings
            WHERE owner_principal = ? AND booking_ref = ?
            ORDER BY created_at_ms, id
            ",
        )
        .bind(owner.as_str())
        .bind(booking_ref.as_str())
        .fetch_all(&self.pool)
        .await?;

        rows.iter().map(decode_booking_row).collect()
    }

    async fn lookup_cancellable(
        &self,
        owner: &PrincipalId,
    ) -> Result<Vec<BookingAggregate>, StoreError> {
        let rows = sqlx::query(
            r"
            SELECT id, version, state_name, state_json, requirements_json,
                   selected_venue_json, availability_json, booking_ref, active_effect,
                   created_at_ms, updated_at_ms
            FROM bookings
            WHERE owner_principal = ?
            ORDER BY created_at_ms, id
            ",
        )
        .bind(owner.as_str())
        .fetch_all(&self.pool)
        .await?;

        // The domain decides what "cancellable" means, here and nowhere else.
        rows.iter()
            .map(decode_booking_row)
            .filter_map(|decoded| match decoded {
                Ok(aggregate) => aggregate
                    .state
                    .proposal_menu()
                    .contains(&CANCEL_PROPOSAL)
                    .then_some(Ok(aggregate)),
                Err(error) => Some(Err(error)),
            })
            .collect()
    }

    async fn commit(
        &self,
        id: &BookingId,
        expected_version: u64,
        next: Booking,
        audit: TransitionAudit,
    ) -> Result<BookingAggregate, StoreError> {
        let now = self.now();

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
        let prepared_at_ms = self.now();
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
                outcome_detail, supersedes, created_at_ms, updated_at_ms
            ) VALUES (?, ?, ?, ?, ?, ?, ?, NULL, NULL, NULL, ?, ?)
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

        let now = self.now();
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

        let loaded = match classified {
            FinalisationState::AlreadyRecorded(intent) => {
                // The version already advanced when the outcome was first
                // recorded, so the CAS would fail; returning the truth is what
                // lets a coordinator retry safely after a lost acknowledgement.
                tx.commit().await?;
                return Ok(FinalizedEffect {
                    aggregate: current,
                    intent: *intent,
                    replayed: true,
                });
            }
            FinalisationState::Fresh(intent) => intent,
        };

        record_outcome(
            &mut tx,
            &loaded,
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

        // A handoff is only coherent in one direction, and nothing so far checked
        // it. Review found that a caller could record "the booking never happened"
        // — `Absent`/`Rejected` with no reference — and in the same transaction
        // create a cancellation for some booking reference. Two contradictory
        // facts, committed atomically.
        //
        // The rule, stated without knowing what a booking is: a successor exists
        // *because* its predecessor succeeded, and it acts on what the predecessor
        // produced. So the predecessor must be `Confirmed` (which the shape rule
        // then requires to carry a reference), and the successor's plan must act
        // on exactly that reference — `BookingEffect::acts_on` is the domain's
        // answer to "which reference does this plan operate on".
        if request.finalising_status != EffectStatus::Confirmed {
            return Err(StoreError::IncoherentHandoff {
                reason: "a successor effect exists only because its predecessor succeeded",
            });
        }
        if request.successor_plan.acts_on() != request.finalising_reference.as_ref() {
            return Err(StoreError::IncoherentHandoff {
                reason: "the successor must act on the reference its predecessor produced",
            });
        }
        // And the aggregate must record that reference too, or the booking would
        // point at one thing while its in-flight effect acts on another.
        if request.next.booking_ref != request.finalising_reference {
            return Err(StoreError::IncoherentHandoff {
                reason: "the next booking must record the reference its predecessor produced",
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
        let prepared_at_ms = self.now();
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
            // The successor matching is not enough on its own: a replay must agree
            // about the *predecessor's outcome* too, or a request claiming a
            // different finalisation would be answered as though it had been
            // accepted.
            if finalised.status != request.finalising_status
                || finalised.provider_reference != request.finalising_reference
                || finalised.outcome_detail != request.finalising_detail
            {
                return Err(StoreError::ContradictoryFinalisation {
                    effect_intent_id: request.finalising.clone(),
                    recorded: finalised.status.name(),
                    attempted: request.finalising_status.name(),
                });
            }
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
        let loaded = match classified {
            FinalisationState::AlreadyRecorded(intent) => {
                return Err(StoreError::HandoffPredecessorAlreadyFinal {
                    effect_intent_id: request.finalising.clone(),
                    recorded: intent.status.name(),
                });
            }
            FinalisationState::Fresh(intent) => intent,
        };

        record_outcome(
            &mut tx,
            &loaded,
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
                outcome_detail, supersedes, created_at_ms, updated_at_ms
            ) VALUES (?, ?, ?, ?, ?, ?, ?, NULL, NULL, ?, ?, ?)
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

    async fn due_effects(&self, limit: u32) -> Result<Vec<EffectIntentId>, StoreError> {
        let now = self.now();
        let rows = sqlx::query(
            r"
            SELECT effect_intent_id FROM effect_intents
             WHERE status IN ('Prepared', 'Unknown')
               AND (next_attempt_after_ms <= ?1
                    -- the skew clamp: no live code schedules past now + MAX, so
                    -- a value out there means the clock moved backwards after
                    -- the write, and the honest response is to go ask
                    OR next_attempt_after_ms > ?1 + ?2)
               AND (lease_until_ms IS NULL
                    OR lease_until_ms < ?1
                    -- the same clamp for a lease stranded by the same rollback
                    OR lease_until_ms > ?1 + ?3)
             ORDER BY next_attempt_after_ms ASC
             LIMIT ?4
            ",
        )
        .bind(now)
        .bind(MAX_CADENCE_MS)
        .bind(MAX_LEASE_MS)
        .bind(i64::from(limit))
        .fetch_all(&self.pool)
        .await?;

        Ok(rows
            .iter()
            .map(|row| EffectIntentId::new(row.get::<String, _>("effect_intent_id")))
            .collect())
    }

    async fn claim_effect(
        &self,
        id: &EffectIntentId,
        lease_ms: i64,
    ) -> Result<Option<ClaimedEffect>, StoreError> {
        let now = self.now();
        let mut tx = self.pool.begin_with("BEGIN IMMEDIATE").await?;

        // Eligibility is re-checked HERE, atomically with the claim — not only
        // in `due_effects` — so `claim` with a known id cannot bypass the
        // cadence, and the writer lock decides races between claimants.
        let updated = sqlx::query(
            r"
            UPDATE effect_intents
               SET lease_token = lease_token + 1,
                   lease_until_ms = ?1 + ?2,
                   updated_at_ms = ?1
             WHERE effect_intent_id = ?3
               AND status IN ('Prepared', 'Unknown')
               AND (next_attempt_after_ms <= ?1 OR next_attempt_after_ms > ?1 + ?4)
               AND (lease_until_ms IS NULL
                    OR lease_until_ms < ?1
                    OR lease_until_ms > ?1 + ?5)
            ",
        )
        .bind(now)
        .bind(lease_ms.min(MAX_LEASE_MS))
        .bind(id.as_str())
        .bind(MAX_CADENCE_MS)
        .bind(MAX_LEASE_MS)
        .execute(&mut *tx)
        .await?;

        if updated.rows_affected() != 1 {
            tx.commit().await?;
            return Ok(None);
        }

        let row = sqlx::query(
            r"
            SELECT effect_intent_id, booking_id, operation_kind, source_version,
                   canonical_plan_json, status, expires_at_ms, provider_reference,
                   outcome_detail, supersedes, created_at_ms, updated_at_ms,
                   lease_token, attempts_started, escalated_at_ms
              FROM effect_intents WHERE effect_intent_id = ?
            ",
        )
        .bind(id.as_str())
        .fetch_one(&mut *tx)
        .await?;
        tx.commit().await?;

        let attempts: i64 = row.try_get("attempts_started")?;
        Ok(Some(ClaimedEffect {
            intent: decode_effect_row(&row)?,
            token: row.try_get("lease_token")?,
            attempts_started: u32::try_from(attempts).unwrap_or(u32::MAX),
            escalated: row.try_get::<Option<i64>, _>("escalated_at_ms")?.is_some(),
        }))
    }

    async fn note_attempt_started(
        &self,
        id: &EffectIntentId,
        token: i64,
    ) -> Result<bool, StoreError> {
        let now = self.now();
        let updated = sqlx::query(
            r"
            UPDATE effect_intents
               SET attempts_started = attempts_started + 1,
                   -- the one honest status move on this axis: a call is about to
                   -- happen, so 'never attempted' stops being true BEFORE the
                   -- wire, not after the timeout
                   status = CASE WHEN status = 'Prepared' THEN 'Unknown' ELSE status END,
                   updated_at_ms = ?1
             WHERE effect_intent_id = ?2 AND lease_token = ?3
               -- and the lease must be HELD: a matching token alone would let a
               -- caller pass the freshly-minted row's default (0) and spend
               -- budget without ever claiming, and would let a released turn's
               -- token keep writing after release. Claiming is the gate, and
               -- this predicate is where the schema says so (review of PR #15).
               AND lease_until_ms IS NOT NULL
            ",
        )
        .bind(now)
        .bind(id.as_str())
        .bind(token)
        .execute(&self.pool)
        .await?;
        Ok(updated.rows_affected() == 1)
    }

    async fn note_attempt_finished(
        &self,
        id: &EffectIntentId,
        token: i64,
        cadence_ms: i64,
    ) -> Result<bool, StoreError> {
        let now = self.now();
        // The schedule is now + a clamped DURATION. The shipped first version
        // wrote MIN(cadence, now + MAX) — the cadence compared as a timestamp,
        // five seconds after 1970, always due (ADR-021).
        let updated = sqlx::query(
            r"
            UPDATE effect_intents
               SET attempts_finished = attempts_finished + 1,
                   next_attempt_after_ms = ?2 + MIN(MAX(?1, 0), ?3),
                   updated_at_ms = ?2
             WHERE effect_intent_id = ?4 AND lease_token = ?5
               AND lease_until_ms IS NOT NULL
            ",
        )
        .bind(cadence_ms)
        .bind(now)
        .bind(MAX_CADENCE_MS)
        .bind(id.as_str())
        .bind(token)
        .execute(&self.pool)
        .await?;
        Ok(updated.rows_affected() == 1)
    }

    async fn defer_attempt(
        &self,
        id: &EffectIntentId,
        token: i64,
        cadence_ms: i64,
    ) -> Result<bool, StoreError> {
        let now = self.now();
        let updated = sqlx::query(
            r"
            UPDATE effect_intents
               SET next_attempt_after_ms = ?2 + MIN(MAX(?1, 0), ?3),
                   updated_at_ms = ?2
             WHERE effect_intent_id = ?4 AND lease_token = ?5
               AND lease_until_ms IS NOT NULL
            ",
        )
        .bind(cadence_ms)
        .bind(now)
        .bind(MAX_CADENCE_MS)
        .bind(id.as_str())
        .bind(token)
        .execute(&self.pool)
        .await?;
        Ok(updated.rows_affected() == 1)
    }

    async fn retry_hint_ms(&self, id: &EffectIntentId) -> Result<Option<i64>, StoreError> {
        let now = self.now();
        let hint: Option<i64> = sqlx::query_scalar(
            r"
            SELECT MAX(next_attempt_after_ms - ?1, 0) FROM effect_intents
             WHERE effect_intent_id = ?2 AND status IN ('Prepared', 'Unknown')
            ",
        )
        .bind(now)
        .bind(id.as_str())
        .fetch_optional(&self.pool)
        .await?;
        Ok(hint)
    }

    async fn release_lease(&self, id: &EffectIntentId, token: i64) -> Result<(), StoreError> {
        sqlx::query(
            r"
            UPDATE effect_intents SET lease_until_ms = NULL
             WHERE effect_intent_id = ? AND lease_token = ?
               AND lease_until_ms IS NOT NULL
            ",
        )
        .bind(id.as_str())
        .bind(token)
        .execute(&self.pool)
        .await?;
        Ok(())
    }

    async fn mark_escalated(
        &self,
        id: &EffectIntentId,
        token: i64,
        long_cadence_ms: i64,
    ) -> Result<EscalationWrite, StoreError> {
        let now = self.now();
        let updated = sqlx::query(
            r"
            UPDATE effect_intents
               SET escalated_at_ms = ?1,
                   escalation_attempts = attempts_started,
                   next_attempt_after_ms = ?1 + ?2,
                   updated_at_ms = ?1
             WHERE effect_intent_id = ?3
               AND lease_token = ?4
               AND lease_until_ms IS NOT NULL
               AND escalated_at_ms IS NULL
               AND status IN ('Prepared', 'Unknown')
            ",
        )
        .bind(now)
        .bind(long_cadence_ms.min(MAX_CADENCE_MS))
        .bind(id.as_str())
        .bind(token)
        .execute(&self.pool)
        .await?;

        Ok(if updated.rows_affected() == 1 {
            EscalationWrite::Recorded
        } else {
            EscalationWrite::Noop
        })
    }

    async fn escalated_unresolved(&self, limit: u32) -> Result<Vec<EffectIntentId>, StoreError> {
        let rows = sqlx::query(
            r"
            SELECT effect_intent_id FROM effect_intents
             WHERE escalated_at_ms IS NOT NULL
               AND status IN ('Prepared', 'Unknown')
             ORDER BY escalated_at_ms ASC
             LIMIT ?
            ",
        )
        .bind(i64::from(limit))
        .fetch_all(&self.pool)
        .await?;

        Ok(rows
            .iter()
            .map(|row| EffectIntentId::new(row.get::<String, _>("effect_intent_id")))
            .collect())
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

#[cfg(test)]
mod tests {
    use super::*;
    use bld_types::{
        AvailabilityGrant, Money, PrincipalId, Provenance, SlotId, TimeWindow, VenueId,
    };
    use tempfile::TempDir;
    use townhall_domain::{
        Booked, BookingProposal, BookingState, Cancelled, SelectedVenueRef, VenueFacts,
        VenueSelected,
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

    /// Someone who is not [`test_owner`].
    fn other_principal() -> PrincipalId {
        PrincipalId::new("priya")
    }

    /// The magic string in `lookup_cancellable`, pinned to the proposal it means.
    ///
    /// `proposal_menu()` exports names, so the filter has to match one as text.
    /// Rename the variant and the filter silently matches nothing — every
    /// booking becomes uncancellable through the lookup, and every test that
    /// only asserts *exclusion* keeps passing. This is the test that fails
    /// instead.
    #[test]
    fn the_cancel_menu_name_matches_the_proposal() {
        let cancel = BookingProposal::Cancel {
            reason: "any".to_owned(),
        };
        assert_eq!(
            cancel.name(),
            CANCEL_PROPOSAL,
            "the lookup filter's string drifted from the proposal it means"
        );
        // And it really is what Draft's menu advertises, so the filter matches
        // the same vocabulary the domain exports.
        assert!(
            BookingState::Draft(Draft)
                .proposal_menu()
                .contains(&CANCEL_PROPOSAL),
            "Draft's menu no longer spells Cancel the way the filter expects"
        );
    }

    /// A foreign row is not merely refused — it is not fetched.
    ///
    /// Paired with the owner's own load succeeding, so an implementation that
    /// concealed everything from everyone fails the second half.
    #[tokio::test]
    async fn load_visible_conceals_another_principals_booking() {
        let temp = TempDir::new().expect("temp dir");
        let repo = repo_in(&temp).await;
        let id = BookingId::new("BKG-OWNED");
        repo.create(NewBooking {
            id: id.clone(),
            requirements: requirements(),
            owner: test_owner(),
        })
        .await
        .expect("create");

        let mine = repo.load_visible(&test_owner(), &id).await;
        assert!(mine.is_ok(), "the owner must see their own booking");

        let theirs = repo.load_visible(&other_principal(), &id).await;
        assert!(
            matches!(&theirs, Err(StoreError::NotFound(missing)) if *missing == id),
            "a foreign load must be indistinguishable from absent, got {theirs:?}"
        );
    }

    /// The migration's legacy rows are unreachable for EVERY principal, not just
    /// for one — and they remain readable through the unscoped `load` that
    /// reconciliation depends on.
    #[tokio::test]
    async fn a_null_owner_row_is_concealed_from_everyone_yet_still_decodes() {
        let temp = TempDir::new().expect("temp dir");
        let repo = repo_in(&temp).await;
        let id = BookingId::new("BKG-LEGACY");
        repo.create(NewBooking {
            id: id.clone(),
            requirements: requirements(),
            owner: test_owner(),
        })
        .await
        .expect("create");

        // Backdate it into the pre-ownership world the migration inherits.
        sqlx::query("UPDATE bookings SET owner_principal = NULL WHERE id = ?")
            .bind(id.as_str())
            .execute(&repo.pool)
            .await
            .expect("orphan the row");

        for principal in [
            test_owner(),
            other_principal(),
            PrincipalId::new(""),
            PrincipalId::new("@orphan"),
        ] {
            let seen = repo.load_visible(&principal, &id).await;
            assert!(
                matches!(seen, Err(StoreError::NotFound(_))),
                "a NULL-owned row leaked to {principal}: {seen:?}"
            );
        }

        // Concealment must not break recovery: the reconciler has no principal
        // and still has to be able to finish an in-flight effect.
        assert!(
            repo.load(&id).await.is_ok(),
            "an orphaned row must still decode through the unscoped load"
        );

        // And it is absent from the COLLECTION surfaces too, not just the
        // direct read. A listing that scanned without the ownership predicate
        // would hand every legacy row to whoever asked first.
        assert!(
            repo.lookup_cancellable(&test_owner())
                .await
                .expect("lookup")
                .is_empty(),
            "a NULL-owned row appeared in a cancellable listing"
        );
        sqlx::query("UPDATE bookings SET booking_ref = 'TH-ORPHAN' WHERE id = ?")
            .bind(id.as_str())
            .execute(&repo.pool)
            .await
            .expect("give the orphan a reference to be found by");
        assert!(
            repo.lookup_by_ref(&test_owner(), &CouncilBookingRef::new("TH-ORPHAN"))
                .await
                .expect("lookup")
                .is_empty(),
            "a NULL-owned row was findable by its council reference"
        );
    }

    /// Ordering is genuinely sorted, in both dimensions, against a fixture that
    /// opposes it.
    ///
    /// A "returns rows in order" assertion passes by accident whenever insertion
    /// order already matches the sort. So rows go in newest-first while the
    /// expectation is oldest-first, and the two rows sharing a `created_at_ms`
    /// go in with their ids DESCENDING — so neither the timestamp sort nor the
    /// id tie-break can be satisfied by returning rows as they were written.
    #[tokio::test]
    async fn lookup_orders_by_creation_then_id_against_an_opposed_fixture() {
        let temp = TempDir::new().expect("temp dir");
        let repo = repo_in(&temp).await;
        let reference = CouncilBookingRef::new("TH-ORDER");

        // Written newest-first; within the tie, id-descending.
        //
        // Each row is advanced to `Booked`, because that is the state a council
        // reference belongs to — the store's coherence check rejects a `Draft`
        // carrying one, and rightly so.
        let written = [("BKG-C", 3_000_i64), ("BKG-B", 1_000), ("BKG-A", 1_000)];
        for (id, created) in written {
            let booking_id = BookingId::new(id);
            repo.create(NewBooking {
                id: booking_id.clone(),
                requirements: requirements(),
                owner: test_owner(),
            })
            .await
            .expect("create");
            repo.commit(
                &booking_id,
                0,
                Booking {
                    id: booking_id.clone(),
                    state: BookingState::Booked(Booked {
                        booking_ref: reference.clone(),
                    }),
                    requirements: requirements(),
                    selected_venue: None,
                    availability: None,
                    booking_ref: Some(reference.clone()),
                    active_effect: None,
                },
                TransitionAudit::driven_by(&BookingProposal::Book),
            )
            .await
            .expect("advance to Booked");
            sqlx::query("UPDATE bookings SET created_at_ms = ? WHERE id = ?")
                .bind(created)
                .bind(id)
                .execute(&repo.pool)
                .await
                .expect("backdate the row");
        }

        let found = repo
            .lookup_by_ref(&test_owner(), &reference)
            .await
            .expect("lookup");
        let order: Vec<&str> = found.iter().map(|row| row.id.as_str()).collect();
        assert_eq!(
            order,
            ["BKG-A", "BKG-B", "BKG-C"],
            "rows must come back oldest-first with the id breaking the 1000ms tie"
        );
    }

    /// The filter asks the DOMAIN what cancellable means, rather than knowing.
    ///
    /// The complete state × menu partition is already pinned where it belongs —
    /// `the_exported_menu_is_the_locked_table` in `townhall-domain` sweeps every
    /// state against `LOCKED`. Restating that list here would put it in a second
    /// place to drift. What this test owes is narrower and is the thing the
    /// domain cannot check: that `lookup_cancellable` consults the menu at all.
    ///
    /// So it persists two states that are cancellable but NOT the same shape —
    /// `Draft` and `VenueSelected` — plus one that is not (`Cancelled`). A
    /// filter hardcoded to any single state name fails on the other cancellable
    /// row; one that skips filtering fails on the excluded row.
    #[tokio::test]
    async fn lookup_cancellable_asks_the_domain_rather_than_hardcoding_a_state() {
        let temp = TempDir::new().expect("temp dir");
        let repo = repo_in(&temp).await;

        let draft = BookingId::new("BKG-DRAFT");
        let selected = BookingId::new("BKG-SELECTED");
        let cancelled = BookingId::new("BKG-CANCELLED");
        for id in [&draft, &selected, &cancelled] {
            repo.create(NewBooking {
                id: id.clone(),
                requirements: requirements(),
                owner: test_owner(),
            })
            .await
            .expect("create");
        }

        let venue = SelectedVenueRef {
            venue_id: VenueId::new("TH-A"),
            slot_id: SlotId::new("SLOT-A"),
        };
        repo.commit(
            &selected,
            0,
            Booking {
                id: selected.clone(),
                state: BookingState::VenueSelected(VenueSelected {
                    venue_id: venue.venue_id.clone(),
                    slot_id: venue.slot_id.clone(),
                }),
                requirements: requirements(),
                selected_venue: Some(venue.clone()),
                availability: None,
                booking_ref: None,
                active_effect: None,
            },
            TransitionAudit::driven_by(&BookingProposal::SelectVenue {
                venue_id: venue.venue_id.clone(),
                slot_id: venue.slot_id.clone(),
            }),
        )
        .await
        .expect("advance to VenueSelected");

        repo.commit(
            &cancelled,
            0,
            Booking {
                id: cancelled.clone(),
                state: BookingState::Cancelled(Cancelled),
                requirements: requirements(),
                selected_venue: None,
                availability: None,
                booking_ref: None,
                active_effect: None,
            },
            TransitionAudit::driven_by(&BookingProposal::Cancel {
                reason: "done".to_owned(),
            }),
        )
        .await
        .expect("advance to Cancelled");

        let found = repo
            .lookup_cancellable(&test_owner())
            .await
            .expect("lookup");
        let ids: Vec<&str> = found.iter().map(|row| row.id.as_str()).collect();
        assert_eq!(
            ids,
            ["BKG-DRAFT", "BKG-SELECTED"],
            "cancellable must follow the menu: both offering states in, the \
             empty-menu one out"
        );
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
                    owner: test_owner(),
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
                owner: test_owner(),
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
                owner: test_owner(),
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
                owner: test_owner(),
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
                owner: test_owner(),
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
            owner: test_owner(),
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
            // The grant belongs in this round-trip specifically. The plan is
            // persisted as JSON between Phase A and Phase B, so a grant that did
            // not survive serialisation would be silently absent exactly when the
            // council needs it — and the create would either fail or fall back to
            // re-reading availability, which is the defect it exists to prevent.
            grant: AvailabilityGrant::new("GRANT-TH-A-SLOT-1-v7"),
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
            owner: test_owner(),
        })
        .await
        .expect("first create");

        let error = repo
            .create(NewBooking {
                id: id.clone(),
                requirements: requirements(),
                owner: test_owner(),
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
                    owner: test_owner(),
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
                owner: test_owner(),
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

/// Is this a valid *outcome* to record?
///
/// A narrower question than "is this intent coherent", and deliberately separate
/// from it. `Prepared` is a perfectly good status for an intent to hold — it is
/// just not something an effect can be finalised *to*. Conflating the two, as an
/// earlier version of this function did, meant one name answering two questions
/// and a rule about intent shape living here as well as in the domain.
///
/// The shape rule itself is [`EffectIntent::coherent`]'s, checked when the row is
/// written and again when it is read.
fn validate_outcome_status(status: EffectStatus) -> Result<(), StoreError> {
    if status.is_terminal() {
        Ok(())
    } else {
        Err(StoreError::NotATerminalStatus(status.name()))
    }
}

/// The outcome of asking whether a finalisation has already happened.
enum FinalisationState {
    /// Not yet recorded; proceed. Carries the loaded intent so the write path can
    /// project what the row will become and check it before writing.
    Fresh(Box<EffectIntent>),
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
    validate_outcome_status(status)?;

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

    Ok(FinalisationState::Fresh(Box::new(intent)))
}

/// Write an effect's terminal outcome.
///
/// `projected` is the intent as it will be once written, and it is checked before
/// anything is: the same definition the read gate applies, so a row that could not
/// be loaded can never be stored either.
async fn record_outcome(
    tx: &mut sqlx::Transaction<'_, sqlx::Sqlite>,
    current: &EffectIntent,
    status: EffectStatus,
    provider_reference: Option<&CouncilBookingRef>,
    outcome_detail: Option<&BoundedString>,
    now: i64,
) -> Result<(), StoreError> {
    let effect_intent_id = &current.effect_intent_id;

    // The row as it will be. Checked against the same definition the read gate
    // applies, so a row that could not be loaded can never be stored either.
    let projected = EffectIntent {
        status,
        provider_reference: provider_reference.cloned(),
        outcome_detail: outcome_detail.cloned(),
        ..current.clone()
    };
    if let Err(reason) = projected.coherent() {
        return Err(StoreError::IncoherentIntent {
            where_: "a recorded",
            reason,
        });
    }

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

    let intent = EffectIntent {
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
    };

    // The gate C1 gave bookings and skipped for intents. Its absence was visible
    // as a symptom: the fact door hand-rolled the shape rule and the
    // kind-versus-plan comparison in three places, because nothing guaranteed a
    // loaded row was sound. Refusing here is what let those become one call.
    if let Err(reason) = intent.coherent() {
        return Err(StoreError::IncoherentIntent {
            where_: "a persisted",
            reason,
        });
    }

    Ok(intent)
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
    use bld_types::{AvailabilityGrant, Money, PrincipalId, SlotId, TimeWindow, VenueId};
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
            grant: AvailabilityGrant::new("test-grant"),
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
            owner: test_owner(),
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

        let before = SystemStoreClock.now_ms();
        let prepared = repo
            .prepare_effect(prepare_at(&id, 0, "TH-A"))
            .await
            .expect("prepare");
        let after = SystemStoreClock.now_ms();

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
            principal: PrincipalId::new("lucy"),
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
                    principal: PrincipalId::new("lucy"),
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
    use bld_types::{AvailabilityGrant, Money, PrincipalId, SlotId, TimeWindow, VenueId};
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
            grant: AvailabilityGrant::new("test-grant"),
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
            owner: test_owner(),
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
    ///
    /// C2 folded the rule into `EffectIntent::coherent`, so the refusal now names
    /// the intent contradicting itself rather than a store-local "invalid
    /// outcome" — one definition, applied on write here and on read in
    /// `decode_effect_row`.
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
            // `where_` is asserted, not ignored: without the write-path check the
            // malformed row is written and then refused by the read-back inside
            // the same transaction — same rollback, but the diagnosis becomes
            // "this was already in the database" when the truth is "you tried to
            // write this". Mutation testing found the write gate untested for
            // exactly that reason.
            assert!(
                matches!(
                    error,
                    StoreError::IncoherentIntent {
                        where_: "a recorded",
                        reason: IncoherentIntent::OutcomeShape { .. },
                    }
                ),
                "expected the WRITE path to refuse {status:?}/{reference:?}, got {error:?}"
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

    /// The gate the audit found missing. C1 refused an incoherent *booking* on
    /// read and on write; the intent got only the write half, and the absence was
    /// visible as a symptom — the fact door hand-rolled the same rules in three
    /// places because nothing guaranteed a loaded row was sound.
    ///
    /// Written past the repository's own API, which is the only way such a row can
    /// come into being now.
    #[tokio::test]
    async fn a_persisted_intent_that_contradicts_itself_fails_to_load() {
        let temp = TempDir::new().expect("temp dir");
        let repo = repo_in(&temp).await;
        let id = BookingId::new("BKG-BADINTENT");
        let (_, effect) = in_flight(&repo, &id).await;

        // Prepared, yet carrying a provider reference: an effect that officially
        // has not been attempted, naming something the council supposedly made.
        sqlx::query("UPDATE effect_intents SET provider_reference = ? WHERE effect_intent_id = ?")
            .bind("TH-92718")
            .bind(effect.as_str())
            .execute(repo.pool())
            .await
            .expect("the corrupt row should be written");

        let error = repo
            .load_effect(&effect)
            .await
            .expect_err("a self-contradictory intent must not load");
        assert!(
            matches!(
                error,
                StoreError::IncoherentIntent {
                    reason: IncoherentIntent::OutcomeShape { .. },
                    ..
                }
            ),
            "expected an outcome-shape refusal, got {error:?}"
        );
    }

    /// And the same row cannot be reached through a *booking* load either, since
    /// Phase C reads the intent on its way to a decision.
    #[tokio::test]
    async fn a_corrupt_intent_blocks_the_operations_that_read_it() {
        let temp = TempDir::new().expect("temp dir");
        let repo = repo_in(&temp).await;
        let id = BookingId::new("BKG-BADINTENT2");
        let (aggregate, effect) = in_flight(&repo, &id).await;

        sqlx::query("UPDATE effect_intents SET operation_kind = ? WHERE effect_intent_id = ?")
            .bind(OperationKind::Cancel.name())
            .bind(effect.as_str())
            .execute(repo.pool())
            .await
            .expect("the corrupt row should be written");

        let error = repo
            .finalize_effect(finalize(
                &id,
                aggregate.version,
                &effect,
                EffectStatus::Confirmed,
                Some(REF),
                booked(&id),
                &confirmed_fact(&effect),
            ))
            .await
            .expect_err("a corrupt intent must not be finalisable");
        assert!(
            matches!(
                error,
                StoreError::IncoherentIntent {
                    reason: IncoherentIntent::KindDisagreesWithPlan { .. },
                    ..
                }
            ),
            "expected a kind-versus-plan refusal, got {error:?}"
        );
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
                        cancelled_by: PrincipalId::new("lucy"),
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
                    principal: PrincipalId::new("lucy"),
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
                booking_ref: CouncilBookingRef::new(REF),
                principal: PrincipalId::new("lucy"),
            }
        );
        // And the audit row attributes it to the fact, not to Lucy.
        let audit = repo.audit_events(&id).await.expect("audit");
        let last = audit.last().expect("a row");
        assert_eq!(last.driver_kind, Provenance::Fact);
        assert_eq!(last.driver_detail, "BookingExists");
    }

    /// A handoff is only coherent in one direction, and this is the test for it.
    ///
    /// Without the rule a caller could record "the booking never happened" —
    /// `Absent` or `Rejected`, no reference — and in the same transaction create a
    /// cancellation for some booking reference. Two contradictory facts committed
    /// atomically, which is worse than either alone: the trail would say the
    /// booking failed while a cancellation intent went to the council for it.
    // Three fixtures inline, each isolating one contradiction, plus the positive
    // control that proves the rule is not simply refusing everything. Splitting
    // them into separate tests would duplicate the twelve-line setup four times.
    #[allow(clippy::too_many_lines)]
    #[tokio::test]
    async fn a_handoff_that_contradicts_itself_is_refused() {
        let temp = TempDir::new().expect("temp dir");
        let repo = repo_in(&temp).await;
        let id = BookingId::new("BKG-INCOHERENT");
        let (aggregate, book_effect) = in_flight(&repo, &id).await;
        let requested = repo
            .commit(
                &id,
                aggregate.version,
                booking_at(
                    &id,
                    BookingState::CancellationRequested(CancellationRequested {
                        effect_intent_id: book_effect.clone(),
                        cancelled_by: PrincipalId::new("lucy"),
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

        let coherent_next = booking_at(
            &id,
            BookingState::CancellingBooking(CancellingBooking {
                booking_ref: CouncilBookingRef::new(REF),
                effect_intent_id: cancel_effect.clone(),
            }),
            Some(REF),
            Some(&cancel_effect),
        );
        let base = || HandoffEffect {
            booking_id: id.clone(),
            source_version: requested.version,
            finalising: book_effect.clone(),
            finalising_status: EffectStatus::Confirmed,
            finalising_reference: Some(CouncilBookingRef::new(REF)),
            finalising_detail: None,
            successor_plan: BookingEffect::CancelBooking {
                booking_ref: CouncilBookingRef::new(REF),
                principal: PrincipalId::new("lucy"),
            },
            next: coherent_next.clone(),
            audit: TransitionAudit::driven_by(&confirmed_fact(&book_effect)),
        };

        // One defect per fixture, three ways to contradict.

        // (a) The predecessor failed, so there is nothing for a successor to
        //     continue. Isolated to that one defect: an `Absent` outcome carries no
        //     reference, so the successor plan here is one that acts on nothing
        //     either — every reference comparison agrees on `None`, and only the
        //     status is wrong.
        let book_successor = derive_effect_intent_id(&id, OperationKind::Book, requested.version);
        let error = repo
            .handoff_effect(HandoffEffect {
                finalising_status: EffectStatus::Absent,
                finalising_reference: None,
                successor_plan: book_plan(),
                next: booking_at(
                    &id,
                    BookingState::BookingInProgress(townhall_domain::BookingInProgress {
                        effect_intent_id: book_successor.clone(),
                    }),
                    None,
                    Some(&book_successor),
                ),
                ..base()
            })
            .await
            .expect_err("a failed predecessor cannot hand off");
        assert!(
            matches!(error, StoreError::IncoherentHandoff { .. }),
            "expected IncoherentHandoff, got {error:?}"
        );

        // (b) The successor acts on a different booking than the one confirmed.
        let error = repo
            .handoff_effect(HandoffEffect {
                successor_plan: BookingEffect::CancelBooking {
                    booking_ref: CouncilBookingRef::new("TH-00000"),
                    principal: PrincipalId::new("lucy"),
                },
                ..base()
            })
            .await
            .expect_err("a successor acting on another reference must be refused");
        assert!(matches!(error, StoreError::IncoherentHandoff { .. }));

        // (c) The aggregate records a different reference than was confirmed.
        let error = repo
            .handoff_effect(HandoffEffect {
                next: booking_at(
                    &id,
                    BookingState::CancellingBooking(CancellingBooking {
                        booking_ref: CouncilBookingRef::new("TH-00000"),
                        effect_intent_id: cancel_effect.clone(),
                    }),
                    Some("TH-00000"),
                    Some(&cancel_effect),
                ),
                ..base()
            })
            .await
            .expect_err("an aggregate recording another reference must be refused");
        assert!(matches!(error, StoreError::IncoherentHandoff { .. }));

        // Nothing moved, and no cancellation intent exists.
        let current = repo.load(&id).await.expect("load");
        assert_eq!(current.version, requested.version);
        assert_eq!(current.active_effect, Some(book_effect.clone()));
        assert!(
            repo.load_effect(&cancel_effect).await.is_err(),
            "no successor may have been created"
        );

        // And the coherent request still works, so the rule is not simply refusing
        // everything.
        repo.handoff_effect(base())
            .await
            .expect("the coherent handoff must still succeed");
    }

    /// A replay must agree about the predecessor's outcome, not just the
    /// successor. Otherwise a request claiming a different finalisation would be
    /// answered as though it had been accepted.
    #[tokio::test]
    async fn a_replay_claiming_a_different_predecessor_outcome_is_refused() {
        let temp = TempDir::new().expect("temp dir");
        let repo = repo_in(&temp).await;
        let id = BookingId::new("BKG-REPLAYOUT");
        let (aggregate, book_effect) = in_flight(&repo, &id).await;
        let requested = repo
            .commit(
                &id,
                aggregate.version,
                booking_at(
                    &id,
                    BookingState::CancellationRequested(CancellationRequested {
                        effect_intent_id: book_effect.clone(),
                        cancelled_by: PrincipalId::new("lucy"),
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
        let next = booking_at(
            &id,
            BookingState::CancellingBooking(CancellingBooking {
                booking_ref: CouncilBookingRef::new(REF),
                effect_intent_id: cancel_effect.clone(),
            }),
            Some(REF),
            Some(&cancel_effect),
        );
        let request = HandoffEffect {
            booking_id: id.clone(),
            source_version: requested.version,
            finalising: book_effect.clone(),
            finalising_status: EffectStatus::Confirmed,
            finalising_reference: Some(CouncilBookingRef::new(REF)),
            finalising_detail: None,
            successor_plan: BookingEffect::CancelBooking {
                booking_ref: CouncilBookingRef::new(REF),
                principal: PrincipalId::new("lucy"),
            },
            next,
            audit: TransitionAudit::driven_by(&confirmed_fact(&book_effect)),
        };

        repo.handoff_effect(request.clone())
            .await
            .expect("first handoff");
        // The genuine retry is idempotent.
        let replay = repo
            .handoff_effect(request.clone())
            .await
            .expect("an identical retry must replay");
        assert!(replay.replayed);

        // A retry claiming the predecessor was rejected instead must not be
        // answered as a replay. It is refused before the coherence rule, since a
        // rejected predecessor cannot hand off at all.
        let error = repo
            .handoff_effect(HandoffEffect {
                finalising_status: EffectStatus::Rejected,
                finalising_reference: None,
                ..request.clone()
            })
            .await
            .expect_err("a contradictory retry must be refused");
        assert!(
            matches!(error, StoreError::IncoherentHandoff { .. }),
            "expected refusal, got {error:?}"
        );

        // And a retry that is coherent, names the same successor, but disagrees
        // about *why* the predecessor ended. `outcome_detail` is the one dimension
        // the coherence rule leaves free, which is what makes this the fixture that
        // isolates the replay path's outcome check — everything else matches, so
        // only that comparison can refuse it.
        let error = repo
            .handoff_effect(HandoffEffect {
                finalising_detail: Some(BoundedString::truncating("a different story")),
                ..request
            })
            .await
            .expect_err("a replay disagreeing about the outcome must be refused");
        assert!(
            matches!(error, StoreError::ContradictoryFinalisation { .. }),
            "expected ContradictoryFinalisation, got {error:?}"
        );
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
                        cancelled_by: PrincipalId::new("lucy"),
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
                    principal: PrincipalId::new("lucy"),
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

#[cfg(test)]
mod pursuit {
    //! The pursuit axis under a movable clock: lease fencing (gate M2), the
    //! once-only escalation write (gate M4's replay half), and the skew clamp
    //! under rollback (gate M17's rollback half). These are the writes the
    //! recovery path stands on, tested where the clock can be moved both ways.

    use super::*;
    use bld_types::{AvailabilityGrant, Money, PrincipalId, SlotId, TimeWindow, VenueId};
    use std::sync::atomic::{AtomicI64, Ordering};
    use tempfile::TempDir;
    use townhall_domain::{BookingProposal, BookingState, SelectedVenueRef, VenueFacts};

    const T0: i64 = 1_000_000_000;

    /// A clock the tests move — forwards past leases, and BACKWARDS, which is
    /// the half nothing else in the suite can produce.
    #[derive(Debug)]
    struct RewindableClock(AtomicI64);

    impl RewindableClock {
        fn starting() -> Arc<Self> {
            Arc::new(Self(AtomicI64::new(T0)))
        }
        fn set(&self, ms: i64) {
            self.0.store(ms, Ordering::SeqCst);
        }
        fn advance(&self, by_ms: i64) {
            self.0.fetch_add(by_ms, Ordering::SeqCst);
        }
    }

    impl StoreClock for RewindableClock {
        fn now_ms(&self) -> i64 {
            self.0.load(Ordering::SeqCst)
        }
    }

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
            availability: Some(facts()),
            booking_ref: None,
            active_effect: Some(effect.clone()),
        }
    }

    fn prepare_at(id: &BookingId, version: u64) -> PrepareEffect {
        let effect = derive_effect_intent_id(id, OperationKind::Book, version);
        PrepareEffect {
            booking_id: id.clone(),
            source_version: version,
            canonical_plan: BookingEffect::Book {
                principal: PrincipalId::new("lucy"),
                attendees: 20,
                facts: facts(),
                grant: AvailabilityGrant::new("test-grant"),
            },
            next: in_progress_write(id, &effect),
            audit: TransitionAudit::driven_by(&BookingProposal::Book),
        }
    }

    /// A repository over the movable clock, with one intent prepared and
    /// claimable — the position every test here starts from.
    async fn claimable_world(
        temp: &TempDir,
    ) -> (
        SqliteBookingRepository,
        Arc<RewindableClock>,
        EffectIntentId,
    ) {
        let clock = RewindableClock::starting();
        let repo = SqliteBookingRepository::open_with(
            temp.path().join("townhall.sqlite"),
            DEFAULT_EFFECT_TTL_MS,
            Arc::clone(&clock) as Arc<dyn StoreClock>,
        )
        .await
        .expect("open");
        let id = BookingId::new("BKG-PURSUIT");
        repo.create(NewBooking {
            id: id.clone(),
            requirements: requirements(),
            owner: test_owner(),
        })
        .await
        .expect("create");
        repo.prepare_effect(prepare_at(&id, 0))
            .await
            .expect("prepare");
        let effect = derive_effect_intent_id(&id, OperationKind::Book, 0);
        (repo, clock, effect)
    }

    /// A migrated legacy row's IN-FLIGHT EFFECT is still recoverable.
    ///
    /// # Why the earlier witness was not enough
    ///
    /// `a_null_owner_row_is_concealed_from_everyone_yet_still_decodes` asserts
    /// that an orphan row decodes through the unscoped `load`, and concluded
    /// from that that recovery still works. It does not follow. Recovery is a
    /// pipeline — `due_effects`, then `claim_effect`, then the aggregate load —
    /// and only the last step was witnessed.
    ///
    /// The failure that slips through: join `due_effects` against `bookings`
    /// and require a non-NULL owner. Every fixture in this suite has an owner,
    /// so every existing test stays green, while every migrated in-flight
    /// intent in a real database is stranded forever — the effect never comes
    /// back as due, so the chase never resumes and a booking that may exist at
    /// the council is never reconciled.
    ///
    /// This drives the pipeline itself, on a row whose owner is NULL.
    #[tokio::test]
    async fn a_migrated_row_can_still_be_discovered_and_claimed_for_recovery() {
        let temp = TempDir::new().expect("temp dir");
        let (repo, clock, effect) = claimable_world(&temp).await;

        // Migrate the booking into the pre-ownership world, effect and all.
        sqlx::query("UPDATE bookings SET owner_principal = NULL WHERE id = ?")
            .bind("BKG-PURSUIT")
            .execute(&repo.pool)
            .await
            .expect("orphan the row");

        // It is invisible to every principal...
        assert!(
            matches!(
                repo.load_visible(&test_owner(), &BookingId::new("BKG-PURSUIT"))
                    .await,
                Err(StoreError::NotFound(_))
            ),
            "the orphan must still be concealed"
        );

        // ...and still fully recoverable.
        clock.advance(10_000);
        let due = repo.due_effects(16).await.expect("due");
        assert!(
            due.contains(&effect),
            "a migrated in-flight effect vanished from the recovery queue: {due:?}"
        );
        let claimed = repo.claim_effect(&effect, 30_000).await.expect("claim");
        assert!(
            claimed.is_some(),
            "a migrated in-flight effect could not be claimed for recovery"
        );
        assert!(
            repo.load(&BookingId::new("BKG-PURSUIT")).await.is_ok(),
            "and the reconciler must still be able to load its aggregate"
        );
    }

    /// The pursuit columns, read raw — these tests are ABOUT the writes, so
    /// they read the row rather than trusting a struct that might derive.
    async fn pursuit_row(
        repo: &SqliteBookingRepository,
        id: &EffectIntentId,
    ) -> (i64, i64, Option<i64>, Option<i64>) {
        let row = sqlx::query(
            r"
            SELECT attempts_started, attempts_finished, escalated_at_ms,
                   escalation_attempts
              FROM effect_intents WHERE effect_intent_id = ?
            ",
        )
        .bind(id.as_str())
        .fetch_one(repo.pool())
        .await
        .expect("the intent row exists");
        (
            row.get("attempts_started"),
            row.get("attempts_finished"),
            row.get("escalated_at_ms"),
            row.get("escalation_attempts"),
        )
    }

    /// Gate M2 — the lease fences, and expiry is recovery.
    ///
    /// The story: a worker takes Lucy's file, walks off with the key, and dies.
    /// The next worker MUST be able to take over — a dead owner cannot hold
    /// work hostage — and when the first worker's ghost wanders back, every
    /// write it makes with the key it died holding must open nothing.
    #[tokio::test]
    async fn an_expired_lease_reopens_and_the_stale_token_writes_nothing() {
        let temp = TempDir::new().expect("temp dir");
        let (repo, clock, effect) = claimable_world(&temp).await;

        let first = repo
            .claim_effect(&effect, 30_000)
            .await
            .expect("claim")
            .expect("a prepared intent is claimable");
        assert!(
            repo.note_attempt_started(&effect, first.token)
                .await
                .expect("note")
        );
        assert!(
            repo.claim_effect(&effect, 30_000)
                .await
                .expect("claim")
                .is_none(),
            "a live lease refuses a second claimant"
        );

        // The owner dies. Time passes; the lease expires.
        clock.advance(MAX_LEASE_MS + 1);
        let second = repo
            .claim_effect(&effect, 30_000)
            .await
            .expect("claim")
            .expect("an expired lease is recoverable — recovery is required, not refused");
        assert!(second.token > first.token, "a new claim is a NEW token");

        // The ghost's late writes, every kind of them: nothing matches.
        assert!(
            !repo
                .note_attempt_started(&effect, first.token)
                .await
                .expect("write"),
            "a stale token starts nothing"
        );
        assert!(
            !repo
                .note_attempt_finished(&effect, first.token, 5_000)
                .await
                .expect("write"),
            "a stale token finishes nothing"
        );
        assert_eq!(
            repo.mark_escalated(&effect, first.token, MAX_CADENCE_MS)
                .await
                .expect("write"),
            EscalationWrite::Noop,
            "a stale token escalates nothing"
        );
        repo.release_lease(&effect, first.token)
            .await
            .expect("release");
        assert!(
            repo.claim_effect(&effect, 30_000)
                .await
                .expect("claim")
                .is_none(),
            "the stale release did not free the live owner's lease"
        );

        let (started, finished, escalated_at, _) = pursuit_row(&repo, &effect).await;
        assert_eq!(
            (started, finished),
            (1, 0),
            "only the live owner's writes landed; the ghost's counted for nothing"
        );
        assert!(escalated_at.is_none());
    }

    /// Claiming is the gate IN THE SCHEMA (PR #15 review, HIGH): a freshly
    /// prepared row carries the default token `0`, and a caller passing that
    /// token without ever claiming must write nothing — not spend budget, not
    /// escalate. An implementation whose writes check only `(id, token)`
    /// accepts all three of these and fails here.
    #[tokio::test]
    async fn nothing_writes_without_claiming_first() {
        let temp = TempDir::new().expect("temp dir");
        let (repo, _clock, effect) = claimable_world(&temp).await;

        assert!(
            !repo.note_attempt_started(&effect, 0).await.expect("write"),
            "the unclaimed row's own default token starts nothing"
        );
        assert!(
            !repo
                .note_attempt_finished(&effect, 0, 5_000)
                .await
                .expect("write"),
            "and finishes nothing"
        );
        assert_eq!(
            repo.mark_escalated(&effect, 0, MAX_CADENCE_MS)
                .await
                .expect("write"),
            EscalationWrite::Noop,
            "and escalates nothing"
        );
        let (started, finished, escalated_at, _) = pursuit_row(&repo, &effect).await;
        assert_eq!((started, finished, escalated_at), (0, 0, None));

        // The same row, claimed: the same writes now land — the refusals above
        // were about the missing claim, not about the row.
        let claimed = repo
            .claim_effect(&effect, 30_000)
            .await
            .expect("claim")
            .expect("claimable");
        assert!(
            repo.note_attempt_started(&effect, claimed.token)
                .await
                .expect("note")
        );
    }

    /// The turn's own release closes the door behind it: once the owner gives
    /// the lease back, its token writes nothing further. Without this, a
    /// replayed finish after release would double-count a conversation.
    #[tokio::test]
    async fn a_released_token_writes_nothing_further() {
        let temp = TempDir::new().expect("temp dir");
        let (repo, _clock, effect) = claimable_world(&temp).await;

        let claimed = repo
            .claim_effect(&effect, 30_000)
            .await
            .expect("claim")
            .expect("claimable");
        assert!(
            repo.note_attempt_started(&effect, claimed.token)
                .await
                .expect("note")
        );
        assert!(
            repo.note_attempt_finished(&effect, claimed.token, 5_000)
                .await
                .expect("note")
        );
        repo.release_lease(&effect, claimed.token)
            .await
            .expect("release");

        assert!(
            !repo
                .note_attempt_finished(&effect, claimed.token, 5_000)
                .await
                .expect("write"),
            "the turn is over; a replayed finish counts nothing"
        );
        assert_eq!(
            repo.mark_escalated(&effect, claimed.token, MAX_CADENCE_MS)
                .await
                .expect("write"),
            EscalationWrite::Noop,
            "and a post-release escalation writes nothing"
        );
        let (started, finished, escalated_at, _) = pursuit_row(&repo, &effect).await;
        assert_eq!((started, finished, escalated_at), (1, 1, None));
    }

    /// The decided edge (PR #15 review, HIGH 2's untested leg): between a
    /// lease's EXPIRY and the next claim, the token's owner may still finish
    /// its own record. It is the only owner ever issued that token, the write
    /// records a call that truly happened, and the fence a takeover needs is
    /// the token bump — atomic with every claim, proven above. Refusing here
    /// would discard true accounting to defend against nobody.
    #[tokio::test]
    async fn an_expired_but_unclaimed_lease_still_records_its_own_finish() {
        let temp = TempDir::new().expect("temp dir");
        let (repo, clock, effect) = claimable_world(&temp).await;

        let claimed = repo
            .claim_effect(&effect, 30_000)
            .await
            .expect("claim")
            .expect("claimable");
        assert!(
            repo.note_attempt_started(&effect, claimed.token)
                .await
                .expect("note")
        );

        // The lease expires with the owner still alive — a slow council call,
        // not a crash — and NOBODY has claimed the row yet.
        clock.advance(MAX_LEASE_MS + 1);
        assert!(
            repo.note_attempt_finished(&effect, claimed.token, 5_000)
                .await
                .expect("write"),
            "the sole owner's true accounting lands: the call did return"
        );

        // The moment anyone takes over, that same token is dead — the fence is
        // the bump, not the clock. (The finish above scheduled a REAL cadence
        // — since ADR-021's repair it actually gates — so the takeover waits
        // it out like any honest worker.)
        clock.advance(5_001);
        let successor = repo
            .claim_effect(&effect, 30_000)
            .await
            .expect("claim")
            .expect("expired and past the cadence means claimable");
        assert!(successor.token > claimed.token);
        assert!(
            !repo
                .note_attempt_finished(&effect, claimed.token, 5_000)
                .await
                .expect("write"),
            "outlived by a successor, the old token writes nothing"
        );
    }

    /// The cadence is a DURATION and the schedule is absolute — pinned to the
    /// clock, both sides of the boundary (ADR-021; the repair of the defect
    /// that shipped with slice E, where the stored "next attempt" was
    /// milliseconds after 1970 and every retry was instantly due). This is the
    /// assertion every earlier test skipped by advancing its clock first.
    #[tokio::test]
    async fn a_finished_attempt_schedules_the_next_one_a_cadence_away() {
        let temp = TempDir::new().expect("temp dir");
        let (repo, clock, effect) = claimable_world(&temp).await;

        let claimed = repo
            .claim_effect(&effect, 30_000)
            .await
            .expect("claim")
            .expect("claimable");
        assert!(
            repo.note_attempt_started(&effect, claimed.token)
                .await
                .expect("note")
        );
        assert!(
            repo.note_attempt_finished(&effect, claimed.token, 5_000)
                .await
                .expect("note")
        );
        repo.release_lease(&effect, claimed.token)
            .await
            .expect("release");

        // The stored schedule, exactly: now + cadence — never the bare cadence.
        let stored: i64 = sqlx::query_scalar(
            "SELECT next_attempt_after_ms FROM effect_intents WHERE effect_intent_id = ?",
        )
        .bind(effect.as_str())
        .fetch_one(repo.pool())
        .await
        .expect("row");
        assert_eq!(stored, T0 + 5_000, "an absolute moment, not a 1970 offset");

        // NOT due one tick before the cadence elapses...
        clock.set(T0 + 4_999);
        assert!(
            repo.due_effects(10).await.expect("due").is_empty(),
            "the cadence actually gates: not due yet"
        );
        assert!(
            repo.claim_effect(&effect, 30_000)
                .await
                .expect("claim")
                .is_none(),
            "and the claim's own recheck agrees"
        );
        // ...due exactly when it does.
        clock.set(T0 + 5_000);
        assert_eq!(
            repo.due_effects(10).await.expect("due"),
            vec![effect.clone()]
        );

        // The retry hint is the same durable schedule, under the store's clock.
        clock.set(T0 + 1_000);
        assert_eq!(
            repo.retry_hint_ms(&effect).await.expect("hint"),
            Some(4_000),
            "Retry-After projects the row, not a constant"
        );
        clock.set(T0 + 9_000);
        assert_eq!(
            repo.retry_hint_ms(&effect).await.expect("hint"),
            Some(0),
            "a schedule already passed hints zero, never negative"
        );
        assert_eq!(
            repo.retry_hint_ms(&EffectIntentId::new("EFF-NOBODY"))
                .await
                .expect("hint"),
            None,
            "no hint for an identity the store does not chase"
        );
    }

    /// Gate M4's replay half — escalation is written ONCE. A second write with
    /// the same live token, later, moves nothing: not the timestamp, not the
    /// derived count, and the human queue still holds exactly one question.
    #[tokio::test]
    async fn escalation_is_recorded_once_and_a_replay_is_a_noop() {
        let temp = TempDir::new().expect("temp dir");
        let (repo, clock, effect) = claimable_world(&temp).await;

        let claimed = repo
            .claim_effect(&effect, 30_000)
            .await
            .expect("claim")
            .expect("claimable");
        assert_eq!(
            repo.mark_escalated(&effect, claimed.token, MAX_CADENCE_MS)
                .await
                .expect("mark"),
            EscalationWrite::Recorded
        );
        let (_, _, first_at, first_count) = pursuit_row(&repo, &effect).await;
        assert_eq!(first_at, Some(T0), "escalated at the moment of giving up");

        // Later — so a second write WOULD move the timestamp if it landed.
        clock.advance(10_000);
        assert_eq!(
            repo.mark_escalated(&effect, claimed.token, MAX_CADENCE_MS)
                .await
                .expect("mark"),
            EscalationWrite::Noop
        );
        let (_, _, second_at, second_count) = pursuit_row(&repo, &effect).await;
        assert_eq!(second_at, first_at, "the marker did not move");
        assert_eq!(second_count, first_count, "the derived count did not move");
        assert_eq!(
            repo.escalated_unresolved(10).await.expect("queue"),
            vec![effect.clone()],
            "one question in the queue, not two"
        );
    }

    /// Gate M17's rollback half — the skew clamp (spec §3.4). Escalation
    /// schedules the next ask an hour out; if the clock then winds BACK, that
    /// schedule is suddenly hours in the future and a plain `<= now` would
    /// silence the chase for the rollback's whole length. The clamp treats
    /// "scheduled further out than the longest legal cadence" as due — no live
    /// code writes such a value, so it can only mean the clock moved, and the
    /// honest response to a moved clock is to go ask.
    #[tokio::test]
    async fn a_clock_rollback_cannot_silence_the_chase() {
        let temp = TempDir::new().expect("temp dir");
        let (repo, clock, effect) = claimable_world(&temp).await;

        let claimed = repo
            .claim_effect(&effect, 30_000)
            .await
            .expect("claim")
            .expect("claimable");
        assert_eq!(
            repo.mark_escalated(&effect, claimed.token, MAX_CADENCE_MS)
                .await
                .expect("mark"),
            EscalationWrite::Recorded
        );
        repo.release_lease(&effect, claimed.token)
            .await
            .expect("release");

        // Sanity: at the long cadence, not yet due.
        assert!(
            repo.due_effects(10).await.expect("due").is_empty(),
            "escalated means slower, and the hour is not up"
        );

        // The rollback: two hours backwards. The scheduled ask is now three
        // hours in the future — an impossible value, which is the signal.
        clock.set(T0 - 2 * MAX_CADENCE_MS);
        assert_eq!(
            repo.due_effects(10).await.expect("due"),
            vec![effect.clone()],
            "the clamp surfaces the intent the rollback tried to bury"
        );
        // And the claim's own atomic recheck agrees — a turn could actually run.
        assert!(
            repo.claim_effect(&effect, 30_000)
                .await
                .expect("claim")
                .is_some(),
            "claimable under the same clamp, so the chase continues"
        );
    }
}

#[cfg(test)]
mod migration_gate {
    //! Gate M18 — migration 0004 meets a pre-ADR-019 database.
    //!
    //! Two doors: a database holding the retired `Abandoned` status is REFUSED
    //! at open (this build cannot carry what that value claims), and a lying
    //! pre-E `Prepared` row — one an old Phase B may have called the council
    //! about — is reopened as `Unknown` with its budget honestly spent.

    use super::*;
    use std::borrow::Cow;
    use tempfile::TempDir;

    /// The world as a pre-slice-E build left it: schema at migration 0003,
    /// built by the REAL migrator so it cannot drift from what 0004 actually
    /// meets, with whatever rows the test plants on top.
    async fn pre_e_database(temp: &TempDir) -> std::path::PathBuf {
        let path = temp.path().join("townhall.sqlite");
        let pool = SqlitePoolOptions::new()
            .max_connections(1)
            .connect_with(
                SqliteConnectOptions::new()
                    .filename(&path)
                    .create_if_missing(true),
            )
            .await
            .expect("open the fixture database");
        let up_to_0003 = sqlx::migrate::Migrator {
            migrations: Cow::Owned(
                MIGRATOR
                    .migrations
                    .iter()
                    .filter(|migration| migration.version <= 3)
                    .cloned()
                    .collect(),
            ),
            ..MIGRATOR
        };
        up_to_0003.run(&pool).await.expect("migrate to 0003");
        pool.close().await;
        path
    }

    /// Plant one booking and one effect row in the pre-E schema. The JSON
    /// payloads are placeholders on purpose: both tests read raw columns and
    /// neither loads an aggregate, so a decodable payload would only disguise
    /// what the fixture is — a foreign database this code must judge, not use.
    async fn plant(path: &std::path::Path, status: &str, active: bool) {
        let pool = SqlitePoolOptions::new()
            .max_connections(1)
            .connect_with(SqliteConnectOptions::new().filename(path))
            .await
            .expect("reopen the fixture");
        sqlx::query(
            r"
            INSERT INTO bookings (id, version, state_name, state_json,
                                  requirements_json, active_effect,
                                  created_at_ms, updated_at_ms)
            VALUES ('BKG-OLD', 3, 'BookingInProgress', '{}', '{}', ?, 0, 0)
            ",
        )
        .bind(active.then_some("EFF-OLD"))
        .execute(&pool)
        .await
        .expect("plant the booking");
        sqlx::query(
            r"
            INSERT INTO effect_intents (effect_intent_id, booking_id,
                                        operation_kind, source_version,
                                        canonical_plan_json, status,
                                        expires_at_ms, created_at_ms,
                                        updated_at_ms)
            VALUES ('EFF-OLD', 'BKG-OLD', 'Book', 3, '{}', ?, 0, 0, 0)
            ",
        )
        .bind(status)
        .execute(&pool)
        .await
        .expect("plant the intent");
        pool.close().await;
    }

    /// An `Abandoned` row is a decision this schema no longer carries, and the
    /// originating state it destroyed cannot be recovered automatically —
    /// so the open REFUSES, with the recovery route in the message (ADR-019).
    #[tokio::test]
    async fn a_database_holding_abandoned_rows_is_refused_at_open() {
        let temp = TempDir::new().expect("temp dir");
        let path = pre_e_database(&temp).await;
        plant(&path, "Abandoned", true).await;

        let refused = SqliteBookingRepository::open(&path)
            .await
            .expect_err("a pre-ADR-019 database must be refused, not translated");
        assert!(
            matches!(refused, StoreError::AbandonedRowsPresent { count: 1 }),
            "expected AbandonedRowsPresent, got {refused:?}"
        );
        assert!(
            refused.to_string().contains("ADR-019"),
            "the refusal names its reasoning: {refused}"
        );
    }

    /// A pre-E `Prepared` row joined to a booking's active effect may LIE —
    /// the old Phase B could call the council, time out, and leave the row
    /// claiming "never attempted". 0004 reopens it as `Unknown` with one
    /// attempt spent: conservative in the safe direction, because `Unknown`
    /// gets re-asked and a false "never attempted" does not.
    #[tokio::test]
    async fn a_lying_prepared_row_reopens_as_unknown_with_budget_spent() {
        use sqlx::Row as _;
        let temp = TempDir::new().expect("temp dir");
        let path = pre_e_database(&temp).await;
        plant(&path, "Prepared", true).await;

        let repo = SqliteBookingRepository::open(&path)
            .await
            .expect("an Abandoned-free database migrates");
        let row = sqlx::query(
            "SELECT status, attempts_started FROM effect_intents \
             WHERE effect_intent_id = 'EFF-OLD'",
        )
        .fetch_one(repo.pool())
        .await
        .expect("the planted row survived");
        assert_eq!(row.get::<String, _>("status"), "Unknown");
        assert_eq!(row.get::<i64, _>("attempts_started"), 1);
    }

    /// The backfill's predicate is the JOIN, not the status: a `Prepared` row
    /// NO booking actively waits on is a plain never-attempted intent, and
    /// rewriting it would invent an attempt that never happened.
    #[tokio::test]
    async fn a_prepared_row_nothing_waits_on_is_left_alone() {
        use sqlx::Row as _;
        let temp = TempDir::new().expect("temp dir");
        let path = pre_e_database(&temp).await;
        plant(&path, "Prepared", false).await;

        let repo = SqliteBookingRepository::open(&path)
            .await
            .expect("migrates");
        let row = sqlx::query(
            "SELECT status, attempts_started FROM effect_intents \
             WHERE effect_intent_id = 'EFF-OLD'",
        )
        .fetch_one(repo.pool())
        .await
        .expect("the planted row survived");
        assert_eq!(
            row.get::<String, _>("status"),
            "Prepared",
            "never attempted stays never attempted"
        );
        assert_eq!(row.get::<i64, _>("attempts_started"), 0);
    }
}
