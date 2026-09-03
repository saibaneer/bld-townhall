//! The SQL side of `townhall-authority`'s ports.
//!
//! It lives here because this crate owns `sqlx` and the authority crate must not
//! name a connection pool (ADR-025). What it deliberately does NOT do is
//! interpret the delegation envelope: those bytes arrive from the issuer's own
//! codec and leave the same way. The columns beside them exist only so a `WHERE`
//! clause has something to stand on.
//!
//! # The one operation that must be a transaction
//!
//! `settle_with_grant` consumes a challenge and inserts its one grant. Written
//! as two statements it is a race: two concurrent correct replies both read
//! `pending`, both update, both insert, and one challenge yields two grants.
//! Here it is one transaction whose `UPDATE` carries `WHERE status = 'pending'`,
//! so the reply that loses matches no row and never reaches the `INSERT`. The
//! `delegations.challenge_id` UNIQUE index is the second lock on the same door.
//!
//! # Why every timestamp is a parameter
//!
//! No clock is reached for in this module. The verifier already takes `now_ms`
//! explicitly, so passing it down keeps one notion of "now" per operation —
//! rather than two that can disagree by however long a query took.

use crate::StoreError;
use bld_types::{ApprovalChallengeId, DelegationId, EvidenceReceiptId, PrincipalId, ServiceId};
use sqlx::{Row, SqlitePool};
use townhall_authority::{
    ApprovalCode, ApprovalStore, AssuranceLevel, BindingRef, BoundChannel, CanonicalScope,
    ChallengeRecord, ChallengeStatus, DelegationRecord, EvidenceReceipt, InboundEvidenceRecord,
    InsertOutcome, LoadedEvidence, MAX_ATTEMPTS, ScopeHash, Settled,
    store::StoreError as AuthorityStoreError,
};

/// A channel binding row.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ChannelBinding {
    pub id: String,
    pub address: String,
    pub principal: PrincipalId,
    pub version: u64,
    pub assurance: AssuranceLevel,
    pub withdrawn: bool,
}

impl ChannelBinding {
    /// The reference a challenge binds to: principal and revision, together.
    #[must_use]
    pub fn reference(&self) -> BindingRef {
        BindingRef {
            principal: self.principal.clone(),
            version: self.version,
        }
    }
}

/// `townhall-authority`'s ports, over SQLite.
#[derive(Clone, Debug)]
pub struct SqlApprovalStore {
    pool: SqlitePool,
}

impl SqlApprovalStore {
    #[must_use]
    pub const fn new(pool: SqlitePool) -> Self {
        Self { pool }
    }

    /// Bind an address to a principal at a stated assurance.
    ///
    /// # Errors
    /// The insert failed — most usefully, the partial unique index refused a
    /// second LIVE binding for one address.
    pub async fn bind_channel(
        &self,
        binding: &ChannelBinding,
        evidence: Option<&str>,
        at_ms: u64,
    ) -> Result<(), StoreError> {
        sqlx::query(
            r"
            INSERT INTO channel_bindings
                (id, address, principal, version, status, assurance, evidence,
                 verified_at_ms, created_at_ms, updated_at_ms)
            VALUES (?, ?, ?, ?, ?, ?, ?, ?, ?, ?)
            ",
        )
        .bind(&binding.id)
        .bind(&binding.address)
        .bind(binding.principal.as_str())
        .bind(as_i64(binding.version))
        .bind(if binding.withdrawn {
            "withdrawn"
        } else {
            "active"
        })
        .bind(binding.assurance.name())
        .bind(evidence)
        .bind(as_i64(at_ms))
        .bind(as_i64(at_ms))
        .bind(as_i64(at_ms))
        .execute(&self.pool)
        .await?;
        Ok(())
    }

    /// The live binding for an address, if any.
    ///
    /// # Why only the live one
    ///
    /// A withdrawn binding is history — kept so a challenge naming it can still
    /// explain its own refusal, never returned as an answer to "who is this
    /// number?". The predicate is positive (`status = 'active'`) for the reason
    /// migration 0005 recorded about ownership: a negation over a column that
    /// can be absent includes rows it should exclude.
    ///
    /// # Errors
    /// The query failed, or a row carries an assurance level that does not read.
    pub async fn live_binding(&self, address: &str) -> Result<Option<ChannelBinding>, StoreError> {
        let Some(row) = sqlx::query(
            r"
            SELECT id, address, principal, version, assurance
            FROM channel_bindings
            WHERE address = ? AND status = 'active'
            ",
        )
        .bind(address)
        .fetch_optional(&self.pool)
        .await?
        else {
            return Ok(None);
        };

        Self::decode_binding(&row).map(Some)
    }

    /// One binding row, shared by both lookups so neither can decode it
    /// differently from the other.
    fn decode_binding(row: &sqlx::sqlite::SqliteRow) -> Result<ChannelBinding, StoreError> {
        let id: String = row.try_get("id")?;
        let assurance_text: String = row.try_get("assurance")?;
        // An unreadable level is a refusal, never a default. Defaulting weak
        // un-authorizes a real grant; defaulting strong promotes a corrupt row.
        let assurance = AssuranceLevel::parse(&assurance_text).ok_or_else(|| {
            StoreError::CorruptRow(format!(
                "channel binding {id} carries an unreadable assurance level"
            ))
        })?;

        Ok(ChannelBinding {
            id,
            address: row.try_get("address")?,
            principal: PrincipalId::new(row.try_get::<String, _>("principal")?),
            version: from_i64(row.try_get::<i64, _>("version")?),
            assurance,
            withdrawn: false,
        })
    }

    /// The live binding for a PRINCIPAL, if any.
    ///
    /// # Why this exists beside `live_binding`
    ///
    /// That one answers "who is this number?", which is what an inbound message
    /// needs. This one answers "is this person's channel bound?", which is what
    /// a read gate needs before it will scope a query to them — so that a
    /// principal named in a header is checked against a row rather than
    /// believed.
    ///
    /// # Errors
    /// The query failed, or a row carries an assurance level that does not read.
    pub async fn live_binding_for(
        &self,
        principal: &PrincipalId,
    ) -> Result<Option<ChannelBinding>, StoreError> {
        let Some(row) = sqlx::query(
            r"
            SELECT id, address, principal, version, assurance
            FROM channel_bindings
            WHERE principal = ? AND status = 'active'
            ORDER BY version DESC
            ",
        )
        .bind(principal.as_str())
        .fetch_optional(&self.pool)
        .await?
        else {
            return Ok(None);
        };
        Self::decode_binding(&row).map(Some)
    }

    /// Raise a binding's revision — a re-verification, or a status change.
    ///
    /// Every pending challenge bound to the old revision stops being answerable
    /// the moment this returns, which is the whole point of the column.
    ///
    /// # Errors
    /// The update failed.
    pub async fn bump_binding(&self, id: &str, at_ms: u64) -> Result<Option<u64>, StoreError> {
        let row = sqlx::query(
            r"
            UPDATE channel_bindings
            SET version = version + 1, updated_at_ms = ?
            WHERE id = ?
            RETURNING version
            ",
        )
        .bind(as_i64(at_ms))
        .bind(id)
        .fetch_optional(&self.pool)
        .await?;

        row.map(|row| Ok(from_i64(row.try_get::<i64, _>("version")?)))
            .transpose()
    }

    /// Read one challenge row into the authority crate's record.
    fn decode_challenge(row: &sqlx::sqlite::SqliteRow) -> Result<ChallengeRecord, StoreError> {
        let id: String = row.try_get("id")?;

        let scope_bytes: Vec<u8> = row.try_get("scope")?;
        let scope = CanonicalScope::decode(&scope_bytes).ok_or_else(|| {
            StoreError::CorruptRow(format!("challenge {id} holds a scope that does not decode"))
        })?;

        let hash_text: String = row.try_get("scope_hash")?;
        let scope_hash = ScopeHash::parse_hex(&hash_text).ok_or_else(|| {
            StoreError::CorruptRow(format!("challenge {id} holds an unreadable scope digest"))
        })?;

        // The denormalized digest must still describe the stored scope.
        //
        // If they disagree, one of the two was edited, and choosing which to
        // believe would be choosing whose version of the approval stands. The
        // row is refused instead.
        if scope_hash != scope.digest() {
            return Err(StoreError::CorruptRow(format!(
                "challenge {id}'s stored digest does not describe its stored scope"
            )));
        }

        let code = ApprovalCode::new(row.try_get::<String, _>("code")?).ok_or_else(|| {
            StoreError::CorruptRow(format!("challenge {id} holds a malformed code"))
        })?;
        let status_text: String = row.try_get("status")?;
        let status = ChallengeStatus::parse(&status_text).ok_or_else(|| {
            StoreError::CorruptRow(format!("challenge {id} holds an unknown status"))
        })?;
        let assurance_text: String = row.try_get("assurance")?;
        let assurance = AssuranceLevel::parse(&assurance_text).ok_or_else(|| {
            StoreError::CorruptRow(format!(
                "challenge {id} holds an unreadable assurance level"
            ))
        })?;

        Ok(ChallengeRecord {
            id: ApprovalChallengeId::new(id),
            code,
            scope,
            scope_hash,
            binding: BindingRef {
                principal: PrincipalId::new(row.try_get::<String, _>("binding_principal")?),
                version: from_i64(row.try_get::<i64, _>("binding_version")?),
            },
            grantor: PrincipalId::new(row.try_get::<String, _>("grantor")?),
            subject: PrincipalId::new(row.try_get::<String, _>("subject")?),
            created_at_ms: from_i64(row.try_get::<i64, _>("created_at_ms")?),
            attempts_used: u8::try_from(row.try_get::<i64, _>("attempts_used")?)
                .unwrap_or(MAX_ATTEMPTS),
            status,
            assurance,
            actor: bld_types::ActorId::new(row.try_get::<String, _>("actor")?),
        })
    }

    /// What a challenge's state is, when an update matched no row.
    ///
    /// Read back rather than guessed: "no rows affected" means either the
    /// challenge does not exist or it was already settled, and those are
    /// different answers.
    async fn settled_state(
        &self,
        id: &ApprovalChallengeId,
    ) -> Result<Settled, AuthorityStoreError> {
        match self.load_challenge(id).await? {
            Some(challenge) => Ok(Settled::Already(challenge.status)),
            None => Err(AuthorityStoreError::UnknownChallenge),
        }
    }
}

#[async_trait::async_trait]
impl ApprovalStore for SqlApprovalStore {
    async fn insert_challenge(
        &self,
        challenge: &ChallengeRecord,
    ) -> Result<(), AuthorityStoreError> {
        sqlx::query(
            r"
            INSERT INTO approval_challenges
                (id, code, scope, scope_hash, binding_principal, binding_version,
                 grantor, subject, actor, assurance, status, attempts_used,
                 created_at_ms, expires_at_ms, settled_at_ms)
            VALUES (?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, NULL)
            ",
        )
        .bind(challenge.id.as_str())
        .bind(challenge.code.revealed())
        .bind(challenge.scope.encode())
        .bind(challenge.scope_hash.to_string())
        .bind(challenge.binding.principal.as_str())
        .bind(as_i64(challenge.binding.version))
        .bind(challenge.grantor.as_str())
        .bind(challenge.subject.as_str())
        .bind(challenge.actor.as_str())
        .bind(challenge.assurance.name())
        .bind(challenge.status.name())
        .bind(i64::from(challenge.attempts_used))
        .bind(as_i64(challenge.created_at_ms))
        .bind(as_i64(challenge.scope.expires_at_ms))
        .execute(&self.pool)
        .await
        .map_err(|error| {
            if is_unique_violation(&error) {
                AuthorityStoreError::ChallengeExists
            } else {
                unavailable(&error)
            }
        })?;
        Ok(())
    }

    async fn load_challenge(
        &self,
        id: &ApprovalChallengeId,
    ) -> Result<Option<ChallengeRecord>, AuthorityStoreError> {
        let row = sqlx::query(
            r"
            SELECT id, code, scope, scope_hash, binding_principal, binding_version,
                   grantor, subject, actor, assurance, status, attempts_used,
                   created_at_ms
            FROM approval_challenges
            WHERE id = ?
            ",
        )
        .bind(id.as_str())
        .fetch_optional(&self.pool)
        .await
        .map_err(|error| unavailable(&error))?;

        row.map(|row| {
            Self::decode_challenge(&row)
                .map_err(|error| AuthorityStoreError::Unavailable(error.to_string()))
        })
        .transpose()
    }

    async fn record_failed_attempt(
        &self,
        id: &ApprovalChallengeId,
        now_ms: u64,
    ) -> Result<(u8, ChallengeStatus), AuthorityStoreError> {
        // One statement, so two concurrent wrong guesses cannot share an
        // attempt, and `WHERE status = 'pending'` makes the increment
        // conditional on the challenge still being answerable at all.
        let row = sqlx::query(
            r"
            UPDATE approval_challenges
            SET attempts_used = attempts_used + 1,
                status = CASE WHEN attempts_used + 1 >= ? THEN 'exhausted' ELSE status END,
                settled_at_ms = CASE WHEN attempts_used + 1 >= ? THEN ? ELSE settled_at_ms END
            WHERE id = ? AND status = 'pending'
            RETURNING attempts_used, status
            ",
        )
        .bind(i64::from(MAX_ATTEMPTS))
        .bind(i64::from(MAX_ATTEMPTS))
        .bind(as_i64(now_ms))
        .bind(id.as_str())
        .fetch_optional(&self.pool)
        .await
        .map_err(|error| unavailable(&error))?;

        let Some(row) = row else {
            let challenge = self
                .load_challenge(id)
                .await?
                .ok_or(AuthorityStoreError::UnknownChallenge)?;
            return Ok((challenge.attempts_left(), challenge.status));
        };

        let used = u8::try_from(row.try_get::<i64, _>("attempts_used").unwrap_or(0))
            .unwrap_or(MAX_ATTEMPTS);
        let status_text: String = row.try_get("status").unwrap_or_default();
        let status = ChallengeStatus::parse(&status_text).ok_or_else(|| {
            AuthorityStoreError::Unavailable(format!("challenge {id} took on an unknown status"))
        })?;
        Ok((MAX_ATTEMPTS.saturating_sub(used), status))
    }

    async fn settle_with_grant(
        &self,
        id: &ApprovalChallengeId,
        grant: &DelegationRecord,
        receipt: &EvidenceReceiptId,
        address: &str,
        now_ms: u64,
    ) -> Result<Settled, AuthorityStoreError> {
        let mut transaction = self
            .pool
            .begin()
            .await
            .map_err(|error| unavailable(&error))?;

        // The conditional UPDATE is the lock. A reply that loses matches no row
        // here and never reaches the INSERT below.
        let claimed = sqlx::query(
            r"
            UPDATE approval_challenges
            SET status = 'approved', settled_at_ms = ?
            WHERE id = ? AND status = 'pending'
            ",
        )
        .bind(as_i64(grant.issued_at_ms))
        .bind(id.as_str())
        .execute(&mut *transaction)
        .await
        .map_err(|error| unavailable(&error))?
        .rows_affected();

        if claimed == 0 {
            transaction
                .rollback()
                .await
                .map_err(|error| unavailable(&error))?;
            return self.settled_state(id).await;
        }

        sqlx::query(
            r"
            INSERT INTO delegations
                (id, grantor, subject, service, challenge_id, expires_at_ms,
                 revoked_at_ms, envelope, created_at_ms)
            VALUES (?, ?, ?, ?, ?, ?, NULL, ?, ?)
            ",
        )
        .bind(grant.id.as_str())
        .bind(grant.grantor.as_str())
        .bind(grant.subject.as_str())
        .bind(grant.service.as_str())
        .bind(id.as_str())
        .bind(as_i64(grant.expires_at_ms))
        .bind(&grant.envelope)
        .bind(as_i64(grant.issued_at_ms))
        .execute(&mut *transaction)
        .await
        .map_err(|error| {
            if is_unique_violation(&error) {
                AuthorityStoreError::DelegationExists
            } else {
                unavailable(&error)
            }
        })?;

        // Spend the receipt under a one-use guard, in the same transaction that
        // claimed the challenge. Reaching here means we won the `status =
        // 'pending'` race, so the receipt bound to this challenge is unconsumed;
        // 0 rows is a contradiction (a spent receipt for a just-claimed
        // challenge), so the whole settlement rolls back rather than mint a grant
        // over evidence that was already spent.
        let spent = sqlx::query(
            r"
            UPDATE inbound_evidence
            SET consumed_at_ms = ?
            WHERE receipt = ? AND consumed_at_ms IS NULL
            ",
        )
        .bind(as_i64(now_ms))
        .bind(receipt.as_str())
        .execute(&mut *transaction)
        .await
        .map_err(|error| unavailable(&error))?
        .rows_affected();

        if spent == 0 {
            transaction
                .rollback()
                .await
                .map_err(|error| unavailable(&error))?;
            return Err(AuthorityStoreError::EvidenceSpent);
        }

        // The number is no longer awaiting a reply.
        sqlx::query("DELETE FROM awaiting_reply WHERE address = ?")
            .bind(address)
            .execute(&mut *transaction)
            .await
            .map_err(|error| unavailable(&error))?;

        transaction
            .commit()
            .await
            .map_err(|error| unavailable(&error))?;
        Ok(Settled::Now)
    }

    async fn settle_rejected(
        &self,
        id: &ApprovalChallengeId,
        receipt: &EvidenceReceiptId,
        address: &str,
        now_ms: u64,
    ) -> Result<Settled, AuthorityStoreError> {
        let mut transaction = self
            .pool
            .begin()
            .await
            .map_err(|error| unavailable(&error))?;

        let claimed = sqlx::query(
            r"
            UPDATE approval_challenges
            SET status = 'rejected', settled_at_ms = expires_at_ms
            WHERE id = ? AND status = 'pending'
            ",
        )
        .bind(id.as_str())
        .execute(&mut *transaction)
        .await
        .map_err(|error| unavailable(&error))?
        .rows_affected();

        if claimed == 0 {
            transaction
                .rollback()
                .await
                .map_err(|error| unavailable(&error))?;
            return self.settled_state(id).await;
        }

        let spent = sqlx::query(
            r"
            UPDATE inbound_evidence
            SET consumed_at_ms = ?
            WHERE receipt = ? AND consumed_at_ms IS NULL
            ",
        )
        .bind(as_i64(now_ms))
        .bind(receipt.as_str())
        .execute(&mut *transaction)
        .await
        .map_err(|error| unavailable(&error))?
        .rows_affected();

        if spent == 0 {
            transaction
                .rollback()
                .await
                .map_err(|error| unavailable(&error))?;
            return Err(AuthorityStoreError::EvidenceSpent);
        }

        sqlx::query("DELETE FROM awaiting_reply WHERE address = ?")
            .bind(address)
            .execute(&mut *transaction)
            .await
            .map_err(|error| unavailable(&error))?;

        transaction
            .commit()
            .await
            .map_err(|error| unavailable(&error))?;
        Ok(Settled::Now)
    }

    async fn load_delegation(
        &self,
        id: &DelegationId,
    ) -> Result<Option<DelegationRecord>, AuthorityStoreError> {
        let row = sqlx::query(
            r"
            SELECT id, challenge_id, grantor, subject, service, expires_at_ms,
                   revoked_at_ms, envelope, created_at_ms
            FROM delegations
            WHERE id = ?
            ",
        )
        .bind(id.as_str())
        .fetch_optional(&self.pool)
        .await
        .map_err(|error| unavailable(&error))?;

        let Some(row) = row else {
            return Ok(None);
        };
        decode_delegation(&row).map(Some)
    }

    async fn live_binding(
        &self,
        principal: &PrincipalId,
    ) -> Result<Option<BindingRef>, AuthorityStoreError> {
        // The inherent `live_binding_for` already answers this against a row;
        // the port simply exposes it to the verifier, which needs it to check a
        // claimed binding against something other than the claim itself.
        Self::live_binding_for(self, principal)
            .await
            .map(|found| found.map(|binding| binding.reference()))
            .map_err(|error| AuthorityStoreError::Unavailable(error.to_string()))
    }

    async fn revoke_delegation(
        &self,
        id: &DelegationId,
        at_ms: u64,
    ) -> Result<bool, AuthorityStoreError> {
        // `WHERE revoked_at_ms IS NULL` makes this idempotent in one statement:
        // a second REVOKE matches no row and reports `false`, which is not an
        // error for a safety exit (spec §2).
        let affected = sqlx::query(
            r"
            UPDATE delegations
            SET revoked_at_ms = ?
            WHERE id = ? AND revoked_at_ms IS NULL
            ",
        )
        .bind(as_i64(at_ms))
        .bind(id.as_str())
        .execute(&self.pool)
        .await
        .map_err(|error| unavailable(&error))?
        .rows_affected();
        Ok(affected == 1)
    }

    async fn write_inbound_evidence(
        &self,
        receipt: &EvidenceReceiptId,
        evidence: &InboundEvidenceRecord,
        challenge: &ApprovalChallengeId,
        now_ms: u64,
        expires_at_ms: u64,
    ) -> Result<EvidenceReceipt, AuthorityStoreError> {
        // `DO NOTHING` on the inbound-identity index makes a carrier redelivery a
        // no-op rather than a second row — the evidence analogue of idempotent
        // begin. The receipt is bound to its challenge here, at deposit.
        let inserted = sqlx::query(
            r"
            INSERT INTO inbound_evidence
                (receipt, provider, provider_account, provider_message_id,
                 claimed_sender, verified, signature, challenge_id,
                 consumed_at_ms, created_at_ms, expires_at_ms)
            VALUES (?, ?, ?, ?, ?, ?, ?, ?, NULL, ?, ?)
            ON CONFLICT(provider, provider_account, provider_message_id) DO NOTHING
            ",
        )
        .bind(receipt.as_str())
        .bind(&evidence.provider)
        .bind(&evidence.provider_account)
        .bind(&evidence.provider_message_id)
        .bind(&evidence.claimed_sender)
        .bind(i64::from(evidence.verified))
        .bind(evidence.signature.as_deref())
        .bind(challenge.as_str())
        .bind(as_i64(now_ms))
        .bind(as_i64(expires_at_ms))
        .execute(&self.pool)
        .await
        .map_err(|error| unavailable(&error))?
        .rows_affected();

        if inserted == 1 {
            return Ok(EvidenceReceipt {
                receipt: receipt.clone(),
                fresh: true,
            });
        }

        // Redelivery: return the receipt the first deposit minted, not the one
        // offered now.
        let row = sqlx::query(
            r"
            SELECT receipt FROM inbound_evidence
            WHERE provider = ? AND provider_account = ? AND provider_message_id = ?
            ",
        )
        .bind(&evidence.provider)
        .bind(&evidence.provider_account)
        .bind(&evidence.provider_message_id)
        .fetch_one(&self.pool)
        .await
        .map_err(|error| unavailable(&error))?;
        Ok(EvidenceReceipt {
            receipt: EvidenceReceiptId::new(
                row.try_get::<String, _>("receipt")
                    .map_err(|error| row_error(&error))?,
            ),
            fresh: false,
        })
    }

    async fn load_evidence_by_receipt(
        &self,
        receipt: &EvidenceReceiptId,
    ) -> Result<Option<LoadedEvidence>, AuthorityStoreError> {
        let row = sqlx::query(
            r"
            SELECT receipt, claimed_sender, verified, signature, challenge_id,
                   consumed_at_ms
            FROM inbound_evidence
            WHERE receipt = ?
            ",
        )
        .bind(receipt.as_str())
        .fetch_optional(&self.pool)
        .await
        .map_err(|error| unavailable(&error))?;

        let Some(row) = row else {
            return Ok(None);
        };
        Ok(Some(LoadedEvidence {
            receipt: EvidenceReceiptId::new(
                row.try_get::<String, _>("receipt")
                    .map_err(|error| row_error(&error))?,
            ),
            claimed_sender: row
                .try_get::<String, _>("claimed_sender")
                .map_err(|error| row_error(&error))?,
            verified: row
                .try_get::<i64, _>("verified")
                .map_err(|error| row_error(&error))?
                != 0,
            signature: row
                .try_get::<Option<String>, _>("signature")
                .map_err(|error| row_error(&error))?,
            challenge_id: row
                .try_get::<Option<String>, _>("challenge_id")
                .map_err(|error| row_error(&error))?
                .map(ApprovalChallengeId::new),
            consumed_at_ms: row
                .try_get::<Option<i64>, _>("consumed_at_ms")
                .map_err(|error| row_error(&error))?
                .map(from_i64),
        }))
    }

    async fn load_delegation_by_challenge(
        &self,
        challenge: &ApprovalChallengeId,
    ) -> Result<Option<DelegationRecord>, AuthorityStoreError> {
        let row = sqlx::query(
            r"
            SELECT id, challenge_id, grantor, subject, service, expires_at_ms,
                   revoked_at_ms, envelope, created_at_ms
            FROM delegations
            WHERE challenge_id = ?
            ",
        )
        .bind(challenge.as_str())
        .fetch_optional(&self.pool)
        .await
        .map_err(|error| unavailable(&error))?;

        let Some(row) = row else {
            return Ok(None);
        };
        decode_delegation(&row).map(Some)
    }

    async fn live_binding_by_address(
        &self,
        address: &str,
    ) -> Result<Option<BoundChannel>, AuthorityStoreError> {
        // The inherent `live_binding` answers "who is this number?" against a
        // row, carrying the assurance the grant is capped at.
        self.live_binding(address)
            .await
            .map(|found| {
                found.map(|binding| BoundChannel {
                    reference: binding.reference(),
                    assurance: binding.assurance,
                })
            })
            .map_err(|error| AuthorityStoreError::Unavailable(error.to_string()))
    }

    async fn address_for(
        &self,
        principal: &PrincipalId,
    ) -> Result<Option<String>, AuthorityStoreError> {
        self.live_binding_for(principal)
            .await
            .map(|found| found.map(|binding| binding.address))
            .map_err(|error| AuthorityStoreError::Unavailable(error.to_string()))
    }

    async fn await_reply(
        &self,
        address: &str,
        challenge: &ApprovalChallengeId,
        now_ms: u64,
        expires_at_ms: u64,
    ) -> Result<(), AuthorityStoreError> {
        // Address is the key: a second challenge raised to the same number
        // supersedes the first, so one number awaits at most one reply.
        sqlx::query(
            r"
            INSERT INTO awaiting_reply (address, challenge_id, created_at_ms, expires_at_ms)
            VALUES (?, ?, ?, ?)
            ON CONFLICT(address) DO UPDATE SET
                challenge_id = excluded.challenge_id,
                created_at_ms = excluded.created_at_ms,
                expires_at_ms = excluded.expires_at_ms
            ",
        )
        .bind(address)
        .bind(challenge.as_str())
        .bind(as_i64(now_ms))
        .bind(as_i64(expires_at_ms))
        .execute(&self.pool)
        .await
        .map_err(|error| unavailable(&error))?;
        Ok(())
    }

    async fn awaiting_reply(
        &self,
        address: &str,
    ) -> Result<Option<ApprovalChallengeId>, AuthorityStoreError> {
        let row = sqlx::query("SELECT challenge_id FROM awaiting_reply WHERE address = ?")
            .bind(address)
            .fetch_optional(&self.pool)
            .await
            .map_err(|error| unavailable(&error))?;
        let Some(row) = row else {
            return Ok(None);
        };
        Ok(Some(ApprovalChallengeId::new(
            row.try_get::<String, _>("challenge_id")
                .map_err(|error| row_error(&error))?,
        )))
    }

    async fn insert_or_get_challenge(
        &self,
        challenge: &ChallengeRecord,
    ) -> Result<InsertOutcome, AuthorityStoreError> {
        // A fresh id, so the only conflict this can hit is the partial UNIQUE on
        // `booking_intent`: a challenge already exists for this inbound booking.
        let inserted = sqlx::query(
            r"
            INSERT INTO approval_challenges
                (id, code, scope, scope_hash, binding_principal, binding_version,
                 grantor, subject, actor, assurance, status, attempts_used,
                 created_at_ms, expires_at_ms, settled_at_ms, booking_intent)
            VALUES (?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, NULL, ?)
            ON CONFLICT DO NOTHING
            ",
        )
        .bind(challenge.id.as_str())
        .bind(challenge.code.revealed())
        .bind(challenge.scope.encode())
        .bind(challenge.scope_hash.to_string())
        .bind(challenge.binding.principal.as_str())
        .bind(as_i64(challenge.binding.version))
        .bind(challenge.grantor.as_str())
        .bind(challenge.subject.as_str())
        .bind(challenge.actor.as_str())
        .bind(challenge.assurance.name())
        .bind(challenge.status.name())
        .bind(i64::from(challenge.attempts_used))
        .bind(as_i64(challenge.created_at_ms))
        .bind(as_i64(challenge.scope.expires_at_ms))
        .bind(challenge.scope.booking.as_str())
        .execute(&self.pool)
        .await
        .map_err(|error| unavailable(&error))?
        .rows_affected();

        if inserted == 1 {
            return Ok(InsertOutcome::Inserted);
        }

        // The conflict was the booking's existing challenge — return it, so the
        // redelivered BOOK reuses that id and code rather than raising a second.
        let row = sqlx::query(
            r"
            SELECT id, code, scope, scope_hash, binding_principal, binding_version,
                   grantor, subject, actor, assurance, status, attempts_used,
                   created_at_ms
            FROM approval_challenges
            WHERE booking_intent = ?
            ",
        )
        .bind(challenge.scope.booking.as_str())
        .fetch_optional(&self.pool)
        .await
        .map_err(|error| unavailable(&error))?;

        match row {
            Some(row) => Ok(InsertOutcome::Existing(Box::new(
                Self::decode_challenge(&row)
                    .map_err(|error| AuthorityStoreError::Unavailable(error.to_string()))?,
            ))),
            // No row for this booking, yet the insert conflicted: an id
            // collision, not a redelivery. Refuse rather than silently reuse an
            // unrelated challenge.
            None => Err(AuthorityStoreError::ChallengeExists),
        }
    }
}

/// Decode one delegation row into the record.
///
/// Every column is read with `try_get?` rather than a defaulting `unwrap_or`:
/// the M5.1 review found a shim whose `.unwrap_or(0)` made an assertion on a
/// nonexistent column vacuous, and a delegation that silently reads as
/// never-revoked is that defect with teeth. Shared by the by-id and by-challenge
/// lookups so neither can decode a row differently from the other.
fn decode_delegation(
    row: &sqlx::sqlite::SqliteRow,
) -> Result<DelegationRecord, AuthorityStoreError> {
    Ok(DelegationRecord {
        id: DelegationId::new(
            row.try_get::<String, _>("id")
                .map_err(|error| row_error(&error))?,
        ),
        challenge_id: ApprovalChallengeId::new(
            row.try_get::<String, _>("challenge_id")
                .map_err(|error| row_error(&error))?,
        ),
        grantor: PrincipalId::new(
            row.try_get::<String, _>("grantor")
                .map_err(|error| row_error(&error))?,
        ),
        subject: PrincipalId::new(
            row.try_get::<String, _>("subject")
                .map_err(|error| row_error(&error))?,
        ),
        service: ServiceId::new(
            row.try_get::<String, _>("service")
                .map_err(|error| row_error(&error))?,
        ),
        issued_at_ms: from_i64(
            row.try_get::<i64, _>("created_at_ms")
                .map_err(|error| row_error(&error))?,
        ),
        expires_at_ms: from_i64(
            row.try_get::<i64, _>("expires_at_ms")
                .map_err(|error| row_error(&error))?,
        ),
        revoked_at_ms: row
            .try_get::<Option<i64>, _>("revoked_at_ms")
            .map_err(|error| row_error(&error))?
            .map(from_i64),
        envelope: row
            .try_get::<Vec<u8>, _>("envelope")
            .map_err(|error| row_error(&error))?,
    })
}

fn as_i64(value: u64) -> i64 {
    i64::try_from(value).unwrap_or(i64::MAX)
}

fn from_i64(value: i64) -> u64 {
    u64::try_from(value).unwrap_or(0)
}

fn unavailable(error: &sqlx::Error) -> AuthorityStoreError {
    AuthorityStoreError::Unavailable(error.to_string())
}

fn row_error(error: &sqlx::Error) -> AuthorityStoreError {
    AuthorityStoreError::Unavailable(error.to_string())
}

/// Whether SQLite refused this because a unique index said no.
///
/// Matched on the driver's own codes (2067 for a unique index, 1555 for a
/// primary key) rather than on message text, which changes between versions.
fn is_unique_violation(error: &sqlx::Error) -> bool {
    error
        .as_database_error()
        .and_then(sqlx::error::DatabaseError::code)
        .is_some_and(|code| code == "2067" || code == "1555")
}
