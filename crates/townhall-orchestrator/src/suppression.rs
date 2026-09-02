//! STOP that survives a restart.
//!
//! The replay window is allowed to be in-memory because the boundary makes a
//! re-admitted duplicate harmless. Suppression gets no such grace: nothing
//! downstream re-suppresses, so a store that forgot across a restart would
//! silently resume automated messages to someone who asked them stopped — and a
//! safety exit that forgets is not one (spec §2's "safety exits" row, taken at
//! its word).
//!
//! `std::fs`, no database: the crate graph forbids `sqlx` here on purpose, and
//! a newline-separated file of addresses is exactly as durable as this needs.

use std::collections::HashSet;
use std::path::PathBuf;
use std::sync::Mutex;
use townhall_channel::{ChannelAddress, SuppressionStore};

#[derive(Debug)]
pub struct FileSuppression {
    path: PathBuf,
    /// The working set — the file is the truth, this is the cache of it.
    silenced: Mutex<HashSet<String>>,
}

impl FileSuppression {
    /// Open (or create) the store at `path`, loading whatever a previous
    /// process left there — which is the entire point.
    ///
    /// # Errors
    /// The file existed and could not be read. Failing loudly here beats
    /// starting with an empty set and un-silencing everyone who asked.
    pub fn open(path: PathBuf) -> Result<Self, std::io::Error> {
        let silenced = match std::fs::read_to_string(&path) {
            Ok(contents) => contents.lines().map(str::to_owned).collect(),
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => HashSet::new(),
            Err(error) => return Err(error),
        };
        Ok(Self {
            path,
            silenced: Mutex::new(silenced),
        })
    }

    fn persist(&self, silenced: &HashSet<String>) {
        // Write-through, whole-file, atomically via a sibling temp file — a
        // torn write on the real path could half-silence someone.
        let staged = self.path.with_extension("staged");
        let mut lines: Vec<&str> = silenced.iter().map(String::as_str).collect();
        lines.sort_unstable();
        if std::fs::write(&staged, lines.join("\n")).is_ok() {
            let _ = std::fs::rename(&staged, &self.path);
        }
    }
}

impl SuppressionStore for FileSuppression {
    fn is_suppressed(&self, address: &ChannelAddress) -> bool {
        self.silenced
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .contains(address.revealed())
    }

    fn suppress(&self, address: &ChannelAddress) {
        let mut silenced = self
            .silenced
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        silenced.insert(address.revealed().to_owned());
        self.persist(&silenced);
    }

    fn allow(&self, address: &ChannelAddress) {
        let mut silenced = self
            .silenced
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        silenced.remove(address.revealed());
        self.persist(&silenced);
    }
}
