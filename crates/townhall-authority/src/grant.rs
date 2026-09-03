//! The approval evidence, and the grant issued from it.
//!
//! # Why these types have private fields when the rest of the domain's do not
//!
//! ADR-021 amended away ADR-017's private-field ceremony for the verdict enums,
//! with reasons: the trusted half legitimately constructs them, an enum cannot
//! hide a variant's fields without breaking that, and forgery was already
//! unrepresentable three other ways. It kept exactly one piece of that ceremony
//! for M7 — "`VerifiedAuthority`'s constructor ceremony belongs to M7, with its
//! issuer" — and this is it.
//!
//! The reason it could not be deferred further: until now the only route from a
//! bearer token to an authority was the server's resolver, so the type's public
//! fields were unreachable in practice. M7 adds an issuer, a store and a
//! delegation row, and "in practice" stops being an argument. Public fields
//! would mean any crate — including the one holding the untrusted proposer seat
//! — could write the struct literal and mint whatever it liked. Moving the
//! issuer to its own crate does not fix that on its own; that was one of the
//! three reasons ADR-025 records as void.
//!
//! # The chain, stated once
//!
//! A [`VerifiedApproval`] can only be built inside this crate, by the verifier,
//! from a challenge it consumed. A [`VerifiedAuthority`] can only be built from
//! a `VerifiedApproval` or loaded from a delegation this crate persisted. No
//! public function hands out a `VerifiedApproval`, so the only way to obtain
//! authority anywhere in the workspace — production code and tests alike — is
//! to answer a real challenge.

use crate::assurance::AssuranceLevel;
use crate::scope::{BehaviourSet, CanonicalScope, ScopeHash};
use bld_types::{
    ActorId, ApprovalChallengeId, Behaviour, BookingId, DelegationId, Money, PrincipalId, ServiceId,
};

/// A channel binding's identity and revision.
///
/// # Why a version travels with it
///
/// The binding is what makes "the reply came from the number we texted" mean
/// anything. Bindings change: a number is re-verified, moved to another
/// principal, or withdrawn. A challenge bound only to a `PrincipalId` or to a
/// normalized phone string would still verify after the binding beneath it had
/// moved — the recurring defect of this project's last three reviews, which is
/// state outliving the moment it was true.
///
/// So the challenge records which binding, at which revision, and the verifier
/// compares both.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct BindingRef {
    pub principal: PrincipalId,
    /// The binding row's revision, incremented whenever its verification
    /// evidence or status changes.
    pub version: u64,
}

/// What an authority may do, beyond which behaviours it names.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct AuthorityConstraints {
    max_fee: Money,
    /// The headcount the person was shown, and approved.
    ///
    /// # Why the grant carries this and not only the fee
    ///
    /// The preview says `Attendees: <= 20`. Until review found it, the grant
    /// kept only the fee ceiling and the booking id — so a holder of that grant
    /// could send `UpdateRequirements` with 500 attendees and carry on under
    /// the same approval. The money was bounded and the booking was not: Lucy
    /// approved a room for twenty people and could end up with a booking for
    /// five hundred.
    ///
    /// A ceiling rather than an exact value, matching what the preview
    /// promises: fewer people than approved is not a widening.
    max_attendees: u16,
    /// The exact resources this grant reaches.
    ///
    /// # Why "one booking" is not a boolean
    ///
    /// Spec §23.1 issues authority "with one-booking and £50 constraints", and
    /// a `one_booking: bool` beside a resource list would be two statements of
    /// one fact — the second one free to disagree. One booking is what a
    /// one-element resource list MEANS.
    ///
    /// It is also what keeps grant reuse honest: a grant is presented on every
    /// call of an approved workflow (create, select, verify, book), so "one
    /// booking" cannot mean "one use" without breaking the workflow it
    /// authorizes (ADR-025).
    resources: Vec<BookingId>,
}

impl AuthorityConstraints {
    #[must_use]
    pub fn new(
        max_fee: Money,
        max_attendees: u16,
        resources: impl IntoIterator<Item = BookingId>,
    ) -> Self {
        Self {
            max_fee,
            max_attendees,
            resources: resources.into_iter().collect(),
        }
    }

    #[must_use]
    pub fn max_fee(&self) -> Money {
        self.max_fee
    }

    /// The headcount ceiling the person approved.
    #[must_use]
    pub fn max_attendees(&self) -> u16 {
        self.max_attendees
    }

    #[must_use]
    pub fn resources(&self) -> &[BookingId] {
        &self.resources
    }

    #[must_use]
    pub fn reaches(&self, booking: &BookingId) -> bool {
        self.resources.contains(booking)
    }
}

/// Human approval evidence, validated (spec §12's `VerifiedApproval<T>`).
///
/// Constructible only by this crate's verifier, and returned by no public
/// function — see this module's header. Holding one is proof a challenge was
/// answered, not a claim that it was.
#[derive(Clone, Debug)]
pub struct VerifiedApproval {
    challenge: ApprovalChallengeId,
    scope: CanonicalScope,
    binding: BindingRef,
    assurance: AssuranceLevel,
    approved_at_ms: u64,
}

impl VerifiedApproval {
    pub(crate) fn new(
        challenge: ApprovalChallengeId,
        scope: CanonicalScope,
        binding: BindingRef,
        assurance: AssuranceLevel,
        approved_at_ms: u64,
    ) -> Self {
        Self {
            challenge,
            scope,
            binding,
            assurance,
            approved_at_ms,
        }
    }

    #[must_use]
    pub fn challenge(&self) -> &ApprovalChallengeId {
        &self.challenge
    }

    #[must_use]
    pub fn scope(&self) -> &CanonicalScope {
        &self.scope
    }

    #[must_use]
    pub fn binding(&self) -> &BindingRef {
        &self.binding
    }

    #[must_use]
    pub fn assurance(&self) -> AssuranceLevel {
        self.assurance
    }

    #[must_use]
    pub fn approved_at_ms(&self) -> u64 {
        self.approved_at_ms
    }
}

/// One verified authority grant (spec §13's envelope).
///
/// # Why three principals where the spec sketch has one
///
/// §13's sketch carries a single `principal`, and ADR-022 spent that one field
/// three times: the booking's owner at create, the visibility predicate in SQL,
/// and the requester persisted in the cancellation plan. ADR-020 had already
/// promised that the booker and the canceller need not be the same person, and
/// one field cannot keep that promise — copying the sketch literally would have
/// carried the ambiguity into the schema (ADR-025).
///
/// - `grantor` — on whose behalf. The booking's owner, and the visibility scope.
/// - `subject` — who the action is attributed to. ADR-020's requester.
/// - `actor` — the authenticated workload that presented the grant.
///
/// Lucy books her own hall: grantor = subject = `lucy`. Marco cancels it under
/// delegation: grantor = `lucy`, subject = `marco`, actor = Marco's agent.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct VerifiedAuthority {
    delegation: DelegationId,
    grantor: PrincipalId,
    subject: PrincipalId,
    actor: ActorId,
    service: ServiceId,
    behaviours: BehaviourSet,
    constraints: AuthorityConstraints,
    scope_hash: ScopeHash,
    issued_at_ms: u64,
    expires_at_ms: u64,
    assurance: AssuranceLevel,
}

impl VerifiedAuthority {
    /// Issue a grant from approval evidence.
    ///
    /// `pub(crate)` deliberately. Public would be almost safe — a caller cannot
    /// fabricate the `VerifiedApproval` this needs — but "almost" hides a real
    /// widening: a caller holding a legitimate approval for one scope could
    /// issue an authority naming different constraints. The approval and the
    /// grant derived from it must not be separable by anyone outside the
    /// issuer.
    pub(crate) fn issue(
        delegation: DelegationId,
        approval: &VerifiedApproval,
        grantor: PrincipalId,
        subject: PrincipalId,
        actor: ActorId,
        assurance: AssuranceLevel,
    ) -> Self {
        let scope = approval.scope();
        Self {
            delegation,
            grantor,
            subject,
            actor,
            service: scope.service.clone(),
            behaviours: scope.behaviours.clone(),
            constraints: AuthorityConstraints::new(
                scope.requirements.max_fee,
                scope.requirements.attendees,
                [scope.booking.clone()],
            ),
            scope_hash: scope.digest(),
            issued_at_ms: approval.approved_at_ms(),
            // The grant's clock starts at the APPROVAL, not at the offer.
            //
            // `scope.expires_at_ms` is the deadline for answering, and using it
            // here was the first version's bug: approving in the last second of
            // the reply window issued a grant that had already expired. Both
            // deadlines are in the scope, both are hashed, and both are shown
            // to the person, precisely so this arithmetic is checkable.
            expires_at_ms: approval.approved_at_ms().saturating_add(scope.grant_ttl_ms),
            assurance,
        }
    }

    /// Rebuild a grant from a delegation this crate persisted.
    ///
    /// `pub(crate)` for the same reason as [`Self::issue`]: the only callers are
    /// the store's decoder inside this crate, and the resolver that reads it.
    #[allow(clippy::too_many_arguments)]
    pub(crate) fn restore(
        delegation: DelegationId,
        grantor: PrincipalId,
        subject: PrincipalId,
        actor: ActorId,
        service: ServiceId,
        behaviours: BehaviourSet,
        constraints: AuthorityConstraints,
        scope_hash: ScopeHash,
        issued_at_ms: u64,
        expires_at_ms: u64,
        assurance: AssuranceLevel,
    ) -> Self {
        Self {
            delegation,
            grantor,
            subject,
            actor,
            service,
            behaviours,
            constraints,
            scope_hash,
            issued_at_ms,
            expires_at_ms,
            assurance,
        }
    }

    #[must_use]
    pub fn delegation(&self) -> &DelegationId {
        &self.delegation
    }

    /// On whose behalf — the booking's owner, and the visibility scope.
    #[must_use]
    pub fn grantor(&self) -> &PrincipalId {
        &self.grantor
    }

    /// Who the action is attributed to (ADR-020's requester).
    #[must_use]
    pub fn subject(&self) -> &PrincipalId {
        &self.subject
    }

    #[must_use]
    pub fn actor(&self) -> &ActorId {
        &self.actor
    }

    #[must_use]
    pub fn service(&self) -> &ServiceId {
        &self.service
    }

    #[must_use]
    pub fn behaviours(&self) -> &BehaviourSet {
        &self.behaviours
    }

    #[must_use]
    pub fn constraints(&self) -> &AuthorityConstraints {
        &self.constraints
    }

    /// The digest of the scope a person actually approved.
    ///
    /// Kept so a later mutation can be checked against what was shown, rather
    /// than against a re-derivation of what someone says was shown.
    #[must_use]
    pub fn scope_hash(&self) -> ScopeHash {
        self.scope_hash
    }

    #[must_use]
    pub fn issued_at_ms(&self) -> u64 {
        self.issued_at_ms
    }

    #[must_use]
    pub fn expires_at_ms(&self) -> u64 {
        self.expires_at_ms
    }

    #[must_use]
    pub fn assurance(&self) -> AssuranceLevel {
        self.assurance
    }

    #[must_use]
    pub fn max_fee(&self) -> Money {
        self.constraints.max_fee()
    }

    /// The headcount ceiling this grant permits.
    #[must_use]
    pub fn max_attendees(&self) -> u16 {
        self.constraints.max_attendees()
    }

    /// Whether this grant names `behaviour` over `booking`.
    ///
    /// # Why time is not a parameter
    ///
    /// An earlier version took `now_ms` and folded expiry in. That made a
    /// SECOND place where liveness is decided, and the two would answer
    /// differently: this one can see expiry, and only the resolver can see
    /// REVOCATION — it has the store. A caller checking `permits(…, now)` and
    /// believing it had asked the whole question would sail past a grant
    /// revoked a moment ago.
    ///
    /// So liveness belongs to `AuthorityService::resolve` alone, and a grant
    /// that reached this method has already been proven live by it. What is
    /// left is the part a value in hand can actually answer: does this grant
    /// name this behaviour, over this resource. No clock, which also keeps one
    /// out of the domain (spec §2's "no HTTP or SMS logic in the domain" has
    /// the same motive).
    #[must_use]
    pub fn covers(&self, behaviour: Behaviour, booking: &BookingId) -> bool {
        self.behaviours.permits(behaviour) && self.constraints.reaches(booking)
    }
}
