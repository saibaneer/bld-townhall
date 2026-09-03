//! Shared test doubles for the approve-first ports, so the unit tests exercise
//! the dispatcher's real control flow without a server.

#![allow(dead_code)] // each test file uses a different subset

use async_trait::async_trait;
use bld_types::{BookingId, PrincipalId};
use std::sync::Mutex;
use std::sync::atomic::{AtomicUsize, Ordering};
use townhall_orchestrator::{
    ApprovalError, ApprovalPort, BeginApproval, Continuation, ContinuationStore, Deposited,
    EvidenceDeposit, InboundEvidence, Raised,
};

/// The in-memory analogue of `FileContinuation` — same load/record/clear/resume
/// contract, backed by a `Vec`, so a unit test can seed a mid-flow state and read
/// the result back.
#[derive(Default)]
pub struct MemoryContinuation {
    held: Mutex<Vec<Continuation>>,
}

impl MemoryContinuation {
    #[must_use]
    pub fn new() -> Self {
        Self::default()
    }

    /// Plant a continuation, for a test that begins mid-conversation.
    pub fn seed(&self, continuation: Continuation) {
        self.locked().push(continuation);
    }

    /// Every held continuation, for readback assertions.
    #[must_use]
    pub fn all(&self) -> Vec<Continuation> {
        self.locked().clone()
    }

    fn locked(&self) -> std::sync::MutexGuard<'_, Vec<Continuation>> {
        self.held
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
    }
}

impl ContinuationStore for MemoryContinuation {
    fn load(&self, principal: &PrincipalId) -> Option<Continuation> {
        self.locked()
            .iter()
            .rev()
            .find(|held| held.principal.as_str() == principal.as_str())
            .cloned()
    }

    fn load_for_booking(&self, booking: &BookingId) -> Option<Continuation> {
        self.locked()
            .iter()
            .rev()
            .find(|held| held.booking_id.as_str() == booking.as_str())
            .cloned()
    }

    fn record(&self, continuation: Continuation) -> Result<(), String> {
        let mut held = self.locked();
        held.retain(|held| held.booking_id.as_str() != continuation.booking_id.as_str());
        held.push(continuation);
        Ok(())
    }

    fn clear(&self, challenge_id: &str) -> Result<(), String> {
        self.locked().retain(|held| held.challenge_id != challenge_id);
        Ok(())
    }

    fn take_resumable(&self) -> Vec<Continuation> {
        self.locked()
            .iter()
            .filter(|held| held.reference.is_some())
            .cloned()
            .collect()
    }
}

/// A stub approval port: `begin` returns a fixed challenge and a preview carrying
/// a known code; `reply` returns a fixed reference. Enough for the tests that
/// never actually approve (control/grammar ordering).
#[derive(Default)]
pub struct StubApprovals {
    pub begins: AtomicUsize,
    pub replies: AtomicUsize,
}

#[async_trait]
impl ApprovalPort for StubApprovals {
    async fn begin(&self, request: &BeginApproval) -> Result<Raised, ApprovalError> {
        self.begins.fetch_add(1, Ordering::SeqCst);
        Ok(Raised {
            challenge: format!("ch-{}", request.booking),
            preview: "Reply YES 0000 to approve. Maximum booking fee: £50.00.".to_owned(),
        })
    }

    async fn reply(
        &self,
        challenge: &str,
        _answer: &str,
        _code: &str,
        _receipt: &str,
    ) -> Result<Option<String>, ApprovalError> {
        self.replies.fetch_add(1, Ordering::SeqCst);
        Ok(Some(format!("ref-{challenge}")))
    }
}

/// A stub deposit port: returns a fixed challenge + receipt for any inbound.
#[derive(Default)]
pub struct StubEvidence {
    pub deposits: AtomicUsize,
}

#[async_trait]
impl EvidenceDeposit for StubEvidence {
    async fn deposit(&self, evidence: &InboundEvidence) -> Result<Deposited, ApprovalError> {
        self.deposits.fetch_add(1, Ordering::SeqCst);
        Ok(Deposited {
            challenge: format!("ch-{}", evidence.message_id),
            receipt: "receipt".to_owned(),
        })
    }
}
