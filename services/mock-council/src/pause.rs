//! Three points where a test can stop the council mid-write.
//!
//! # Why a parent process has to drive them
//!
//! Slice E needs to crash the council either side of a durable write and see what
//! survives. Cancelling a Tokio task is not a crash — the process keeps its
//! memory, its open transaction rolls back cleanly, and nothing about recovery is
//! exercised. So the council runs as a subprocess and the parent kills it.
//!
//! Which means the pause has to be *parent-visible*. An earlier design had a
//! process-local barrier, which a parent cannot wait on: the test would have to
//! sleep and hope, in exactly the tests whose purpose is to remove timing from the
//! question. So on reaching a point the child announces it on stdout and blocks
//! reading stdin, and the parent decides what happens next.
//!
//! ```text
//! child  -> PAUSED before_settle_commit EFF-1
//! parent -> SETCLOCK 1000030001          (and waits for CLOCK <ms>)
//! parent -> RELEASE                      (or SIGKILL, and nothing resumes)
//! ```
//!
//! No sleeps and no polling anywhere: every step is an acknowledged line.
//!
//! # Why this is not fault injection
//!
//! There is no endpoint and no runtime switch. The hook is behind a
//! [`Pauses`] implementation chosen at construction, and the IPC one is only ever
//! installed by the test binary. Slice E's `POST /test/faults` — a *reachable*
//! way to make the council misbehave — is still slice E's work.

use async_trait::async_trait;
use std::fmt;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum PausePoint {
    /// Inside the write transaction, after the writer lock is held and before the
    /// deadline is read.
    ///
    /// This is where a create waits while a test moves the clock past its
    /// deadline. A council that checked expiry on arrival cannot survive it: its
    /// check already passed, so it writes anyway.
    BeforeExpiryWrite,
    /// After the terminal row is written and before `COMMIT`.
    ///
    /// Killed here, nothing must be discoverable — no absence answered, no
    /// rejection recorded. It is the half of commit-before-response that a test
    /// reading *after* the response can never prove.
    BeforeSettleCommit,
    /// After `COMMIT` and before the response is written to the socket.
    ///
    /// Killed here, the answer must be reproducible: the caller never saw it, and
    /// asking again has to give the same one.
    AfterSettleCommit,
}

impl fmt::Display for PausePoint {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        let name = match self {
            Self::BeforeExpiryWrite => "before_expiry_write",
            Self::BeforeSettleCommit => "before_settle_commit",
            Self::AfterSettleCommit => "after_settle_commit",
        };
        f.write_str(name)
    }
}

#[async_trait]
pub trait Pauses: Send + Sync + fmt::Debug {
    /// Called when the council reaches `point` while handling `effect_intent_id`.
    async fn reach(&self, point: PausePoint, effect_intent_id: &str);
}

/// Pauses nowhere. What the service runs with unless a test says otherwise.
#[derive(Debug, Default, Clone, Copy)]
pub struct NeverPauses;

#[async_trait]
impl Pauses for NeverPauses {
    async fn reach(&self, _point: PausePoint, _effect_intent_id: &str) {}
}
