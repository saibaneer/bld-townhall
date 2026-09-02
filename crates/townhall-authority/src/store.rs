//! What the authority component needs persisted, as ports someone else answers.
//!
//! The SQL implementation lives in `townhall-store`, which owns `sqlx`; this
//! crate must not name a connection pool (ADR-025). The in-memory
//! implementation here is for this crate's own tests and for the testkit
//! issuer — it is not a fallback, and nothing in a composition root may reach
//! for it.
//!
//! # Why one atomic method rather than "consume, then insert"
//!
//! One challenge must yield at most one grant (spec §17). Written as two calls,
//! the window between them is a second grant: two concurrent correct replies
//! both see `Pending`, both consume, both insert. [`ApprovalStore::settle_with_grant`]
//! is therefore a single operation, and its contract is that an implementation
//! performs it atomically — the in-memory one under a single lock, the SQL one
//! in a transaction.

use crate::challenge::{ChallengeRecord, ChallengeStatus};
use bld_types::{ApprovalChallengeId, DelegationId, PrincipalId, ServiceId};
use std::collections::HashMap;
use std::sync::Mutex;

/// A delegation as it is persisted.
///
/// # Why the envelope is opaque bytes
///
/// ADR-025: the row representation is owned by the ISSUER, not by the store's
/// decoder. The store persists these bytes and the handful of columns
/// revocation and expiry must index; it never interprets the envelope, so it
/// cannot grow its own opinion about what a grant permits.
///
/// The alternative — a fully-typed row the store decodes — is the second
/// vocabulary ADR-025 named: a mirror of the envelope, free to drift, and the
/// no-serde assertion on `VerifiedAuthority` would keep passing while the
/// mirror became the real minting path.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct DelegationRecord {
    pub id: DelegationId,
    /// Indexed: "everything granted over Lucy's bookings".
    pub grantor: PrincipalId,
    /// Indexed: "everything Marco holds".
    pub subject: PrincipalId,
    pub service: ServiceId,
    /// Indexed: expiry sweeps, and the resolver's liveness check.
    pub expires_at_ms: u64,
    /// `Some` once revoked. Never un-set.
    pub revoked_at_ms: Option<u64>,
    /// The issuer's own encoding of the envelope.
    pub envelope: Vec<u8>,
}

/// Whether an operation was the one that settled a challenge.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Settled {
    /// This call settled it.
    Now,
    /// Someone else already had — a replay, or a race that lost.
    Already(ChallengeStatus),
}

#[derive(Debug, thiserror::Error)]
pub enum StoreError {
    #[error("no such challenge")]
    UnknownChallenge,
    #[error("a challenge with that identifier already exists")]
    ChallengeExists,
    #[error("a delegation with that identifier already exists")]
    DelegationExists,
    #[error("the authority store could not be reached: {0}")]
    Unavailable(String),
}

/// Everything the verifier and issuer need to persist.
pub trait ApprovalStore: Send + Sync {
    /// Record a new challenge. Refuses a duplicate id rather than overwriting —
    /// an overwrite would reset the attempt count, which is the bound.
    ///
    /// # Errors
    /// The id already exists, or the store is unreachable.
    fn insert_challenge(&self, challenge: &ChallengeRecord) -> Result<(), StoreError>;

    /// # Errors
    /// The store is unreachable. A missing challenge is `Ok(None)`, not an
    /// error — "no such challenge" is an answer.
    fn load_challenge(
        &self,
        id: &ApprovalChallengeId,
    ) -> Result<Option<ChallengeRecord>, StoreError>;

    /// Count one wrong answer, returning attempts left and the resulting status.
    ///
    /// Atomic, and it is the ONLY way the count moves: a verifier that read,
    /// decided and wrote would let concurrent wrong guesses share an attempt.
    ///
    /// # Errors
    /// No such challenge, or the store is unreachable.
    fn record_failed_attempt(
        &self,
        id: &ApprovalChallengeId,
        now_ms: u64,
    ) -> Result<(u8, ChallengeStatus), StoreError>;

    /// Consume the challenge and insert its one grant, atomically.
    ///
    /// Returns [`Settled::Already`] without inserting if the challenge had been
    /// settled — which is how a replayed approval is refused a second grant.
    ///
    /// # Errors
    /// No such challenge, a delegation id collision, or an unreachable store.
    fn settle_with_grant(
        &self,
        id: &ApprovalChallengeId,
        grant: &DelegationRecord,
    ) -> Result<Settled, StoreError>;

    /// Answer `NO`: terminal, and no grant.
    ///
    /// # Errors
    /// No such challenge, or the store is unreachable.
    fn settle_rejected(&self, id: &ApprovalChallengeId) -> Result<Settled, StoreError>;

    /// # Errors
    /// The store is unreachable. A missing delegation is `Ok(None)`.
    fn load_delegation(&self, id: &DelegationId) -> Result<Option<DelegationRecord>, StoreError>;

    /// Revoke, returning whether this call was the one that did it.
    ///
    /// Idempotent by contract: REVOKE is a safety exit (spec §2) and a second
    /// one must not be an error.
    ///
    /// # Errors
    /// The store is unreachable. An unknown or already-revoked delegation is
    /// `Ok(false)`.
    fn revoke_delegation(&self, id: &DelegationId, at_ms: u64) -> Result<bool, StoreError>;
}

/// The in-memory store: this crate's tests, and the testkit issuer.
///
/// One `Mutex` over the whole thing, deliberately. Two locks would make
/// [`ApprovalStore::settle_with_grant`]'s atomicity a matter of lock ordering,
/// and the operation it guards is the one that must not happen twice.
#[derive(Debug, Default)]
pub struct MemoryApprovalStore {
    held: Mutex<Held>,
}

#[derive(Debug, Default)]
struct Held {
    challenges: HashMap<String, ChallengeRecord>,
    delegations: HashMap<String, DelegationRecord>,
}

impl MemoryApprovalStore {
    #[must_use]
    pub fn new() -> Self {
        Self::default()
    }

    fn locked(&self) -> std::sync::MutexGuard<'_, Held> {
        self.held
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
    }
}

impl ApprovalStore for MemoryApprovalStore {
    fn insert_challenge(&self, challenge: &ChallengeRecord) -> Result<(), StoreError> {
        let mut held = self.locked();
        let key = challenge.id.as_str().to_owned();
        if held.challenges.contains_key(&key) {
            return Err(StoreError::ChallengeExists);
        }
        held.challenges.insert(key, challenge.clone());
        Ok(())
    }

    fn load_challenge(
        &self,
        id: &ApprovalChallengeId,
    ) -> Result<Option<ChallengeRecord>, StoreError> {
        Ok(self.locked().challenges.get(id.as_str()).cloned())
    }

    fn record_failed_attempt(
        &self,
        id: &ApprovalChallengeId,
        _now_ms: u64,
    ) -> Result<(u8, ChallengeStatus), StoreError> {
        let mut held = self.locked();
        let challenge = held
            .challenges
            .get_mut(id.as_str())
            .ok_or(StoreError::UnknownChallenge)?;
        if challenge.status.is_settled() {
            return Ok((challenge.attempts_left(), challenge.status));
        }
        challenge.attempts_used = challenge.attempts_used.saturating_add(1);
        if challenge.attempts_left() == 0 {
            challenge.status = ChallengeStatus::Exhausted;
        }
        Ok((challenge.attempts_left(), challenge.status))
    }

    fn settle_with_grant(
        &self,
        id: &ApprovalChallengeId,
        grant: &DelegationRecord,
    ) -> Result<Settled, StoreError> {
        // One lock for the whole operation — the borrow is taken twice only
        // because the status check and the insertion touch different maps, and
        // nothing else can interleave between them.
        let mut held = self.locked();
        let status = held
            .challenges
            .get(id.as_str())
            .ok_or(StoreError::UnknownChallenge)?
            .status;
        if status.is_settled() {
            return Ok(Settled::Already(status));
        }
        let delegation_key = grant.id.as_str().to_owned();
        if held.delegations.contains_key(&delegation_key) {
            return Err(StoreError::DelegationExists);
        }
        held.challenges
            .get_mut(id.as_str())
            .ok_or(StoreError::UnknownChallenge)?
            .status = ChallengeStatus::Approved;
        held.delegations.insert(delegation_key, grant.clone());
        Ok(Settled::Now)
    }

    fn settle_rejected(&self, id: &ApprovalChallengeId) -> Result<Settled, StoreError> {
        let mut held = self.locked();
        let challenge = held
            .challenges
            .get_mut(id.as_str())
            .ok_or(StoreError::UnknownChallenge)?;
        if challenge.status.is_settled() {
            return Ok(Settled::Already(challenge.status));
        }
        challenge.status = ChallengeStatus::Rejected;
        Ok(Settled::Now)
    }

    fn load_delegation(&self, id: &DelegationId) -> Result<Option<DelegationRecord>, StoreError> {
        Ok(self.locked().delegations.get(id.as_str()).cloned())
    }

    fn revoke_delegation(&self, id: &DelegationId, at_ms: u64) -> Result<bool, StoreError> {
        let mut held = self.locked();
        let Some(delegation) = held.delegations.get_mut(id.as_str()) else {
            return Ok(false);
        };
        if delegation.revoked_at_ms.is_some() {
            return Ok(false);
        }
        delegation.revoked_at_ms = Some(at_ms);
        Ok(true)
    }
}
