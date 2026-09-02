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

    /// Write the candidate set durably, or say why not.
    ///
    /// # The three corrections the PR review forced
    ///
    /// - **Failure is returned, not discarded.** The first version's `let _ =`
    ///   meant a full disk produced a confirmed STOP that lasted until the next
    ///   restart — the M5.1 shim lesson (`unwrap_or(0)`) replayed on a safety
    ///   write.
    /// - **The staged file is `fsync`ed and uniquely named.** Rename atomicity
    ///   without a sync is not crash durability; a FIXED sibling name is a
    ///   collision between writers and, on a shared machine, a symlink target.
    /// - **This store is single-writer by contract** — one dispatcher process
    ///   owns one file. Two instances over one path are last-writer-wins on
    ///   the whole set, which is a deployment error this type cannot repair,
    ///   only document.
    fn persist(&self, silenced: &HashSet<String>) -> Result<(), std::io::Error> {
        use std::io::Write as _;

        static NONCE: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);
        let staged = self.path.with_extension(format!(
            "staged-{}-{}",
            std::process::id(),
            NONCE.fetch_add(1, std::sync::atomic::Ordering::SeqCst)
        ));

        let mut lines: Vec<&str> = silenced.iter().map(String::as_str).collect();
        lines.sort_unstable();

        let mut file = std::fs::File::create(&staged)?;
        file.write_all(lines.join("\n").as_bytes())?;
        file.sync_all()?;
        drop(file);
        std::fs::rename(&staged, &self.path)?;
        // Directory sync, best effort: the rename's durability rides on it on
        // some filesystems, and a failure here is not worth un-suppressing over
        // when the data itself is already synced.
        if let Some(parent) = self.path.parent() {
            if let Ok(dir) = std::fs::File::open(parent) {
                let _ = dir.sync_all();
            }
        }
        Ok(())
    }
}

impl SuppressionStore for FileSuppression {
    fn is_suppressed(&self, address: &ChannelAddress) -> bool {
        self.silenced
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .contains(address.revealed())
    }

    // Persist FIRST, commit to memory second, in both directions. The other
    // order confirms a state the disk never saw: memory says suppressed, the
    // file says not, and the restart resurrects the automated messages the
    // human asked stopped.

    fn suppress(&self, address: &ChannelAddress) -> Result<(), String> {
        let mut silenced = self
            .silenced
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        let mut candidate = silenced.clone();
        candidate.insert(address.revealed().to_owned());
        self.persist(&candidate)
            .map_err(|error| format!("could not persist the stop list: {error}"))?;
        *silenced = candidate;
        Ok(())
    }

    fn allow(&self, address: &ChannelAddress) -> Result<(), String> {
        let mut silenced = self
            .silenced
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        let mut candidate = silenced.clone();
        candidate.remove(address.revealed());
        self.persist(&candidate)
            .map_err(|error| format!("could not persist the stop list: {error}"))?;
        *silenced = candidate;
        Ok(())
    }
}
