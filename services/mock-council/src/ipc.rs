//! The parent-driven pause protocol, over stdin/stdout.
//!
//! # The contract, line by line
//!
//! ```text
//! child  → READY <port>                      startup; the parent never sleeps to connect
//! child  → PAUSED <point> <id> <occurrence>
//! parent → SETCLOCK <occurrence> <ms>
//! child  → CLOCK <occurrence> <ms>           applied to the shared handle, then acked
//! child  → REFUSED <occurrence> <why>        a command the child will not obey, said aloud
//! parent → RELEASE <occurrence>
//! child  → RELEASED <occurrence>             the occurrence is closed
//! child  → UNKNOWN <occurrence>              any line naming a closed or unseen occurrence
//! ```
//!
//! Every line is acknowledged and every occurrence is explicitly closed, because
//! the holes this protocol went through review to close are all silences: a
//! `SETCLOCK` arriving after its `RELEASE` (undefined → `UNKNOWN`), two pauses
//! live against one uncorrelated release (→ occurrence tokens), and a parent
//! waiting forever on a child that died mid-handshake (→ the *parent's* waits
//! select on process exit; this side simply exits, which is the signal).
//!
//! # `SETCLOCK` is a global mutation, and the child says so
//!
//! There is one clock. An occurrence token authenticates *which pause authorised
//! the command*; it cannot make the effect local, because the thing being changed
//! is shared. So a `SETCLOCK` while more than one occurrence is live is answered
//! `REFUSED <occ> multiple-occurrences-live` — and refused means *nothing was
//! mutated*, not mutated-and-apologised (gate M13). A test that needs a clock
//! move alongside a second in-flight request must sequence them.
//!
//! # Only armed points pause
//!
//! The driver pauses at the points the parent named at startup and passes
//! through every other. A driver that paused everywhere would hang every
//! ordinary request in the scenario, turning single-fault tests into all-fault
//! tests.

use crate::{
    clock::SettableClock,
    pause::{PausePoint, Pauses},
};
use async_trait::async_trait;
use std::{
    collections::{HashMap, HashSet},
    io::{BufRead as _, Write as _},
    sync::{Arc, Mutex},
};
use tokio::sync::oneshot;

/// One paused request, waiting for its `RELEASE`.
struct Live {
    release: oneshot::Sender<()>,
}

struct Shared {
    armed: HashSet<PausePoint>,
    clock: Arc<dyn SettableClock>,
    next_occurrence: u64,
    live: HashMap<u64, Live>,
}

/// The child's half of the protocol.
pub struct IpcPauses {
    shared: Arc<Mutex<Shared>>,
}

impl std::fmt::Debug for IpcPauses {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str("IpcPauses")
    }
}

/// Writes one protocol line to stdout, flushed — a buffered announcement the
/// parent cannot read yet is a pause the parent cannot see.
fn say(line: &str) {
    let mut out = std::io::stdout().lock();
    let _ = writeln!(out, "{line}");
    let _ = out.flush();
}

impl IpcPauses {
    /// Start the driver: spawns the stdin-reading thread and returns the hook.
    ///
    /// A thread rather than a tokio task, deliberately: stdin reads block, and a
    /// blocked task would occupy a runtime worker for the process's lifetime.
    #[must_use]
    pub fn start(armed: HashSet<PausePoint>, clock: Arc<dyn SettableClock>) -> Arc<Self> {
        let shared = Arc::new(Mutex::new(Shared {
            armed,
            clock,
            next_occurrence: 0,
            live: HashMap::new(),
        }));

        let reader = Arc::clone(&shared);
        std::thread::spawn(move || {
            let stdin = std::io::stdin();
            for line in stdin.lock().lines() {
                let Ok(line) = line else { break };
                handle_command(&reader, line.trim());
            }
            // EOF: the parent is gone. Nothing to resume for — a paused request
            // stays paused and the process is about to be killed anyway.
        });

        Arc::new(Self { shared })
    }
}

fn handle_command(shared: &Arc<Mutex<Shared>>, line: &str) {
    let mut parts = line.split_whitespace();
    // A malformed line gets a refusal on occurrence 0 rather than silence, so a
    // parent with a protocol bug finds out.
    let (Some(verb), Some(occurrence)) = (
        parts.next(),
        parts.next().and_then(|t| t.parse::<u64>().ok()),
    ) else {
        say(&format!("REFUSED 0 unparseable-command {line:?}"));
        return;
    };

    let mut guard = shared
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner);

    match verb {
        "RELEASE" => {
            if let Some(live) = guard.live.remove(&occurrence) {
                // The acknowledgement goes out BEFORE the request resumes:
                // `RELEASED` means "the occurrence is closed", and a late
                // SETCLOCK naming it must already be answerable as UNKNOWN.
                say(&format!("RELEASED {occurrence}"));
                let _ = live.release.send(());
            } else {
                say(&format!("UNKNOWN {occurrence}"));
            }
        }
        "SETCLOCK" => {
            let Some(ms) = parts.next().and_then(|t| t.parse::<i64>().ok()) else {
                say(&format!("REFUSED {occurrence} setclock-needs-a-timestamp"));
                return;
            };
            if !guard.live.contains_key(&occurrence) {
                say(&format!("UNKNOWN {occurrence}"));
                return;
            }
            // One clock. A mutation with two live pauses would move the OTHER
            // request's deadline decision too, so it is refused — and refused
            // means nothing was mutated (gate M13), which is why this check
            // precedes the `set`.
            if guard.live.len() > 1 {
                say(&format!("REFUSED {occurrence} multiple-occurrences-live"));
                return;
            }
            guard.clock.set(ms);
            say(&format!("CLOCK {occurrence} {ms}"));
        }
        _ => {
            say(&format!("REFUSED {occurrence} unknown-verb {verb:?}"));
        }
    }
}

#[async_trait]
impl Pauses for IpcPauses {
    async fn reach(&self, point: PausePoint, effect_intent_id: &str) {
        let waiter = {
            let mut guard = self
                .shared
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner);
            if !guard.armed.contains(&point) {
                return;
            }
            let occurrence = guard.next_occurrence;
            guard.next_occurrence += 1;

            let (release, waiter) = oneshot::channel();
            guard.live.insert(occurrence, Live { release });
            say(&format!("PAUSED {point} {effect_intent_id} {occurrence}"));
            waiter
        };

        // If the reader thread is gone (parent EOF) the sender is dropped and
        // this resolves with an error; resuming is the only sane behaviour left,
        // since the parent that wanted the pause no longer exists.
        let _ = waiter.await;
    }
}
