//! Approve-first bookings that survive a restart.
//!
//! Two moments must be durable (ADR-026). A `BOOK` raises a challenge and creates
//! nothing, so between the preview and the person's `YES` the only record that a
//! booking is owed is the parked challenge — lose it, and a `YES` after a restart
//! answers nothing. And a `YES` mints a live grant before the booking walk
//! finishes, so between the approval and `Booked` a crash would leave a person
//! who approved with no booking and no way to recover one.
//!
//! This is [`FileSuppression`](crate::suppression::FileSuppression)'s durability,
//! reused: `std::fs` (the crate graph forbids `sqlx` here), persist-BEFORE-memory,
//! a uniquely-named staged file, `fsync`, atomic rename. The only difference is
//! the payload — a `Continuation` is structured, so the file is JSONL rather than
//! bare lines.

use crate::ports::{Continuation, ContinuationStore};
use bld_types::{BookingId, PrincipalId};
use std::path::PathBuf;
use std::sync::Mutex;

#[derive(Debug)]
pub struct FileContinuation {
    path: PathBuf,
    /// The working set — the file is the truth, this caches it. Insertion order
    /// is kept so `load` can return the most recent continuation for a principal
    /// (the latest `BOOK` supersedes an earlier one, mirroring the server's
    /// address-keyed awaiting-reply).
    held: Mutex<Vec<Continuation>>,
}

impl FileContinuation {
    /// Open (or create) the store at `path`, loading whatever a previous process
    /// left there — which is the entire point.
    ///
    /// # Errors
    /// The file existed and could not be read or parsed. Failing loudly beats
    /// starting empty and stranding every booking a prior process had approved.
    pub fn open(path: PathBuf) -> Result<Self, std::io::Error> {
        let held = match std::fs::read_to_string(&path) {
            Ok(contents) => contents
                .lines()
                .filter(|line| !line.trim().is_empty())
                .map(|line| {
                    serde_json::from_str::<Continuation>(line).map_err(|error| {
                        std::io::Error::new(std::io::ErrorKind::InvalidData, error)
                    })
                })
                .collect::<Result<Vec<_>, _>>()?,
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => Vec::new(),
            Err(error) => return Err(error),
        };
        Ok(Self {
            path,
            held: Mutex::new(held),
        })
    }

    /// Write the candidate set durably, or say why not.
    ///
    /// Copies [`FileSuppression::persist`](crate::suppression) verbatim in
    /// structure — the failure-is-returned, uniquely-staged, `fsync`ed,
    /// atomically-renamed, single-writer core the PR review forced there — over a
    /// JSONL body. Same reasons; a booking owed but not durable is the same class
    /// of lie as a STOP confirmed but not written.
    fn persist(&self, held: &[Continuation]) -> Result<(), std::io::Error> {
        use std::io::Write as _;

        static NONCE: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);
        let staged = self.path.with_extension(format!(
            "staged-{}-{}",
            std::process::id(),
            NONCE.fetch_add(1, std::sync::atomic::Ordering::SeqCst)
        ));

        let mut body = String::new();
        for continuation in held {
            let line = serde_json::to_string(continuation)
                .map_err(|error| std::io::Error::new(std::io::ErrorKind::InvalidData, error))?;
            body.push_str(&line);
            body.push('\n');
        }

        let mut file = std::fs::File::create(&staged)?;
        file.write_all(body.as_bytes())?;
        file.sync_all()?;
        drop(file);
        std::fs::rename(&staged, &self.path)?;
        // Directory sync, best effort — the rename's durability rides on it on
        // some filesystems, and a failure here is not worth losing a synced
        // record over.
        if let Some(parent) = self.path.parent() {
            if let Ok(dir) = std::fs::File::open(parent) {
                let _ = dir.sync_all();
            }
        }
        Ok(())
    }
}

impl ContinuationStore for FileContinuation {
    fn load(&self, principal: &PrincipalId) -> Option<Continuation> {
        self.held
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .iter()
            .rev()
            .find(|continuation| continuation.principal.as_str() == principal.as_str())
            .cloned()
    }

    fn load_for_booking(&self, booking: &BookingId) -> Option<Continuation> {
        self.held
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .iter()
            .rev()
            .find(|continuation| continuation.booking_id.as_str() == booking.as_str())
            .cloned()
    }

    // Persist FIRST, commit to memory second, in both directions — the other
    // order confirms a state the disk never saw, and the whole reason this store
    // exists is to be believed after a crash.

    fn record(&self, continuation: Continuation) -> Result<(), String> {
        let mut held = self
            .held
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        let mut candidate = held.clone();
        // Upsert by booking id: a redelivered BOOK, or the same booking moving
        // from parked (reference: None) to approved (reference: Some), replaces
        // its own row rather than adding a second.
        candidate.retain(|held| held.booking_id.as_str() != continuation.booking_id.as_str());
        candidate.push(continuation);
        self.persist(&candidate)
            .map_err(|error| format!("could not persist the continuation: {error}"))?;
        *held = candidate;
        Ok(())
    }

    fn clear(&self, challenge_id: &str) -> Result<(), String> {
        let mut held = self
            .held
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        let mut candidate = held.clone();
        candidate.retain(|held| held.challenge_id != challenge_id);
        self.persist(&candidate)
            .map_err(|error| format!("could not persist the continuation: {error}"))?;
        *held = candidate;
        Ok(())
    }

    fn take_resumable(&self) -> Vec<Continuation> {
        // A snapshot, not a removal: the resume runner clears each row only after
        // its booking commits, so a resume that fails leaves the row for retry.
        self.held
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .iter()
            .filter(|continuation| continuation.reference.is_some())
            .cloned()
            .collect()
    }
}
