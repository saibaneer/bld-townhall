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

use crate::assurance::AssuranceLevel;
use crate::challenge::{ChallengeRecord, ChallengeStatus};
use crate::grant::BindingRef;
use async_trait::async_trait;
use bld_types::{ApprovalChallengeId, DelegationId, EvidenceReceiptId, PrincipalId, ServiceId};
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
    /// The challenge this grant came from. Carried so a replayed `YES` can
    /// recover the reference by challenge rather than be refused — the
    /// `delegations.challenge_id` UNIQUE column, surfaced onto the record.
    pub challenge_id: ApprovalChallengeId,
    /// Indexed: "everything granted over Lucy's bookings".
    pub grantor: PrincipalId,
    /// Indexed: "everything Marco holds".
    pub subject: PrincipalId,
    pub service: ServiceId,
    /// When the grant was issued — the row's own `created_at_ms`.
    ///
    /// Carried rather than read out of the envelope, because the store must be
    /// able to write the column without interpreting those bytes.
    pub issued_at_ms: u64,
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

/// One inbound message's transport evidence, to be deposited under a receipt.
///
/// Primitives only: this crate must not name `TransportEvidence`, which lives in
/// `townhall-channel` above it. The trusted ingress fills these from the carrier;
/// the identity triple (provider/account/message-id) is transport-set, never a
/// caller-chosen field.
///
/// No `Debug`: `claimed_sender` and `signature` must not reach a log (§15.1).
#[derive(Clone)]
pub struct InboundEvidenceRecord {
    pub provider: String,
    pub provider_account: String,
    pub provider_message_id: String,
    pub claimed_sender: String,
    pub verified: bool,
    pub signature: Option<String>,
}

/// The outcome of a deposit: the receipt, and whether this call created the row.
///
/// `fresh == false` means a carrier redelivery mapped to the row a prior deposit
/// wrote — the evidence analogue of idempotent begin. The receipt returned is
/// then the EXISTING one, not the id the caller offered.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct EvidenceReceipt {
    pub receipt: EvidenceReceiptId,
    pub fresh: bool,
}

/// A deposited evidence row, read back by the verifier.
///
/// Only what `submit` needs to check: the sender it resolves to a live binding,
/// the challenge it was bound to at deposit, and whether it has been spent. No
/// `Debug` — `claimed_sender`/`signature` stay out of logs (§15.1).
#[derive(Clone)]
pub struct LoadedEvidence {
    pub receipt: EvidenceReceiptId,
    pub claimed_sender: String,
    pub verified: bool,
    pub signature: Option<String>,
    /// The challenge this evidence was bound to at deposit. `None` only for a row
    /// deposited outside a correlated challenge, which `submit` refuses.
    pub challenge_id: Option<ApprovalChallengeId>,
    /// `Some` once spent. A second answer to an already-approved challenge is the
    /// idempotent-replay path, not a second grant.
    pub consumed_at_ms: Option<u64>,
}

/// A resolved live channel binding: which principal at which revision, and how
/// well the channel is known.
///
/// The verifier resolves an inbound reply's number to one of these rather than
/// trusting a claimed identity, and it needs the assurance so the grant can be
/// CAPPED at the level the channel actually establishes (ADR-021): a binding
/// cannot lift a grant above the assurance it provides.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct BoundChannel {
    pub reference: BindingRef,
    pub assurance: AssuranceLevel,
}

/// Whether an idempotent insert created a challenge or found an existing one.
#[derive(Clone, Debug)]
pub enum InsertOutcome {
    /// This call wrote the row.
    Inserted,
    /// A challenge for this inbound intent already existed — the redelivered
    /// BOOK reuses it rather than raising a second challenge with a fresh code.
    /// Boxed because a `ChallengeRecord` dwarfs the `Inserted` variant.
    Existing(Box<ChallengeRecord>),
}

#[derive(Debug, thiserror::Error)]
pub enum StoreError {
    #[error("no such challenge")]
    UnknownChallenge,
    #[error("a challenge with that identifier already exists")]
    ChallengeExists,
    #[error("a delegation with that identifier already exists")]
    DelegationExists,
    /// A settlement named a receipt that the store does not hold. Not a normal
    /// path — `submit` loads the row first — so it signals an invariant break,
    /// not a user error.
    #[error("no such receipt")]
    UnknownReceipt,
    /// A settlement tried to spend a receipt that was already spent. The one-use
    /// guard in `settle_with_grant`; a contradiction reaching here rather than an
    /// expected denial.
    #[error("that receipt was already spent")]
    EvidenceSpent,
    #[error("the authority store could not be reached: {0}")]
    Unavailable(String),
}

/// Everything the verifier and issuer need to persist.
#[async_trait]
pub trait ApprovalStore: Send + Sync {
    /// Record a new challenge. Refuses a duplicate id rather than overwriting —
    /// an overwrite would reset the attempt count, which is the bound.
    ///
    /// # Errors
    /// The id already exists, or the store is unreachable.
    async fn insert_challenge(&self, challenge: &ChallengeRecord) -> Result<(), StoreError>;

    /// # Errors
    /// The store is unreachable. A missing challenge is `Ok(None)`, not an
    /// error — "no such challenge" is an answer.
    async fn load_challenge(
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
    async fn record_failed_attempt(
        &self,
        id: &ApprovalChallengeId,
        now_ms: u64,
    ) -> Result<(u8, ChallengeStatus), StoreError>;

    /// Consume the challenge, insert its one grant, spend the receipt, and clear
    /// the awaiting-reply correlation — all in ONE transaction.
    ///
    /// Returns [`Settled::Already`] without inserting if the challenge had been
    /// settled — which is how a replayed approval is refused a second grant.
    ///
    /// # Why the receipt and address fold in here
    ///
    /// Marking the receipt spent and clearing the correlation must commit with
    /// the same transaction that claims the challenge, or a separate round-trip
    /// reopens the race the receipt exists to close. The receipt is spent under a
    /// `consumed_at_ms IS NULL` guard for the same reason the challenge carries
    /// `WHERE status = 'pending'`: one-use, enforced by the write.
    ///
    /// # Errors
    /// No such challenge, a delegation id collision, or an unreachable store.
    async fn settle_with_grant(
        &self,
        id: &ApprovalChallengeId,
        grant: &DelegationRecord,
        receipt: &EvidenceReceiptId,
        address: &str,
        now_ms: u64,
    ) -> Result<Settled, StoreError>;

    /// Answer `NO`: terminal, and no grant. Spends the receipt and clears the
    /// awaiting-reply correlation, as [`Self::settle_with_grant`] does — a
    /// decline stops the number awaiting a reply.
    ///
    /// # Errors
    /// No such challenge, or the store is unreachable.
    async fn settle_rejected(
        &self,
        id: &ApprovalChallengeId,
        receipt: &EvidenceReceiptId,
        address: &str,
        now_ms: u64,
    ) -> Result<Settled, StoreError>;

    /// # Errors
    /// The store is unreachable. A missing delegation is `Ok(None)`.
    async fn load_delegation(
        &self,
        id: &DelegationId,
    ) -> Result<Option<DelegationRecord>, StoreError>;

    /// The live binding for a principal, if their channel is bound.
    ///
    /// # Why the VERIFIER needs this and not just the read gate
    ///
    /// Because without it, "the reply came from the channel the challenge was
    /// sent to" was checked by comparing two values the CALLER supplied: the
    /// binding it named when raising the challenge, and the binding it named
    /// when answering. A caller that sent the same pair twice passed. Review
    /// found this after M7B was written, and it is the difference between a
    /// check and a formality.
    ///
    /// Now the verifier compares against a row. That still does not prove a
    /// person answered — proving that needs evidence from the channel itself,
    /// which arrives with M7C — but it does mean the binding named must exist,
    /// be active, and be at the revision it claims.
    ///
    /// # Errors
    /// The store is unreachable. An unbound principal is `Ok(None)`.
    async fn live_binding(
        &self,
        principal: &PrincipalId,
    ) -> Result<Option<crate::grant::BindingRef>, StoreError>;

    /// Revoke, returning whether this call was the one that did it.
    ///
    /// Idempotent by contract: REVOKE is a safety exit (spec §2) and a second
    /// one must not be an error.
    ///
    /// # Errors
    /// The store is unreachable. An unknown or already-revoked delegation is
    /// `Ok(false)`.
    async fn revoke_delegation(&self, id: &DelegationId, at_ms: u64) -> Result<bool, StoreError>;

    /// Spend a control receipt one-use AND revoke every live delegation the
    /// receipt's principal granted — both in ONE transaction. Returns the count
    /// revoked.
    ///
    /// # Why the spend and the sweep must commit together
    ///
    /// Split into two calls, a crash between them either strands the receipt
    /// (spent, nothing revoked) or double-spends it (revoked twice on a retry).
    /// Folding the spend in also makes idempotency fall out for free: a receipt
    /// already `consumed_at_ms` spends nothing, the sweep is skipped, and the
    /// call returns `Ok(0)` — a re-sent REVOKE is a safety exit (spec §2), never
    /// an error.
    ///
    /// # Why key on `grantor` and not the challenge→binding join
    ///
    /// `grantor` is who AUTHORIZED the grants — definitionally "everything this
    /// person set in motion". It is a first-class indexed column, so the sweep is
    /// one statement. The verifier resolves the inbound sender to a binding and
    /// passes that binding's principal as `grantor`; for M7C's own-behalf
    /// bookings grantor, subject and channel-owner coincide (see the dispatcher).
    ///
    /// # Errors
    /// The store is unreachable.
    async fn revoke_all_by_grantor_with_receipt(
        &self,
        grantor: &PrincipalId,
        receipt: &EvidenceReceiptId,
        at_ms: u64,
    ) -> Result<u64, StoreError>;

    /// Deposit one inbound message's transport evidence under `receipt`, BOUND to
    /// the challenge it answers, and return the receipt actually stored.
    ///
    /// Idempotent on the inbound identity triple: a carrier redelivery returns
    /// the existing row's receipt with `fresh == false` rather than writing a
    /// second row. The caller mints `receipt` (it holds the entropy); on a
    /// redelivery the offered id is discarded.
    ///
    /// # Errors
    /// The store is unreachable.
    async fn write_inbound_evidence(
        &self,
        receipt: &EvidenceReceiptId,
        evidence: &InboundEvidenceRecord,
        challenge: &ApprovalChallengeId,
        now_ms: u64,
        expires_at_ms: u64,
    ) -> Result<EvidenceReceipt, StoreError>;

    /// Deposit a CONTROL inbound's transport evidence under `receipt`, bound to
    /// NO challenge (`challenge_id` NULL) — a command that answers no approval,
    /// such as a REVOKE.
    ///
    /// One-use and short-TTL, and idempotent on the inbound identity triple
    /// exactly as [`Self::write_inbound_evidence`]. A separate method rather than
    /// an `Option<challenge>` parameter so the correlated path keeps its
    /// bind-at-deposit invariant type-enforced: only a control inbound may land a
    /// NULL-challenge row, and it must come through here to do it.
    ///
    /// # Errors
    /// The store is unreachable.
    async fn write_control_evidence(
        &self,
        receipt: &EvidenceReceiptId,
        evidence: &InboundEvidenceRecord,
        now_ms: u64,
        expires_at_ms: u64,
    ) -> Result<EvidenceReceipt, StoreError>;

    /// Read a deposited evidence row back by its receipt.
    ///
    /// # Errors
    /// The store is unreachable. A receipt naming no row is `Ok(None)`.
    async fn load_evidence_by_receipt(
        &self,
        receipt: &EvidenceReceiptId,
    ) -> Result<Option<LoadedEvidence>, StoreError>;

    /// The one delegation a challenge produced, if it produced one.
    ///
    /// Uses the `delegations.challenge_id` UNIQUE index, so it is the recovery
    /// path a replayed `YES` takes: return the reference already issued rather
    /// than refuse it as a replay.
    ///
    /// # Errors
    /// The store is unreachable. A challenge with no delegation is `Ok(None)`.
    async fn load_delegation_by_challenge(
        &self,
        challenge: &ApprovalChallengeId,
    ) -> Result<Option<DelegationRecord>, StoreError>;

    /// The live binding for an ADDRESS, if that number is bound.
    ///
    /// This is the reverse of [`Self::live_binding`]: an inbound reply names a
    /// number, and the verifier resolves it to a binding row rather than
    /// trusting a claimed identity — the store-mediated move at the heart of
    /// ADR-026.
    ///
    /// # Errors
    /// The store is unreachable. An unbound address is `Ok(None)`.
    async fn live_binding_by_address(
        &self,
        address: &str,
    ) -> Result<Option<BoundChannel>, StoreError>;

    /// The address a principal's live channel binds to, if any.
    ///
    /// Needed when a challenge is raised: the correlation record must be keyed by
    /// the number the reply will come FROM, which is this principal's bound
    /// address.
    ///
    /// # Errors
    /// The store is unreachable. An unbound principal is `Ok(None)`.
    async fn address_for(&self, principal: &PrincipalId) -> Result<Option<String>, StoreError>;

    /// Record that `address` is now awaiting a reply to `challenge`.
    ///
    /// `address` is the key, so a second challenge raised to the same number
    /// supersedes the first — one number awaits at most one challenge. This is
    /// what lets a bare `YES 7312` route by the number it came from rather than
    /// by its (non-unique) code.
    ///
    /// # Errors
    /// The store is unreachable.
    async fn await_reply(
        &self,
        address: &str,
        challenge: &ApprovalChallengeId,
        now_ms: u64,
        expires_at_ms: u64,
    ) -> Result<(), StoreError>;

    /// Which challenge, if any, `address` is awaiting a reply to.
    ///
    /// # Errors
    /// The store is unreachable. A number awaiting nothing is `Ok(None)`.
    async fn awaiting_reply(
        &self,
        address: &str,
    ) -> Result<Option<ApprovalChallengeId>, StoreError>;

    /// Insert a challenge, or return the existing one for the same inbound intent.
    ///
    /// Idempotent begin: a redelivered `BOOK` for a booking that already has a
    /// challenge — at ANY lifecycle stage — gets that challenge back rather than
    /// a fresh one with a new code.
    ///
    /// # Errors
    /// The store is unreachable.
    async fn insert_or_get_challenge(
        &self,
        challenge: &ChallengeRecord,
    ) -> Result<InsertOutcome, StoreError>;
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
    /// Principal -> live binding revision.
    bindings: HashMap<String, u64>,
    /// Principal -> the address its live channel binds to.
    addresses: HashMap<String, String>,
    /// Address -> (principal, revision, assurance): the reverse the verifier
    /// resolves an inbound reply against.
    by_address: HashMap<String, (String, u64, AssuranceLevel)>,
    /// Receipt -> the deposited evidence row.
    evidence: HashMap<String, StoredEvidence>,
    /// Inbound identity triple -> receipt, for redelivery idempotency.
    evidence_by_identity: HashMap<(String, String, String), String>,
    /// Address -> the challenge it awaits a reply to.
    awaiting: HashMap<String, String>,
    /// Inbound booking intent -> challenge id, for idempotent begin.
    booking_intent: HashMap<String, String>,
}

/// The in-memory shape of an `inbound_evidence` row.
#[derive(Clone)]
struct StoredEvidence {
    receipt: String,
    claimed_sender: String,
    verified: bool,
    signature: Option<String>,
    challenge_id: Option<String>,
    consumed_at_ms: Option<u64>,
}

// A manual, redacting `Debug`: `claimed_sender` and `signature` must never reach
// a log (§15.1), but `Held` above derives `Debug` for the store's own.
impl std::fmt::Debug for StoredEvidence {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("StoredEvidence")
            .field("receipt", &self.receipt)
            .field("verified", &self.verified)
            .field("challenge_id", &self.challenge_id)
            .field("consumed_at_ms", &self.consumed_at_ms)
            .finish_non_exhaustive()
    }
}

impl MemoryApprovalStore {
    #[must_use]
    pub fn new() -> Self {
        Self::default()
    }

    /// Bind a principal's channel at `version`, deriving a stand-in address.
    ///
    /// The in-memory equivalent of a `channel_bindings` row. Tests that answer
    /// a challenge need one, because the verifier now checks against it rather
    /// than against the caller's own claim. Use [`Self::bind_address`] when the
    /// test needs to control the number the reply comes from.
    pub fn bind(&self, principal: &PrincipalId, version: u64) {
        let address = format!("+{}", principal.as_str());
        self.bind_address(principal, &address, version);
    }

    /// Bind a principal's channel to a specific address at `version`, at
    /// [`AssuranceLevel::SmsReply`] — the level a real SMS binding establishes.
    ///
    /// Populates both directions: principal -> (version, address), which the read
    /// gate needs, and address -> (principal, version, assurance), which the
    /// verifier resolves an inbound reply against.
    pub fn bind_address(&self, principal: &PrincipalId, address: &str, version: u64) {
        self.bind_address_at(principal, address, version, AssuranceLevel::SmsReply);
    }

    /// As [`Self::bind_address`], but at a stated assurance — for tests that
    /// exercise the grant's assurance cap.
    pub fn bind_address_at(
        &self,
        principal: &PrincipalId,
        address: &str,
        version: u64,
        assurance: AssuranceLevel,
    ) {
        let mut held = self.locked();
        held.bindings.insert(principal.as_str().to_owned(), version);
        held.addresses
            .insert(principal.as_str().to_owned(), address.to_owned());
        held.by_address.insert(
            address.to_owned(),
            (principal.as_str().to_owned(), version, assurance),
        );
    }

    /// Withdraw it, so a test can watch a challenge become unanswerable.
    pub fn unbind(&self, principal: &PrincipalId) {
        let mut held = self.locked();
        held.bindings.remove(principal.as_str());
        if let Some(address) = held.addresses.remove(principal.as_str()) {
            held.by_address.remove(&address);
        }
    }

    fn locked(&self) -> std::sync::MutexGuard<'_, Held> {
        self.held
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
    }
}

#[async_trait]
impl ApprovalStore for MemoryApprovalStore {
    async fn insert_challenge(&self, challenge: &ChallengeRecord) -> Result<(), StoreError> {
        let mut held = self.locked();
        let key = challenge.id.as_str().to_owned();
        if held.challenges.contains_key(&key) {
            return Err(StoreError::ChallengeExists);
        }
        held.challenges.insert(key, challenge.clone());
        Ok(())
    }

    async fn load_challenge(
        &self,
        id: &ApprovalChallengeId,
    ) -> Result<Option<ChallengeRecord>, StoreError> {
        Ok(self.locked().challenges.get(id.as_str()).cloned())
    }

    async fn record_failed_attempt(
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

    async fn settle_with_grant(
        &self,
        id: &ApprovalChallengeId,
        grant: &DelegationRecord,
        receipt: &EvidenceReceiptId,
        address: &str,
        now_ms: u64,
    ) -> Result<Settled, StoreError> {
        // One lock for the whole operation — the borrow is taken more than once
        // only because the checks and the writes touch different maps, and
        // nothing else can interleave between them. This is the single-lock
        // stand-in for the SQL transaction: challenge claim, grant insert,
        // receipt consume, and awaiting-reply clear all commit together or not
        // at all.
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
        // Spend the receipt under a one-use guard. Reaching here with the
        // challenge still pending means this is the settling call, so the row is
        // unconsumed — a spent one is a contradiction, refused rather than
        // silently regranted.
        match held.evidence.get_mut(receipt.as_str()) {
            Some(evidence) if evidence.consumed_at_ms.is_none() => {
                evidence.consumed_at_ms = Some(now_ms);
            }
            Some(_) => return Err(StoreError::EvidenceSpent),
            None => return Err(StoreError::UnknownReceipt),
        }
        held.challenges
            .get_mut(id.as_str())
            .ok_or(StoreError::UnknownChallenge)?
            .status = ChallengeStatus::Approved;
        held.delegations.insert(delegation_key, grant.clone());
        held.awaiting.remove(address);
        Ok(Settled::Now)
    }

    async fn settle_rejected(
        &self,
        id: &ApprovalChallengeId,
        receipt: &EvidenceReceiptId,
        address: &str,
        now_ms: u64,
    ) -> Result<Settled, StoreError> {
        let mut held = self.locked();
        let status = held
            .challenges
            .get(id.as_str())
            .ok_or(StoreError::UnknownChallenge)?
            .status;
        if status.is_settled() {
            return Ok(Settled::Already(status));
        }
        match held.evidence.get_mut(receipt.as_str()) {
            Some(evidence) if evidence.consumed_at_ms.is_none() => {
                evidence.consumed_at_ms = Some(now_ms);
            }
            Some(_) => return Err(StoreError::EvidenceSpent),
            None => return Err(StoreError::UnknownReceipt),
        }
        held.challenges
            .get_mut(id.as_str())
            .ok_or(StoreError::UnknownChallenge)?
            .status = ChallengeStatus::Rejected;
        held.awaiting.remove(address);
        Ok(Settled::Now)
    }

    async fn load_delegation(
        &self,
        id: &DelegationId,
    ) -> Result<Option<DelegationRecord>, StoreError> {
        Ok(self.locked().delegations.get(id.as_str()).cloned())
    }

    async fn live_binding(
        &self,
        principal: &PrincipalId,
    ) -> Result<Option<crate::grant::BindingRef>, StoreError> {
        Ok(self
            .locked()
            .bindings
            .get(principal.as_str())
            .map(|version| crate::grant::BindingRef {
                principal: principal.clone(),
                version: *version,
            }))
    }

    async fn revoke_delegation(&self, id: &DelegationId, at_ms: u64) -> Result<bool, StoreError> {
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

    async fn revoke_all_by_grantor_with_receipt(
        &self,
        grantor: &PrincipalId,
        receipt: &EvidenceReceiptId,
        at_ms: u64,
    ) -> Result<u64, StoreError> {
        // One lock for spend-then-sweep, the single-lock stand-in for the SQL
        // transaction. The spend is the idempotency guard: a receipt already
        // consumed revokes nothing and returns Ok(0), so a replayed REVOKE is a
        // no-op rather than a double-sweep.
        let mut held = self.locked();
        match held.evidence.get_mut(receipt.as_str()) {
            // A fresh CONTROL receipt (challenge_id NULL): spend it, then sweep
            // below. The `challenge_id.is_none()` predicate is the anti-lever
            // guard made structural — mirroring the SQL spend's `challenge_id IS
            // NULL` — so a challenge-bound `YES` receipt can never be consumed
            // here to drive a sweep, whatever the caller checked first.
            Some(row) if row.consumed_at_ms.is_none() && row.challenge_id.is_none() => {
                row.consumed_at_ms = Some(at_ms);
            }
            // Spent already (a replay), challenge-bound, or naming no row: revoke
            // nothing. Ok(0), never an error — a re-sent REVOKE is a safety exit
            // (spec §2).
            _ => return Ok(0),
        }
        // Sweep by grantor — a direct field on the record, no join. Every live
        // grant this principal authorized, not just one: the count is the full
        // number stopped, which is what the caller reports back.
        let mut revoked = 0u64;
        for delegation in held.delegations.values_mut() {
            if delegation.grantor.as_str() == grantor.as_str() && delegation.revoked_at_ms.is_none()
            {
                delegation.revoked_at_ms = Some(at_ms);
                revoked += 1;
            }
        }
        Ok(revoked)
    }

    async fn write_inbound_evidence(
        &self,
        receipt: &EvidenceReceiptId,
        evidence: &InboundEvidenceRecord,
        challenge: &ApprovalChallengeId,
        _now_ms: u64,
        _expires_at_ms: u64,
    ) -> Result<EvidenceReceipt, StoreError> {
        let mut held = self.locked();
        let identity = (
            evidence.provider.clone(),
            evidence.provider_account.clone(),
            evidence.provider_message_id.clone(),
        );
        // Redelivery idempotency: the same inbound identity returns the existing
        // receipt rather than a second row.
        if let Some(existing) = held.evidence_by_identity.get(&identity) {
            return Ok(EvidenceReceipt {
                receipt: EvidenceReceiptId::new(existing.clone()),
                fresh: false,
            });
        }
        let row = StoredEvidence {
            receipt: receipt.as_str().to_owned(),
            claimed_sender: evidence.claimed_sender.clone(),
            verified: evidence.verified,
            signature: evidence.signature.clone(),
            challenge_id: Some(challenge.as_str().to_owned()),
            consumed_at_ms: None,
        };
        held.evidence.insert(receipt.as_str().to_owned(), row);
        held.evidence_by_identity
            .insert(identity, receipt.as_str().to_owned());
        Ok(EvidenceReceipt {
            receipt: receipt.clone(),
            fresh: true,
        })
    }

    async fn write_control_evidence(
        &self,
        receipt: &EvidenceReceiptId,
        evidence: &InboundEvidenceRecord,
        _now_ms: u64,
        _expires_at_ms: u64,
    ) -> Result<EvidenceReceipt, StoreError> {
        // As `write_inbound_evidence`, but the row is bound to NO challenge —
        // `challenge_id = None`, the shape a control command (REVOKE) leaves.
        let mut held = self.locked();
        let identity = (
            evidence.provider.clone(),
            evidence.provider_account.clone(),
            evidence.provider_message_id.clone(),
        );
        if let Some(existing) = held.evidence_by_identity.get(&identity) {
            return Ok(EvidenceReceipt {
                receipt: EvidenceReceiptId::new(existing.clone()),
                fresh: false,
            });
        }
        let row = StoredEvidence {
            receipt: receipt.as_str().to_owned(),
            claimed_sender: evidence.claimed_sender.clone(),
            verified: evidence.verified,
            signature: evidence.signature.clone(),
            challenge_id: None,
            consumed_at_ms: None,
        };
        held.evidence.insert(receipt.as_str().to_owned(), row);
        held.evidence_by_identity
            .insert(identity, receipt.as_str().to_owned());
        Ok(EvidenceReceipt {
            receipt: receipt.clone(),
            fresh: true,
        })
    }

    async fn load_evidence_by_receipt(
        &self,
        receipt: &EvidenceReceiptId,
    ) -> Result<Option<LoadedEvidence>, StoreError> {
        Ok(self
            .locked()
            .evidence
            .get(receipt.as_str())
            .map(|row| LoadedEvidence {
                receipt: EvidenceReceiptId::new(row.receipt.clone()),
                claimed_sender: row.claimed_sender.clone(),
                verified: row.verified,
                signature: row.signature.clone(),
                challenge_id: row.challenge_id.clone().map(ApprovalChallengeId::new),
                consumed_at_ms: row.consumed_at_ms,
            }))
    }

    async fn load_delegation_by_challenge(
        &self,
        challenge: &ApprovalChallengeId,
    ) -> Result<Option<DelegationRecord>, StoreError> {
        Ok(self
            .locked()
            .delegations
            .values()
            .find(|record| record.challenge_id.as_str() == challenge.as_str())
            .cloned())
    }

    async fn live_binding_by_address(
        &self,
        address: &str,
    ) -> Result<Option<BoundChannel>, StoreError> {
        Ok(self
            .locked()
            .by_address
            .get(address)
            .map(|(principal, version, assurance)| BoundChannel {
                reference: BindingRef {
                    principal: PrincipalId::new(principal.clone()),
                    version: *version,
                },
                assurance: *assurance,
            }))
    }

    async fn address_for(&self, principal: &PrincipalId) -> Result<Option<String>, StoreError> {
        Ok(self.locked().addresses.get(principal.as_str()).cloned())
    }

    async fn await_reply(
        &self,
        address: &str,
        challenge: &ApprovalChallengeId,
        _now_ms: u64,
        _expires_at_ms: u64,
    ) -> Result<(), StoreError> {
        self.locked()
            .awaiting
            .insert(address.to_owned(), challenge.as_str().to_owned());
        Ok(())
    }

    async fn awaiting_reply(
        &self,
        address: &str,
    ) -> Result<Option<ApprovalChallengeId>, StoreError> {
        Ok(self
            .locked()
            .awaiting
            .get(address)
            .cloned()
            .map(ApprovalChallengeId::new))
    }

    async fn insert_or_get_challenge(
        &self,
        challenge: &ChallengeRecord,
    ) -> Result<InsertOutcome, StoreError> {
        let mut held = self.locked();
        let intent = challenge.scope.booking.as_str().to_owned();
        if let Some(existing_id) = held.booking_intent.get(&intent) {
            if let Some(existing) = held.challenges.get(existing_id) {
                return Ok(InsertOutcome::Existing(Box::new(existing.clone())));
            }
        }
        let key = challenge.id.as_str().to_owned();
        if held.challenges.contains_key(&key) {
            return Err(StoreError::ChallengeExists);
        }
        held.challenges.insert(key.clone(), challenge.clone());
        held.booking_intent.insert(intent, key);
        Ok(InsertOutcome::Inserted)
    }
}
