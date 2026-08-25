//! The council's clock. There is exactly one.
//!
//! # Why that sentence is the whole module
//!
//! An earlier design read the deadline inside SQL — `WHERE unixepoch() <=
//! expires_at_ms` — so that the comparison and the write could not be
//! interleaved. It looked stronger and was worse: `SQLite` reads its own host clock
//! through its VFS, not this trait, so the council would have had **two** clocks,
//! and the one that decided was the one no test could move. A harness advancing
//! the injected clock past a deadline, releasing a paused create, and watching the
//! booking succeed anyway.
//!
//! One fact in two places, which is this project's recurring defect. So: no
//! `unixepoch`, no `CURRENT_TIMESTAMP`, no `datetime()`, no column defaults
//! involving time, no triggers reading a clock. Every deadline comparison in this
//! service reads [`Clock::now_ms`] and nothing else, and a test asserts the SQL
//! stays clean of the alternatives.
//!
//! What ADR-016 §1 asks for is that the reading happen *inside the write
//! transaction*, after the writer lock — so a request that queued is judged on
//! when it reached the write, not on when it arrived. That is a discipline about
//! *where* `now_ms` is called, not about who calls it.

use std::{
    fmt,
    sync::atomic::{AtomicI64, Ordering},
    time::{SystemTime, UNIX_EPOCH},
};

pub trait Clock: Send + Sync + fmt::Debug {
    /// Wall-clock milliseconds since the Unix epoch.
    fn now_ms(&self) -> i64;
}

/// A clock that can be moved — the pause driver's requirement.
///
/// Separate from [`Clock`] deliberately: the registry needs only to read, and a
/// driver holding a read-only handle could acknowledge a `SETCLOCK` it has no
/// way to apply. The driver is constructed with the *same* handle the registry
/// reads, so a clock command and the deadline comparison after the pause cannot
/// disagree — there is no second source of time to reconcile.
pub trait SettableClock: Clock {
    fn set(&self, now_ms: i64);
}

#[derive(Debug, Default, Clone, Copy)]
pub struct SystemClock;

impl Clock for SystemClock {
    fn now_ms(&self) -> i64 {
        SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .map_or(0, |elapsed| {
                i64::try_from(elapsed.as_millis()).unwrap_or(i64::MAX)
            })
    }
}

/// A clock a test can move, forwards and backwards.
///
/// Backwards matters as much as forwards: ADR-016 §4 exists because a clock that
/// steps back would otherwise let an intent commit after its absence was already
/// reported, and the only way to gate that is to actually wind one back.
#[derive(Debug)]
pub struct TestClock {
    now_ms: AtomicI64,
}

impl TestClock {
    #[must_use]
    pub const fn at(now_ms: i64) -> Self {
        Self {
            now_ms: AtomicI64::new(now_ms),
        }
    }

    pub fn set(&self, now_ms: i64) {
        self.now_ms.store(now_ms, Ordering::SeqCst);
    }

    pub fn advance(&self, by_ms: i64) {
        self.now_ms.fetch_add(by_ms, Ordering::SeqCst);
    }
}

impl Clock for TestClock {
    fn now_ms(&self) -> i64 {
        self.now_ms.load(Ordering::SeqCst)
    }
}

impl SettableClock for TestClock {
    fn set(&self, now_ms: i64) {
        Self::set(self, now_ms);
    }
}
