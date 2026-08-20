//! The effect registry: one row per identity, four states, and the write
//! discipline that makes absence safe.
//!
//! # The property everything here exists for
//!
//! *Creating* an effect and *settling it absent* must be mutually exclusive.
//! Nothing else in ADR-016 matters if that fails, and nothing else is needed if it
//! holds. Two things buy it, and neither is the clock:
//!
//! - **Both are write transactions.** `BEGIN IMMEDIATE` serialises writers, so a
//!   lookup cannot answer "nothing was created" while a create for that identity
//!   is still landing. Whichever takes the lock first decides.
//! - **What it decided is terminal.** The loser is refused by the recorded state,
//!   not by a clock reading — so a clock that steps backwards, or a commit that
//!   lands a moment late, cannot reverse it.
//!
//! That is why the lookup is a write. If it were ever optimised into a read for
//! the common case, the exclusion would silently evaporate while every test still
//! passed. `BEGIN IMMEDIATE` excludes competing *writers*; in WAL mode it does not
//! exclude readers, so "the lookup is a writer" is load-bearing rather than
//! incidental.
//!
//! # Where the deadline is read
//!
//! Once, from [`crate::clock::Clock`], *after* the writer lock is held. A request
//! that queued is therefore judged on when it reached the write rather than on
//! when it arrived — which is the whole content of ADR-016 §1. It is not a claim
//! that the commit is punctual; a paused transaction can still land late, and that
//! is harmless for the two reasons above.

use crate::{
    clock::Clock,
    pause::{PausePoint, Pauses},
};
use council_wire::{BookingFacts, CouncilSigner, EffectOutcome, GrantClaims};
use sqlx::{Row, Sqlite, SqlitePool, Transaction};
use std::sync::Arc;

/// What a create asks the council to do.
pub struct CreateBooking {
    pub effect_intent_id: String,
    pub expires_at_ms: i64,
    pub venue_id: String,
    pub slot_id: String,
    pub attendees: u16,
    /// The fee *we* believe applies.
    ///
    /// An assertion, not an instruction. It is checked against the catalogue and
    /// the booking is refused on disagreement — the council will not book at a
    /// price the caller made up. What lands in the row is always the catalogue's
    /// value, so a response later signs the council's number even when the two
    /// agree.
    pub asserted_fee_pence: u64,
    pub principal: String,
    /// The council's own warrant for the availability facts this plan was built
    /// on. Opaque to the caller, read back here.
    pub grant: String,
}

pub struct ApplyCancellation {
    pub effect_intent_id: String,
    pub expires_at_ms: i64,
    pub booking_reference: String,
}

/// A lookup: what became of this identity?
pub struct ResolveEffect {
    pub effect_intent_id: String,
    pub expires_at_ms: i64,
    pub kind: OperationKind,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum OperationKind {
    Book,
    Cancel,
}

impl OperationKind {
    const fn name(self) -> &'static str {
        match self {
            Self::Book => "Book",
            Self::Cancel => "Cancel",
        }
    }

    fn parse(raw: &str) -> Option<Self> {
        match raw {
            "Book" => Some(Self::Book),
            "Cancel" => Some(Self::Cancel),
            _ => None,
        }
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum State {
    Open,
    Created,
    Absent,
    Rejected,
}

impl State {
    const fn name(self) -> &'static str {
        match self {
            Self::Open => "Open",
            Self::Created => "Created",
            Self::Absent => "Absent",
            Self::Rejected => "Rejected",
        }
    }

    fn parse(raw: &str) -> Option<Self> {
        match raw {
            "Open" => Some(Self::Open),
            "Created" => Some(Self::Created),
            "Absent" => Some(Self::Absent),
            "Rejected" => Some(Self::Rejected),
            _ => None,
        }
    }
}

struct EffectRow {
    kind: OperationKind,
    expires_at_ms: i64,
    state: State,
    booking_reference: Option<String>,
    reason: Option<String>,
}

#[derive(Debug, thiserror::Error)]
pub enum CouncilError {
    #[error(transparent)]
    Sqlx(#[from] sqlx::Error),
    #[error(transparent)]
    Migration(#[from] sqlx::migrate::MigrateError),
    #[error("a stored row holds {field} = {value:?}, which is not a value this council writes")]
    Unreadable { field: &'static str, value: String },
    #[error("a guarded terminal write affected {rows} rows, not 1")]
    NotOpen { rows: u64 },
    #[error(transparent)]
    Wire(#[from] council_wire::codec::CodecError),
}

static MIGRATOR: sqlx::migrate::Migrator = sqlx::migrate!("./migrations");

pub struct Registry {
    pool: SqlitePool,
    clock: Arc<dyn Clock>,
    signer: Arc<CouncilSigner>,
    pauses: Arc<dyn Pauses>,
    /// How long an availability observation stays current, by the council's clock.
    availability_ttl_ms: i64,
}

impl Registry {
    /// # Errors
    /// [`CouncilError::Sqlx`] if the pool cannot connect, [`CouncilError::Migration`]
    /// if migrations fail.
    pub async fn open(
        pool: SqlitePool,
        clock: Arc<dyn Clock>,
        signer: Arc<CouncilSigner>,
        pauses: Arc<dyn Pauses>,
        availability_ttl_ms: i64,
    ) -> Result<Self, CouncilError> {
        MIGRATOR.run(&pool).await?;
        Ok(Self {
            pool,
            clock,
            signer,
            pauses,
            availability_ttl_ms,
        })
    }

    pub fn pool(&self) -> &SqlitePool {
        &self.pool
    }

    pub fn signer(&self) -> &CouncilSigner {
        &self.signer
    }

    /// Seed the catalogue. Council-side setup, not a request path.
    ///
    /// # Errors
    /// [`CouncilError::Sqlx`] on a write failure.
    pub async fn seed_slot(
        &self,
        venue_id: &str,
        slot_id: &str,
        fee_pence: u64,
        capacity: u16,
        accessible: bool,
        available: bool,
    ) -> Result<(), CouncilError> {
        sqlx::query(
            r"
            INSERT INTO venue_slots
                (venue_id, slot_id, fee_pence, capacity, accessible, available, row_version)
            VALUES (?, ?, ?, ?, ?, ?, 1)
            ON CONFLICT(venue_id, slot_id) DO UPDATE SET
                fee_pence = excluded.fee_pence,
                capacity = excluded.capacity,
                accessible = excluded.accessible,
                available = excluded.available
            ",
        )
        .bind(venue_id)
        .bind(slot_id)
        .bind(i64::try_from(fee_pence).unwrap_or(i64::MAX))
        .bind(i64::from(capacity))
        .bind(i64::from(accessible))
        .bind(i64::from(available))
        .execute(&self.pool)
        .await?;
        Ok(())
    }

    /// The current facts for one slot, with a warrant for them.
    ///
    /// # Errors
    /// [`CouncilError::Sqlx`] on a read failure, [`CouncilError::Wire`] if a field
    /// exceeds the wire's limits.
    pub async fn availability(
        &self,
        venue_id: &str,
        slot_id: &str,
    ) -> Result<Option<AvailabilityAnswer>, CouncilError> {
        let row = sqlx::query(
            r"
            SELECT fee_pence, capacity, accessible, available, row_version
              FROM venue_slots
             WHERE venue_id = ? AND slot_id = ?
            ",
        )
        .bind(venue_id)
        .bind(slot_id)
        .fetch_optional(&self.pool)
        .await?;

        let Some(row) = row else { return Ok(None) };

        let valid_until_ms = self.clock.now_ms().saturating_add(self.availability_ttl_ms);
        let row_version: i64 = row.try_get("row_version")?;
        let grant = self.signer.mint_grant(&GrantClaims {
            venue_id: venue_id.to_owned(),
            slot_id: slot_id.to_owned(),
            row_version: u64::try_from(row_version).unwrap_or(0),
            valid_until_ms,
        })?;

        let capacity: i64 = row.try_get("capacity")?;
        let fee: i64 = row.try_get("fee_pence")?;
        Ok(Some(AvailabilityAnswer {
            capacity: u16::try_from(capacity).unwrap_or(u16::MAX),
            accessible: row.try_get::<i64, _>("accessible")? == 1,
            available: row.try_get::<i64, _>("available")? == 1,
            fee_pence: u64::try_from(fee).unwrap_or(0),
            grant,
            valid_until_ms,
        }))
    }

    /// Create a booking, idempotently, for one effect identity.
    ///
    /// # Errors
    /// [`CouncilError`] on a storage failure or an unreadable stored row.
    pub async fn create_booking(
        &self,
        request: &CreateBooking,
    ) -> Result<EffectOutcome, CouncilError> {
        let mut tx = self.pool.begin_with("BEGIN IMMEDIATE").await?;

        if let Some(settled) = self
            .classify_existing(
                &mut tx,
                &request.effect_intent_id,
                request.expires_at_ms,
                OperationKind::Book,
            )
            .await?
        {
            tx.commit().await?;
            return Ok(settled);
        }

        // Paused here, a test can move the clock past the deadline *after* this
        // request was accepted. A council that judged expiry on arrival has
        // already passed its check and writes anyway.
        self.pauses
            .reach(PausePoint::BeforeExpiryWrite, &request.effect_intent_id)
            .await;

        // The one clock, read after the writer lock (ADR-016 §1).
        let now = self.clock.now_ms();

        self.ensure_open(
            &mut tx,
            &request.effect_intent_id,
            OperationKind::Book,
            request.expires_at_ms,
            now,
        )
        .await?;

        if now > request.expires_at_ms {
            let outcome = self
                .settle(
                    &mut tx,
                    &request.effect_intent_id,
                    OperationKind::Book,
                    Settlement::Absent,
                    now,
                )
                .await?;
            self.commit_settled(tx, &request.effect_intent_id).await?;
            return Ok(outcome);
        }

        // The grant is the council reading back its own token: signature and slot
        // here, then — in the write predicate below — the row version and the
        // grant's own validity deadline against the clock reading above.
        let claims = match self.opened_grant(request) {
            Ok(claims) => claims,
            Err(reason) => {
                let outcome = self
                    .settle(
                        &mut tx,
                        &request.effect_intent_id,
                        OperationKind::Book,
                        Settlement::Rejected(reason),
                        now,
                    )
                    .await?;
                self.commit_settled(tx, &request.effect_intent_id).await?;
                return Ok(outcome);
            }
        };

        let settlement = self.attempt_booking(&mut tx, request, &claims, now).await?;
        let outcome = self
            .settle(
                &mut tx,
                &request.effect_intent_id,
                OperationKind::Book,
                settlement,
                now,
            )
            .await?;

        self.commit_settled(tx, &request.effect_intent_id).await?;
        Ok(outcome)
    }

    /// The write itself: one statement that decides and mutates together.
    ///
    /// Every condition is the council's own. `fee_pence` is *read from* the
    /// catalogue and only *compared against* the caller's assertion, so what lands
    /// in the row — and what a response later signs — is the council's number even
    /// when the two agree. The row version and the grant's validity are here rather
    /// than in earlier Rust so they are evaluated against the same row the insert
    /// reads, under the same lock.
    ///
    /// Accessibility is deliberately absent. That is *our* requirement, not a
    /// council rule; the council's obligation is that the facts we decided on are
    /// still current, which `row_version` establishes. Checking it here would move
    /// our policy into the provider.
    async fn attempt_booking(
        &self,
        tx: &mut Transaction<'_, Sqlite>,
        request: &CreateBooking,
        claims: &GrantClaims,
        now: i64,
    ) -> Result<Settlement, CouncilError> {
        let reference = next_reference(tx).await?;
        let inserted = sqlx::query(
            r"
            INSERT INTO bookings
                (booking_reference, created_by, venue_id, slot_id,
                 attendees, fee_pence, principal, created_at_ms)
            SELECT ?, ?, v.venue_id, v.slot_id, ?, v.fee_pence, ?, ?
              FROM venue_slots v
             WHERE v.venue_id = ? AND v.slot_id = ?
               AND v.fee_pence = ?
               AND v.capacity >= ?
               AND v.available = 1
               AND v.row_version = ?
               AND ? <= ?
            ",
        )
        .bind(&reference)
        .bind(&request.effect_intent_id)
        .bind(i64::from(request.attendees))
        .bind(&request.principal)
        .bind(now)
        .bind(&request.venue_id)
        .bind(&request.slot_id)
        .bind(i64::try_from(request.asserted_fee_pence).unwrap_or(i64::MAX))
        .bind(i64::from(request.attendees))
        .bind(i64::try_from(claims.row_version).unwrap_or(i64::MAX))
        .bind(now)
        .bind(claims.valid_until_ms)
        .execute(&mut **tx)
        .await?;

        if inserted.rows_affected() == 1 {
            return Ok(Settlement::Created(reference));
        }

        // Zero rows means one of the council's own conditions refused this. Which
        // one is worth saying: `Rejected` with "refused" tells an operator nothing,
        // and the reason is what a lost response is later resolved with.
        Ok(Settlement::Rejected(
            self.why_refused(tx, request, claims, now).await?,
        ))
    }

    /// Cancel a booking, idempotently, under its own effect identity.
    ///
    /// # Errors
    /// [`CouncilError`] on a storage failure or an unreadable stored row.
    pub async fn apply_cancellation(
        &self,
        request: &ApplyCancellation,
    ) -> Result<EffectOutcome, CouncilError> {
        let mut tx = self.pool.begin_with("BEGIN IMMEDIATE").await?;

        if let Some(settled) = self
            .classify_existing(
                &mut tx,
                &request.effect_intent_id,
                request.expires_at_ms,
                OperationKind::Cancel,
            )
            .await?
        {
            tx.commit().await?;
            return Ok(settled);
        }

        self.pauses
            .reach(PausePoint::BeforeExpiryWrite, &request.effect_intent_id)
            .await;
        let now = self.clock.now_ms();

        self.ensure_open(
            &mut tx,
            &request.effect_intent_id,
            OperationKind::Cancel,
            request.expires_at_ms,
            now,
        )
        .await?;

        let settlement = if now > request.expires_at_ms {
            Settlement::Absent
        } else {
            let target =
                sqlx::query("SELECT cancelled_by FROM bookings WHERE booking_reference = ?")
                    .bind(&request.booking_reference)
                    .fetch_optional(&mut *tx)
                    .await?;

            match target {
                // Nothing to cancel, and there never will be — this reference is
                // not one the council issued. Terminal, so a lost response
                // resolves as the same rejection rather than as absence.
                None => {
                    Settlement::Rejected(format!("no booking {} exists", request.booking_reference))
                }
                Some(row) => {
                    let cancelled_by: Option<String> = row.try_get("cancelled_by")?;
                    match cancelled_by {
                        None => {
                            sqlx::query(
                                "UPDATE bookings SET cancelled_by = ? WHERE booking_reference = ?",
                            )
                            .bind(&request.effect_intent_id)
                            .bind(&request.booking_reference)
                            .execute(&mut *tx)
                            .await?;
                            Settlement::Created(request.booking_reference.clone())
                        }
                        // Already cancelled under *this* identity: the ordinary
                        // replay path, reached when the row exists but the
                        // effect row did not settle.
                        Some(existing) if existing == request.effect_intent_id => {
                            Settlement::Created(request.booking_reference.clone())
                        }
                        // Cancelled by someone else. `CancellationExists` for
                        // this identity would be a lie: `cancelled_by` names the
                        // other one, and this identity did nothing.
                        Some(existing) => Settlement::Rejected(format!(
                            "booking {} was already cancelled by {existing}",
                            request.booking_reference
                        )),
                    }
                }
            }
        };

        let outcome = self
            .settle(
                &mut tx,
                &request.effect_intent_id,
                OperationKind::Cancel,
                settlement,
                now,
            )
            .await?;
        self.commit_settled(tx, &request.effect_intent_id).await?;
        Ok(outcome)
    }

    /// What became of this identity — and, past its deadline, close it.
    ///
    /// A write, not a read. See this module's header.
    ///
    /// # Errors
    /// [`CouncilError`] on a storage failure or an unreadable stored row.
    pub async fn resolve(&self, request: &ResolveEffect) -> Result<EffectOutcome, CouncilError> {
        let mut tx = self.pool.begin_with("BEGIN IMMEDIATE").await?;

        if let Some(settled) = self
            .classify_existing(
                &mut tx,
                &request.effect_intent_id,
                request.expires_at_ms,
                request.kind,
            )
            .await?
        {
            tx.commit().await?;
            return Ok(settled);
        }

        let now = self.clock.now_ms();

        // First sight binds the kind and the deadline, whatever the answer turns
        // out to be. An unbound deadline is exactly what would let a later,
        // shortened one manufacture absence.
        self.ensure_open(
            &mut tx,
            &request.effect_intent_id,
            request.kind,
            request.expires_at_ms,
            now,
        )
        .await?;

        // Before the deadline there is nothing to say beyond "not yet".
        if now <= request.expires_at_ms {
            tx.commit().await?;
            return Ok(EffectOutcome::NotYetVisible);
        }

        let outcome = self
            .settle(
                &mut tx,
                &request.effect_intent_id,
                request.kind,
                Settlement::Absent,
                now,
            )
            .await?;
        self.commit_settled(tx, &request.effect_intent_id).await?;
        Ok(outcome)
    }

    /// Read back the grant, checking what can be checked outside the write.
    ///
    /// The slot check belongs here rather than in the predicate: without it, a
    /// warrant for a cheap accessible room would vouch for the booking of any
    /// other. The row version and validity go into the statement instead, so they
    /// are evaluated against the same row the insert reads.
    fn opened_grant(&self, request: &CreateBooking) -> Result<GrantClaims, String> {
        let claims = self
            .signer
            .open_grant(&request.grant)
            .map_err(|error| format!("the availability grant is not ours: {error}"))?;

        if claims.venue_id != request.venue_id || claims.slot_id != request.slot_id {
            return Err(format!(
                "the grant is for {}/{}, not {}/{}",
                claims.venue_id, claims.slot_id, request.venue_id, request.slot_id
            ));
        }
        Ok(claims)
    }

    /// Everything that can be decided from the existing row alone.
    ///
    /// `Ok(None)` means "not settled, carry on" — either no row, or an `Open` one
    /// whose deadline the caller must now weigh.
    async fn classify_existing(
        &self,
        tx: &mut Transaction<'_, Sqlite>,
        effect_intent_id: &str,
        expires_at_ms: i64,
        kind: OperationKind,
    ) -> Result<Option<EffectOutcome>, CouncilError> {
        let Some(row) = load_effect(tx, effect_intent_id).await? else {
            return Ok(None);
        };

        // The bindings are checked before the state, because a request that
        // contradicts them is not a retry of anything — answering it from the
        // stored outcome would be answering a different question.
        if row.kind != kind {
            return Ok(Some(EffectOutcome::ProtocolConflict {
                reason: format!(
                    "{effect_intent_id} is bound as {}, not {}",
                    row.kind.name(),
                    kind.name()
                ),
            }));
        }
        if row.expires_at_ms != expires_at_ms {
            return Ok(Some(EffectOutcome::ProtocolConflict {
                reason: format!(
                    "{effect_intent_id} is bound to deadline {}, not {expires_at_ms}",
                    row.expires_at_ms
                ),
            }));
        }

        match row.state {
            State::Open => Ok(None),
            State::Absent => Ok(Some(EffectOutcome::DefinitivelyAbsent)),
            State::Rejected => Ok(Some(EffectOutcome::ProviderRejected {
                reason: row.reason.unwrap_or_else(|| "refused".to_owned()),
            })),
            State::Created => {
                let reference = row.booking_reference.ok_or(CouncilError::Unreadable {
                    field: "booking_reference",
                    value: "NULL on a Created row".to_owned(),
                })?;
                Ok(Some(self.created_outcome(tx, kind, reference).await?))
            }
        }
    }

    /// What a `Created` row means, which depends on the kind it is bound to.
    async fn created_outcome(
        &self,
        tx: &mut Transaction<'_, Sqlite>,
        kind: OperationKind,
        booking_reference: String,
    ) -> Result<EffectOutcome, CouncilError> {
        if kind == OperationKind::Cancel {
            return Ok(EffectOutcome::CancellationApplied { booking_reference });
        }

        // Read the canonical facts from the booking row. The fee here came from
        // the catalogue at insert time, so what this signs is the council's number
        // rather than the caller's.
        let row = sqlx::query(
            r"
            SELECT venue_id, slot_id, attendees, fee_pence, principal
              FROM bookings WHERE booking_reference = ?
            ",
        )
        .bind(&booking_reference)
        .fetch_optional(&mut **tx)
        .await?
        .ok_or_else(|| CouncilError::Unreadable {
            field: "booking_reference",
            value: format!("{booking_reference} has no booking row"),
        })?;

        let attendees: i64 = row.try_get("attendees")?;
        let fee: i64 = row.try_get("fee_pence")?;
        Ok(EffectOutcome::BookingCreated(BookingFacts {
            booking_reference,
            venue_id: row.try_get("venue_id")?,
            slot_id: row.try_get("slot_id")?,
            attendees: u16::try_from(attendees).unwrap_or(u16::MAX),
            fee_pence: u64::try_from(fee).unwrap_or(0),
            principal: row.try_get("principal")?,
        }))
    }

    /// Write the `Open` row if this is first sight. Idempotent.
    async fn ensure_open(
        &self,
        tx: &mut Transaction<'_, Sqlite>,
        effect_intent_id: &str,
        kind: OperationKind,
        expires_at_ms: i64,
        now: i64,
    ) -> Result<(), CouncilError> {
        sqlx::query(
            r"
            INSERT INTO effects
                (effect_intent_id, operation_kind, expires_at_ms, state, first_seen_ms)
            VALUES (?, ?, ?, 'Open', ?)
            ON CONFLICT(effect_intent_id) DO NOTHING
            ",
        )
        .bind(effect_intent_id)
        .bind(kind.name())
        .bind(expires_at_ms)
        .bind(now)
        .execute(&mut **tx)
        .await?;
        Ok(())
    }

    /// Move an `Open` identity to a terminal state.
    ///
    /// Every path reaches this through [`Self::ensure_open`], so the row always
    /// exists by now. That ordering is not incidental: `bookings.created_by`
    /// references `effects`, so a booking written before its effect row violates
    /// the foreign key — the schema refuses to let the two get out of order.
    ///
    /// The `UPDATE` is guarded on `state = 'Open'`, so a terminal-to-terminal
    /// transition affects zero rows and is *reported* rather than silently applied.
    /// That is what makes a `Created` row structurally permanent: "discoverable
    /// forever" is a `WHERE` clause here, not a convention.
    async fn settle(
        &self,
        tx: &mut Transaction<'_, Sqlite>,
        effect_intent_id: &str,
        kind: OperationKind,
        settlement: Settlement,
        now: i64,
    ) -> Result<EffectOutcome, CouncilError> {
        let (state, reference, reason) = match &settlement {
            Settlement::Created(reference) => (State::Created, Some(reference.clone()), None),
            Settlement::Absent => (State::Absent, None, None),
            Settlement::Rejected(reason) => (State::Rejected, None, Some(reason.clone())),
        };

        let updated = sqlx::query(
            r"
            UPDATE effects
               SET state = ?, booking_reference = ?, reason = ?, settled_at_ms = ?
             WHERE effect_intent_id = ? AND state = 'Open'
            ",
        )
        .bind(state.name())
        .bind(reference.as_deref())
        .bind(reason.as_deref())
        .bind(now)
        .bind(effect_intent_id)
        .execute(&mut **tx)
        .await?;
        if updated.rows_affected() != 1 {
            return Err(CouncilError::NotOpen {
                rows: updated.rows_affected(),
            });
        }

        match settlement {
            Settlement::Created(reference) => self.created_outcome(tx, kind, reference).await,
            Settlement::Absent => Ok(EffectOutcome::DefinitivelyAbsent),
            Settlement::Rejected(reason) => Ok(EffectOutcome::ProviderRejected { reason }),
        }
    }

    /// Commit a settlement, with a pause either side.
    ///
    /// Killed before the commit, nothing is discoverable. Killed after it, the
    /// answer is reproducible but was never seen. Those are the two halves of
    /// commit-before-response, and a test reading state *after* the response
    /// arrives can prove neither.
    async fn commit_settled(
        &self,
        tx: Transaction<'_, Sqlite>,
        effect_intent_id: &str,
    ) -> Result<(), CouncilError> {
        self.pauses
            .reach(PausePoint::BeforeSettleCommit, effect_intent_id)
            .await;
        tx.commit().await?;
        self.pauses
            .reach(PausePoint::AfterSettleCommit, effect_intent_id)
            .await;
        Ok(())
    }

    /// Which of the council's conditions refused a create.
    ///
    /// Read after the fact rather than checked before it, so the decision itself
    /// stays one statement. This only builds the explanation.
    async fn why_refused(
        &self,
        tx: &mut Transaction<'_, Sqlite>,
        request: &CreateBooking,
        claims: &GrantClaims,
        now: i64,
    ) -> Result<String, CouncilError> {
        let row = sqlx::query(
            r"
            SELECT fee_pence, capacity, available, row_version
              FROM venue_slots WHERE venue_id = ? AND slot_id = ?
            ",
        )
        .bind(&request.venue_id)
        .bind(&request.slot_id)
        .fetch_optional(&mut **tx)
        .await?;

        let Some(row) = row else {
            return Ok(format!(
                "no slot {}/{} exists",
                request.venue_id, request.slot_id
            ));
        };

        let fee: i64 = row.try_get("fee_pence")?;
        let capacity: i64 = row.try_get("capacity")?;
        let available: i64 = row.try_get("available")?;
        let row_version: i64 = row.try_get("row_version")?;

        let asserted = i64::try_from(request.asserted_fee_pence).unwrap_or(i64::MAX);
        if fee != asserted {
            return Ok(format!(
                "the fee for {}/{} is {fee}, not the asserted {asserted}",
                request.venue_id, request.slot_id
            ));
        }
        if capacity < i64::from(request.attendees) {
            return Ok(format!(
                "{}/{} holds {capacity}, fewer than the {} requested",
                request.venue_id, request.slot_id, request.attendees
            ));
        }
        if available != 1 {
            return Ok(format!(
                "{}/{} is not available",
                request.venue_id, request.slot_id
            ));
        }
        if row_version != i64::try_from(claims.row_version).unwrap_or(i64::MAX) {
            return Ok(format!(
                "the availability grant is for version {} of {}/{}, which is now at {row_version}",
                claims.row_version, request.venue_id, request.slot_id
            ));
        }
        if now > claims.valid_until_ms {
            return Ok(format!(
                "the availability grant expired at {}",
                claims.valid_until_ms
            ));
        }
        Ok("the booking was refused".to_owned())
    }
}

enum Settlement {
    Created(String),
    Absent,
    Rejected(String),
}

pub struct AvailabilityAnswer {
    pub capacity: u16,
    pub accessible: bool,
    pub available: bool,
    pub fee_pence: u64,
    pub grant: String,
    pub valid_until_ms: i64,
}

async fn load_effect(
    tx: &mut Transaction<'_, Sqlite>,
    effect_intent_id: &str,
) -> Result<Option<EffectRow>, CouncilError> {
    let row = sqlx::query(
        r"
        SELECT operation_kind, expires_at_ms, state, booking_reference, reason
          FROM effects WHERE effect_intent_id = ?
        ",
    )
    .bind(effect_intent_id)
    .fetch_optional(&mut **tx)
    .await?;

    let Some(row) = row else { return Ok(None) };

    let kind_raw: String = row.try_get("operation_kind")?;
    let state_raw: String = row.try_get("state")?;
    Ok(Some(EffectRow {
        kind: OperationKind::parse(&kind_raw).ok_or(CouncilError::Unreadable {
            field: "operation_kind",
            value: kind_raw,
        })?,
        expires_at_ms: row.try_get("expires_at_ms")?,
        state: State::parse(&state_raw).ok_or(CouncilError::Unreadable {
            field: "state",
            value: state_raw,
        })?,
        booking_reference: row.try_get("booking_reference")?,
        reason: row.try_get("reason")?,
    }))
}

/// Mint the next booking reference, inside the caller's transaction.
///
/// Counted from the table under the writer lock rather than from an in-memory
/// counter, because a restarted council must not reissue a reference the previous
/// process already used.
async fn next_reference(tx: &mut Transaction<'_, Sqlite>) -> Result<String, CouncilError> {
    let count: i64 = sqlx::query("SELECT COUNT(*) AS n FROM bookings")
        .fetch_one(&mut **tx)
        .await?
        .try_get("n")?;
    Ok(format!("TH-{:05}", 90_001 + count))
}
