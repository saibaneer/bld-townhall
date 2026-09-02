//! The verifier and the issuer, as one component with one public path.
//!
//! Spec §13.1 describes five and six as separate steps — the verifier emits a
//! `VerifiedApproval`, the issuer converts it into a `VerifiedAuthority`. They
//! are separate here too, but the seam between them is **private**.
//!
//! # Why the approval never escapes
//!
//! If a public function returned a `VerifiedApproval`, a caller holding a
//! legitimate approval for one scope could pass it to a public `issue` and name
//! different constraints. The evidence and the grant derived from it must not be
//! separable by anyone outside this crate, so [`AuthorityService::submit`] does
//! both in one call and hands back only the grant.
//!
//! That also settles how tests obtain authority anywhere in the workspace: they
//! call this. There is no constructor to reach for and no feature that reveals
//! one (ADR-025).

use crate::assurance::AssuranceLevel;
use crate::challenge::{ApprovalCode, ChallengeRecord, ChallengeStatus};
use crate::envelope;
use crate::grant::{BindingRef, VerifiedApproval, VerifiedAuthority};
use crate::key::EnvelopeKey;
use crate::scope::CanonicalScope;
use crate::store::{ApprovalStore, DelegationRecord, Settled, StoreError};
use bld_types::{ActorId, ApprovalChallengeId, DelegationId, PrincipalId};
use std::sync::Arc;

/// Where codes and opaque identifiers come from.
///
/// A port rather than a call into an RNG, for two reasons. Tests need a
/// deterministic sequence without seeding a global. And this crate holds no
/// capabilities by design — the OS-backed implementation belongs to the
/// composition root, which is M7B's.
pub trait Entropy: Send + Sync {
    /// A fresh one-time code.
    fn code(&self) -> ApprovalCode;

    /// A fresh unguessable identifier.
    ///
    /// Unguessable matters: a `DelegationId` is what the orchestrator presents
    /// as its opaque reference, so a predictable one is a bearer token anybody
    /// can compute.
    fn identifier(&self) -> String;
}

/// The policy an issuer applies to every challenge it raises.
#[derive(Clone, Debug)]
pub struct AuthorityPolicy {
    /// How long a person has to answer.
    pub reply_window_ms: u64,
    /// How long the resulting permission lasts, measured from the approval.
    pub grant_ttl_ms: u64,
    /// The assurance a correct SMS reply establishes.
    ///
    /// Configurable so the `--dev-authority` lane can be pinned to
    /// [`AssuranceLevel::Dev`] at its composition root (ADR-025's amendment to
    /// ADR-021) rather than pretending to be an SMS reply.
    pub assurance: AssuranceLevel,
}

impl Default for AuthorityPolicy {
    fn default() -> Self {
        Self {
            reply_window_ms: 10 * 60 * 1_000,
            grant_ttl_ms: 60 * 60 * 1_000,
            assurance: AssuranceLevel::SmsReply,
        }
    }
}

/// What a caller asks for when it wants a person's approval.
#[derive(Clone, Debug)]
pub struct ApprovalRequest {
    /// The scope, minus the two deadlines the policy supplies.
    pub scope: PendingScope,
    /// Which binding, at which revision, may answer.
    pub binding: BindingRef,
    /// On whose behalf the grant would be issued.
    pub grantor: PrincipalId,
    /// Who the resulting action would be attributed to.
    pub subject: PrincipalId,
    /// The AUTHENTICATED workload that will present the resulting grant.
    ///
    /// Supplied by the caller's own credential, not derived from the subject —
    /// see migration 0007 for why this is settled when the challenge is raised
    /// rather than when it is answered. The preview names this agent, so the
    /// person is approving THIS workload and no other.
    pub actor: ActorId,
}

/// A scope before the issuer stamps its deadlines onto it.
///
/// # Why the caller does not supply the deadlines
///
/// They are policy, and a caller that could choose them could ask for a
/// thousand-year permission. The issuer owns both, so the scope a person
/// approves cannot outlive what the service permits.
#[derive(Clone, Debug)]
pub struct PendingScope {
    pub service: bld_types::ServiceId,
    pub agent: String,
    pub booking: bld_types::BookingId,
    pub behaviours: crate::scope::BehaviourSet,
    pub requirements: bld_types::BookingRequirements,
}

/// What an approval produced.
#[derive(Clone, Debug)]
pub struct IssuedGrant {
    /// The opaque reference a caller presents afterwards.
    ///
    /// Everything else about the grant stays here: spec §13.1 step 7 gives the
    /// agent "only the resulting narrow authority reference/grant, never an
    /// SMS-derived trust-me flag", and a reference is the narrowest thing that
    /// can be handed over.
    pub reference: DelegationId,
    /// When it stops working, so a caller can say so without guessing.
    pub expires_at_ms: u64,
}

/// A raised challenge: what to send, and what to send it about.
#[derive(Clone, Debug)]
pub struct RaisedChallenge {
    pub id: ApprovalChallengeId,
    /// The preview text, rendered from the scope that was hashed.
    pub preview: String,
    /// The code, for the outbound message only.
    pub code: ApprovalCode,
}

/// Why an approval was refused.
///
/// Spec §20 names `ChallengeExpired`, `WrongCode`, `Replay` and
/// `AttemptsExceeded`; the rest are this implementation's honest additions.
/// Each is distinct because the acceptance gate requires each denial to be
/// provable in isolation — "it was refused" is not a witness for which check
/// refused it.
#[derive(Debug, thiserror::Error, PartialEq, Eq)]
pub enum ApprovalDenied {
    #[error("no such challenge")]
    UnknownChallenge,
    #[error("that challenge has expired")]
    ChallengeExpired,
    #[error("wrong code; {attempts_left} attempt(s) left")]
    WrongCode { attempts_left: u8 },
    #[error("too many wrong attempts")]
    AttemptsExceeded,
    #[error("that reply did not come from the channel the challenge was sent to")]
    WrongChannel,
    /// A replayed approval, or a concurrent one that lost.
    #[error("that challenge was already {0}")]
    Replay(&'static str),
    #[error("the authority store could not be reached: {0}")]
    Unavailable(String),
    /// The stored challenge contradicts itself. Never "close enough".
    #[error("that challenge could not be read")]
    Unreadable,
}

impl From<StoreError> for ApprovalDenied {
    fn from(error: StoreError) -> Self {
        match error {
            StoreError::UnknownChallenge => Self::UnknownChallenge,
            other => Self::Unavailable(other.to_string()),
        }
    }
}

/// Why a presented grant did not resolve.
#[derive(Debug, thiserror::Error, PartialEq, Eq)]
pub enum ResolveError {
    #[error("no such delegation")]
    Unknown,
    #[error("that delegation was revoked")]
    Revoked,
    #[error("that delegation has expired")]
    Expired,
    /// The row exists and does not decode. Never treated as "close enough".
    #[error("that delegation could not be read")]
    Unreadable,
    #[error("the authority store could not be reached: {0}")]
    Unavailable(String),
}

/// The trusted authority component (spec §5).
///
/// # Why the store is not reachable through this type
///
/// It was, briefly, behind a `store()` accessor "for callers that own both".
/// That accessor was a complete minting path, and glm-5.3-flash found it in
/// review before this shipped: [`ApprovalStore::insert_challenge`] is public
/// (it must be — the SQL implementation lives in another crate),
/// [`crate::ChallengeRecord`]'s fields are public, and
/// [`crate::ApprovalCode::new`] is public. So anyone holding an
/// `AuthorityService` could insert a challenge carrying a code THEY chose, over
/// any scope, naming any grantor — then answer it and receive a real grant,
/// with nobody ever texted. "The only route to authority is answering a real
/// challenge" held only if the challenge was real.
///
/// So holding this type grants no store access. What remains true, and is
/// stated rather than papered over: **an `ApprovalStore` implementor is trusted
/// infrastructure.** Whoever can write its rows can write grants, exactly as
/// whoever can write the database can. The defence is that the store is
/// reachable only from a composition root, never from a crate that merely holds
/// the service — and a keyed MAC over the challenge, which would beat even a
/// row writer, needs a key to live somewhere and is therefore M7B's.
pub struct AuthorityService<S, E> {
    store: Arc<S>,
    entropy: E,
    policy: AuthorityPolicy,
    /// Authenticates every envelope this service writes, and is required to
    /// read one back. Held here rather than passed per call so no code path can
    /// accidentally encode without it.
    key: EnvelopeKey,
}

impl<S: ApprovalStore, E: Entropy> AuthorityService<S, E> {
    /// Build the component over a store the CALLER owns a handle to.
    ///
    /// `Arc` rather than ownership so a composition root — or a test asserting
    /// on rows — can keep its own handle, without this type having to hand one
    /// out to whoever holds it.
    pub fn new(store: Arc<S>, entropy: E, policy: AuthorityPolicy, key: EnvelopeKey) -> Self {
        Self {
            store,
            entropy,
            policy,
            key,
        }
    }

    /// Raise a challenge: step 2 of §13.1.
    ///
    /// The preview returned here is rendered from the very scope whose digest is
    /// persisted — see [`crate::scope`] for why that is a structural property
    /// and not a convention.
    ///
    /// # Errors
    /// The store refused the new challenge, or could not be reached. A
    /// challenge that cannot be persisted must not be sent: the person would
    /// receive a code that no reply could ever satisfy.
    pub async fn begin(
        &self,
        request: &ApprovalRequest,
        now_ms: u64,
    ) -> Result<RaisedChallenge, StoreError> {
        let scope = CanonicalScope {
            service: request.scope.service.clone(),
            agent: request.scope.agent.clone(),
            booking: request.scope.booking.clone(),
            behaviours: request.scope.behaviours.clone(),
            requirements: request.scope.requirements.clone(),
            expires_at_ms: now_ms.saturating_add(self.policy.reply_window_ms),
            grant_ttl_ms: self.policy.grant_ttl_ms,
        };
        let code = self.entropy.code();
        let id = ApprovalChallengeId::new(self.entropy.identifier());
        let preview = scope.preview(code.revealed(), now_ms);

        let record = ChallengeRecord {
            id: id.clone(),
            code: code.clone(),
            scope_hash: scope.digest(),
            scope,
            binding: request.binding.clone(),
            grantor: request.grantor.clone(),
            subject: request.subject.clone(),
            created_at_ms: now_ms,
            attempts_used: 0,
            status: ChallengeStatus::Pending,
            assurance: self.policy.assurance,
            actor: request.actor.clone(),
        };
        self.store.insert_challenge(&record).await?;
        Ok(RaisedChallenge { id, preview, code })
    }

    /// Answer `YES`: steps 4 to 6 of §13.1, and the only route to a grant.
    ///
    /// # The order of the checks, and why
    ///
    /// 1. **Unknown** — nothing to answer.
    /// 2. **Already settled** — a replay, or a concurrent reply that lost. Asked
    ///    before the code, so a replayed CORRECT code is refused as a replay
    ///    rather than counted as an attempt.
    /// 3. **Expired** — before the code, so a late correct answer never reveals
    ///    that it was correct.
    /// 4. **Wrong channel** — before the code, and it does NOT consume an
    ///    attempt. Consuming one would let anyone who learns a challenge id
    ///    burn the real person's three tries from another number; refusing
    ///    first gives an attacker nothing, because they never reach the code
    ///    check at all.
    /// 5. **Code** — and only a wrong code costs an attempt.
    ///
    /// # Errors
    /// One [`ApprovalDenied`] per check, each distinct so the acceptance gate's
    /// "denied independently" can name which check refused.
    pub async fn submit(
        &self,
        id: &ApprovalChallengeId,
        offered_code: &str,
        from: &BindingRef,
        binding_assurance: AssuranceLevel,
        now_ms: u64,
    ) -> Result<VerifiedAuthority, ApprovalDenied> {
        let challenge = self.pending(id, from, now_ms).await?;

        if !challenge.code.matches(offered_code) {
            let (attempts_left, status) = self.store.record_failed_attempt(id, now_ms).await?;
            return Err(if status == ChallengeStatus::Exhausted {
                ApprovalDenied::AttemptsExceeded
            } else {
                ApprovalDenied::WrongCode { attempts_left }
            });
        }

        // Step 5's evidence. It exists only inside this function.
        let approval = VerifiedApproval::new(
            challenge.id.clone(),
            challenge.scope.clone(),
            challenge.binding.clone(),
            challenge.assurance,
            now_ms,
        );

        // Step 6, with the cap. A binding cannot lift a grant above the
        // assurance it established, and the challenge cannot lift it above the
        // policy it was raised under: the weakest of the three wins.
        let assurance = challenge.assurance.min(binding_assurance);
        let delegation = DelegationId::new(self.entropy.identifier());
        let authority = VerifiedAuthority::issue(
            delegation,
            &approval,
            challenge.grantor.clone(),
            challenge.subject.clone(),
            // The actor the CHALLENGE recorded, which is the workload the
            // person was told about. Not the caller of this method: a
            // different workload answering must not receive a grant naming
            // itself.
            challenge.actor.clone(),
            assurance,
        );

        let record = DelegationRecord {
            id: authority.delegation().clone(),
            grantor: authority.grantor().clone(),
            subject: authority.subject().clone(),
            service: authority.service().clone(),
            issued_at_ms: authority.issued_at_ms(),
            expires_at_ms: authority.expires_at_ms(),
            revoked_at_ms: None,
            envelope: envelope::encode(&authority, &self.key),
        };

        match self.store.settle_with_grant(id, &record).await? {
            Settled::Now => Ok(authority),
            Settled::Already(status) => Err(ApprovalDenied::Replay(status.name())),
        }
    }

    /// Answer `NO`: terminal, and no grant.
    ///
    /// Requires the code and the binding, exactly as `YES` does. A rejection
    /// anyone could send is a denial-of-service on someone else's booking.
    ///
    /// # Errors
    /// As [`Self::submit`]: unknown, expired, wrong channel, wrong code, out of
    /// attempts, or already settled.
    pub async fn reject(
        &self,
        id: &ApprovalChallengeId,
        offered_code: &str,
        from: &BindingRef,
        now_ms: u64,
    ) -> Result<(), ApprovalDenied> {
        let challenge = self.pending(id, from, now_ms).await?;
        if !challenge.code.matches(offered_code) {
            let (attempts_left, status) = self.store.record_failed_attempt(id, now_ms).await?;
            return Err(if status == ChallengeStatus::Exhausted {
                ApprovalDenied::AttemptsExceeded
            } else {
                ApprovalDenied::WrongCode { attempts_left }
            });
        }
        match self.store.settle_rejected(id).await? {
            Settled::Now => Ok(()),
            Settled::Already(status) => Err(ApprovalDenied::Replay(status.name())),
        }
    }

    /// Resolve a presented reference into a live grant.
    ///
    /// Revocation is checked HERE and not on the grant value, because a value in
    /// hand cannot know it was revoked a moment ago. Every mutation resolves
    /// afresh; ADR-025 records what revocation does and does not reach.
    ///
    /// # Errors
    /// Unknown, revoked, expired, or a row that does not decode — never
    /// "close enough".
    pub async fn resolve(
        &self,
        id: &DelegationId,
        now_ms: u64,
    ) -> Result<VerifiedAuthority, ResolveError> {
        let record = self
            .store
            .load_delegation(id)
            .await
            .map_err(|error| ResolveError::Unavailable(error.to_string()))?
            .ok_or(ResolveError::Unknown)?;

        if record.revoked_at_ms.is_some() {
            return Err(ResolveError::Revoked);
        }
        if now_ms >= record.expires_at_ms {
            return Err(ResolveError::Expired);
        }
        envelope::decode(&record.envelope, &self.key).ok_or(ResolveError::Unreadable)
    }

    /// Revoke a grant. Idempotent — `false` means it was already revoked or
    /// never existed, and neither is an error for a safety exit (spec §2).
    ///
    /// # Errors
    /// The store could not be reached.
    pub async fn revoke(&self, id: &DelegationId, now_ms: u64) -> Result<bool, StoreError> {
        self.store.revoke_delegation(id, now_ms).await
    }

    /// The four checks every answer passes before its code is even looked at.
    async fn pending(
        &self,
        id: &ApprovalChallengeId,
        from: &BindingRef,
        now_ms: u64,
    ) -> Result<ChallengeRecord, ApprovalDenied> {
        let challenge = self
            .store
            .load_challenge(id)
            .await?
            .ok_or(ApprovalDenied::UnknownChallenge)?;

        // Two layers guard replay, and the mutation battery proved each
        // sufficient on its own SEQUENTIALLY: this one, and the store's atomic
        // check inside `settle_with_grant`. Neither is redundant.
        //
        // This one gives the answer without spending an attempt or building
        // evidence, and it is the only one `reject` reaches at all. The store's
        // is the only one that holds when two correct replies arrive at once —
        // see the concurrency test, which is the sole witness for it.
        // The stored digest must describe the stored scope.
        //
        // The SQL store checks this too, on the way out of the row. It is
        // checked HERE as well because that made it a property of one
        // implementation rather than of the component: an in-memory store, a
        // future store, or a fabricated row all reach this line. If the two
        // disagree, one was edited, and choosing which to believe would be
        // choosing whose version of the approval stands.
        if challenge.scope_hash != challenge.scope.digest() {
            return Err(ApprovalDenied::Unreadable);
        }
        if challenge.status.is_settled() {
            return Err(ApprovalDenied::Replay(challenge.status.name()));
        }
        if challenge.has_expired(now_ms) {
            return Err(ApprovalDenied::ChallengeExpired);
        }
        if &challenge.binding != from {
            return Err(ApprovalDenied::WrongChannel);
        }
        Ok(challenge)
    }
}
