//! One definition of "the same message", used everywhere it matters.

use bld_types::BookingId;
use sha2::{Digest as _, Sha256};
use std::collections::HashMap;
use std::fmt::Write as _;

// The lock loom models under the deterministic lane, std everywhere else. Loom
// mirrors std's API, so the swap is the import and nothing downstream.
#[cfg(feature = "loom")]
use loom::sync::Mutex;
#[cfg(not(feature = "loom"))]
use std::sync::Mutex;

/// What makes an inbound message *this* message.
///
/// # One identity, not two
///
/// An earlier design keyed the dedupe window on `(channel, provider_message_id)`
/// while deriving the booking id from `(channel, address, provider_message_id)` —
/// two definitions both claiming to be the identity, which is how a redelivery
/// gets deduped by one and duplicated by the other. This is the single
/// definition, and the derived booking id is built from exactly it.
///
/// The address is deliberately *not* part of it: a provider message id is
/// already unique within a provider account, and including the address would
/// make the identity depend on normalization succeeding the same way twice.
#[derive(Clone, Debug, PartialEq, Eq, Hash)]
pub struct InboundIdentity {
    pub provider: String,
    pub provider_account: String,
    pub provider_message_id: String,
}

impl InboundIdentity {
    #[must_use]
    pub fn new(
        provider: impl Into<String>,
        provider_account: impl Into<String>,
        provider_message_id: impl Into<String>,
    ) -> Self {
        Self {
            provider: provider.into(),
            provider_account: provider_account.into(),
            provider_message_id: provider_message_id.into(),
        }
    }

    /// The booking id this message deterministically names.
    ///
    /// This is ADR-014's stable-effect-identity discipline applied one layer
    /// out: the message IS the intent, so it names the intent. A carrier
    /// redelivery — even after a restart has emptied the replay window — derives
    /// the same id, the server answers `AlreadyExists`, and the duplicate
    /// becomes a report about the original rather than a second booking.
    ///
    /// # Why a real digest, and why length-prefixed
    ///
    /// The components are provider-controlled text, so two rules hold. First,
    /// fields are length-prefixed into the hash — naive joining with a
    /// delimiter is not injective when a field may contain the delimiter, and
    /// `("a\u{1f}b", "c")` colliding with `("a", "b\u{1f}c")` would let one
    /// message shadow another. Second, SHA-256 rather than something fast and
    /// forgeable: collision resistance here should not lean on M5.1's ownership
    /// concealment as a backstop, even though it would hold.
    #[must_use]
    pub fn booking_id(&self) -> BookingId {
        let mut hasher = Sha256::new();
        for part in [
            &self.provider,
            &self.provider_account,
            &self.provider_message_id,
        ] {
            hasher.update(u64::try_from(part.len()).unwrap_or(u64::MAX).to_be_bytes());
            hasher.update(part.as_bytes());
        }
        let digest = hasher.finalize();
        // 16 bytes of a 256-bit digest: collision-safe at any plausible scale,
        // and short enough to live in a URL path without dominating it.
        let hex = digest[..16]
            .iter()
            .fold(String::with_capacity(32), |mut hex, byte| {
                let _ = write!(hex, "{byte:02x}");
                hex
            });
        BookingId::new(format!("sms-{hex}"))
    }
}

/// Whether this message has been seen before, inside the window.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Seen {
    /// First sight. Proceed.
    Accepted,
    /// Already handled, within the replay window.
    Duplicate,
}

/// The inbound replay window.
///
/// # What this does and does not buy
///
/// Carriers retry, so the same message arrives twice and must not act twice.
/// This window is how that is *cheap*.
///
/// It is not how it is *correct*. The window is in memory, so a restart inside
/// it admits a redelivery — and correctness under that comes from the boundary
/// instead: a create carries an id derived from the message, so the second one
/// collides; a cancel against an already-`Cancelled` booking is `Undefined`.
/// Recorded as a limitation rather than dressed as a guarantee, because the two
/// are easy to confuse and only one of them survives a crash.
#[derive(Debug)]
pub struct ReplayWindow {
    // Keyed on the identity ITSELF, not a stringified join of its fields — a
    // joined key is not injective when fields can contain the delimiter, and a
    // collision here rejects a legitimate message as a duplicate.
    seen: Mutex<HashMap<InboundIdentity, i64>>,
    window_ms: i64,
}

impl ReplayWindow {
    #[must_use]
    pub fn new(window_ms: i64) -> Self {
        Self {
            seen: Mutex::new(HashMap::new()),
            window_ms,
        }
    }

    /// Record this identity if it is not already present, atomically.
    ///
    /// # Why one call and not two
    ///
    /// A `contains` followed by an `insert` is two operations with a gap, and two
    /// carrier retries arriving together fit in the gap: both look unseen, both
    /// proceed, and the booking happens twice. The check and the write are one
    /// call so that interleaving is not expressible — the same move as
    /// `load_visible` in the store, where removing the second step removed the
    /// bug rather than guarding it.
    pub fn insert_if_absent(&self, identity: &InboundIdentity, now_ms: i64) -> Seen {
        let mut seen = self
            .seen
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        // Saturating: a clock that moved backwards must not turn expiry into an
        // overflow panic inside the one lock everything shares.
        seen.retain(|_, at| now_ms.saturating_sub(*at) < self.window_ms);
        match seen.entry(identity.clone()) {
            std::collections::hash_map::Entry::Occupied(_) => Seen::Duplicate,
            std::collections::hash_map::Entry::Vacant(slot) => {
                slot.insert(now_ms);
                Seen::Accepted
            }
        }
    }
}
