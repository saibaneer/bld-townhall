//! One definition of "the same message", used everywhere it matters.

use std::collections::HashMap;
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

    /// A stable, opaque key — the value a derived booking id is built from.
    #[must_use]
    pub fn key(&self) -> String {
        format!(
            "{}\u{1f}{}\u{1f}{}",
            self.provider, self.provider_account, self.provider_message_id
        )
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
    seen: Mutex<HashMap<String, i64>>,
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
        seen.retain(|_, at| now_ms - *at < self.window_ms);
        match seen.entry(identity.key()) {
            std::collections::hash_map::Entry::Occupied(_) => Seen::Duplicate,
            std::collections::hash_map::Entry::Vacant(slot) => {
                slot.insert(now_ms);
                Seen::Accepted
            }
        }
    }
}
