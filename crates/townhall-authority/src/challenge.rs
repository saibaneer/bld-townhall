//! The one-time approval challenge.
//!
//! Spec §9.1: one-time use, expires, bounded attempts — three separate
//! controls, and ADR-025 records why each needs its own witness. A challenge
//! that only expired could be brute-forced within its window; one that only
//! bounded attempts could be answered a week later; one that only enforced
//! one-time use could be guessed.

use crate::assurance::AssuranceLevel;
use crate::grant::BindingRef;
use crate::scope::{CanonicalScope, ScopeHash};
use bld_types::{ApprovalChallengeId, PrincipalId};
use std::fmt;

/// How many digits a code carries.
pub const CODE_DIGITS: usize = 4;

/// How many wrong answers a challenge tolerates before it is spent.
///
/// Three, over a four-digit space: an attacker gets 3 of 10,000 per challenge,
/// and a new challenge means a new code. The bound is what makes a short code
/// safe — not the code's length.
pub const MAX_ATTEMPTS: u8 = 3;

/// A one-time approval code.
///
/// # Why `Debug` shows nothing
///
/// ADR-023 recorded the rule when the channel wanted to log a message body: an
/// unkeyed digest of a low-entropy value is an encoding of it, not concealment.
/// A four-digit code has 10,000 candidates, so a hash in a log line is the code
/// in a log line. Masking is the only honest option, and it masks completely —
/// a partial reveal of four digits is most of the secret.
#[derive(Clone, PartialEq, Eq)]
pub struct ApprovalCode(String);

impl ApprovalCode {
    /// Accept a code of exactly [`CODE_DIGITS`] ASCII digits, or refuse.
    ///
    /// Refusing here rather than truncating or padding: a code source that
    /// produced `"71"` should fail loudly at issuance, not silently create a
    /// challenge nobody can answer.
    #[must_use]
    pub fn new(text: impl Into<String>) -> Option<Self> {
        let text = text.into();
        let well_formed =
            text.len() == CODE_DIGITS && text.bytes().all(|byte| byte.is_ascii_digit());
        well_formed.then_some(Self(text))
    }

    /// Whether `candidate` is this code.
    ///
    /// Compares every byte regardless of where the first difference is. Not
    /// because a timing side channel is the threat here — [`MAX_ATTEMPTS`] is
    /// the control that matters — but because the alternative is a function
    /// whose runtime depends on a secret, and writing that deliberately invites
    /// the next person to copy it somewhere it does matter.
    #[must_use]
    pub fn matches(&self, candidate: &str) -> bool {
        let expected = self.0.as_bytes();
        let offered = candidate.as_bytes();
        if expected.len() != offered.len() {
            return false;
        }
        let mut difference = 0u8;
        for (left, right) in expected.iter().zip(offered) {
            difference |= left ^ right;
        }
        difference == 0
    }

    /// The code itself — for the outbound message, and nowhere else.
    ///
    /// `pub` because the SMS the person receives has to contain it. Named
    /// `revealed` for the reason ADR-023 named the channel address's accessor
    /// that way: the call site should read like a decision.
    #[must_use]
    pub fn revealed(&self) -> &str {
        &self.0
    }
}

impl fmt::Debug for ApprovalCode {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str("ApprovalCode(****)")
    }
}

/// Where a challenge has got to.
///
/// # Why rejection is a status and not a deletion
///
/// `NO 7312` must be terminal — a later `YES` on the same challenge must not
/// revive it (ADR-025). Deleting the row would make a replayed `YES` look like
/// an unknown challenge, which is the same denial for a different reason and
/// loses the distinction the audit needs.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ChallengeStatus {
    /// Awaiting an answer, with attempts left.
    Pending,
    /// Answered correctly; at most one grant exists for it.
    Approved,
    /// Answered `NO`. Terminal.
    Rejected,
    /// Out of attempts. Terminal.
    Exhausted,
}

impl ChallengeStatus {
    #[must_use]
    pub const fn is_settled(self) -> bool {
        !matches!(self, Self::Pending)
    }

    #[must_use]
    pub const fn name(self) -> &'static str {
        match self {
            Self::Pending => "pending",
            Self::Approved => "approved",
            Self::Rejected => "rejected",
            Self::Exhausted => "exhausted",
        }
    }

    #[must_use]
    pub fn parse(text: &str) -> Option<Self> {
        match text {
            "pending" => Some(Self::Pending),
            "approved" => Some(Self::Approved),
            "rejected" => Some(Self::Rejected),
            "exhausted" => Some(Self::Exhausted),
            _ => None,
        }
    }
}

/// One approval request, as it is persisted.
///
/// Carries the canonical scope as DATA (see [`crate::scope`]) so that the
/// booking can be resumed after approval without conversational memory, and the
/// digest beside it so a tamper check never has to re-derive what it is
/// checking.
#[derive(Clone, Debug)]
pub struct ChallengeRecord {
    pub id: ApprovalChallengeId,
    pub code: ApprovalCode,
    pub scope: CanonicalScope,
    pub scope_hash: ScopeHash,
    /// Which binding, at which revision, may answer this.
    pub binding: BindingRef,
    /// On whose behalf the grant would be issued.
    pub grantor: PrincipalId,
    /// Who the resulting action would be attributed to.
    pub subject: PrincipalId,
    pub created_at_ms: u64,
    pub attempts_used: u8,
    pub status: ChallengeStatus,
    /// The most this challenge's grant may claim, whatever the binding says.
    ///
    /// The issuer caps the grant at the binding's assurance; this records the
    /// level the challenge was raised at so the cap is checkable after the fact
    /// rather than only at issuance.
    pub assurance: AssuranceLevel,
}

impl ChallengeRecord {
    /// Attempts remaining before the challenge is spent.
    #[must_use]
    pub fn attempts_left(&self) -> u8 {
        MAX_ATTEMPTS.saturating_sub(self.attempts_used)
    }

    /// Whether the answering window has closed at `now_ms`.
    ///
    /// Reads the SCOPE's deadline, which is the one the person was shown and
    /// the one the digest covers. A separate expiry field on this record would
    /// be a second copy of a hashed value, free to disagree with it.
    #[must_use]
    pub fn has_expired(&self, now_ms: u64) -> bool {
        now_ms >= self.scope.expires_at_ms
    }
}
