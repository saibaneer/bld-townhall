//! The trusted authority component: it verifies human approval and issues
//! grants, and it cannot touch a booking.
//!
//! Spec §5 grades the "Approval verifier / authority issuer" as a trusted
//! component that "issues `VerifiedAuthority`; does not mutate booking".
//! ADR-025 puts that grading in the crate graph: this crate sits BELOW
//! `townhall-domain` and names no mutation surface, no socket and no connection
//! pool. `townhall-orchestrator` — which holds the untrusted proposer seat —
//! must not depend on it at all, asserted by that crate's resolved-dependency
//! test.
//!
//! # What this crate refuses to make easy
//!
//! Minting authority. [`VerifiedAuthority`] has private fields and no public
//! constructor; the only routes to one are answering a real challenge and
//! loading a delegation this crate persisted. That includes tests: there is no
//! `test-support` constructor, because a cargo feature that leaks through
//! unification would close the backdoor only on paper, and a test whose premise
//! is a forged grant asserts against a fiction. `townhall-testkit`'s issuer
//! drives the same public path production does.
//!
//! # What is deliberately absent
//!
//! Anything that would let a grant be believed on its face. There is no
//! `Deserialize` for the envelope (pinned by assertion, ADR-017 point 4 as
//! amended by ADR-021), no `From<&str>`, and no way to widen a grant after
//! issuance — the issuer reads the approved scope and nothing a caller offers
//! alongside it.

pub mod assurance;
pub mod challenge;
mod codec;
mod envelope;
pub mod grant;
pub mod key;
pub mod scope;
pub mod service;
pub mod store;

pub use assurance::AssuranceLevel;
pub use challenge::{ApprovalCode, CODE_DIGITS, ChallengeRecord, ChallengeStatus, MAX_ATTEMPTS};
pub use grant::{AuthorityConstraints, BindingRef, VerifiedApproval, VerifiedAuthority};
pub use key::{EnvelopeKey, KeyTooShort};
pub use scope::{BehaviourSet, CanonicalScope, ScopeHash};
pub use service::{
    ApprovalDenied, ApprovalRequest, AuthorityPolicy, AuthorityService, BeginOutcome, Entropy,
    PendingScope, RaisedChallenge, ResolveError,
};
pub use store::{
    ApprovalStore, BoundChannel, DelegationRecord, EvidenceReceipt, InboundEvidenceRecord,
    InsertOutcome, LoadedEvidence, MemoryApprovalStore, Settled, StoreError,
};
