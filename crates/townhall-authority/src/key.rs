//! The key the delegation envelope is authenticated with.
//!
//! # What this is for
//!
//! M7A shipped envelopes unauthenticated, and recorded the honest consequence:
//! an `ApprovalStore` implementor is trusted infrastructure, because whoever can
//! write its rows can write grants. True, and not enough — it makes row-write
//! access equivalent to total authority. The tag moves the requirement from
//! "can write a row" to "can sign".

use hmac::{Hmac, KeyInit as _, Mac as _};
use sha2::Sha256;

/// The tag's length in bytes. Full SHA-256 output: there is no bandwidth
/// pressure here worth a truncation argument.
pub const TAG_BYTES: usize = 32;

/// The minimum key length this will accept.
///
/// Thirty-two bytes, and refused rather than stretched. A short key silently
/// padded is a weak key that looks like a strong one, and this type exists so
/// that "the envelope is authenticated" means something.
pub const MIN_KEY_BYTES: usize = 32;

/// A symmetric key for authenticating delegation envelopes.
///
/// # Why `Debug` shows nothing
///
/// ADR-023's rule, applied where it matters most: a key in a log line is the
/// key. Unlike the approval code there is not even an argument about entropy to
/// have — this is masked completely, and the type deliberately offers no
/// accessor that would let it be printed by accident.
#[derive(Clone)]
pub struct EnvelopeKey(Vec<u8>);

impl EnvelopeKey {
    /// Take a key, or refuse it.
    ///
    /// # Errors
    /// The material is shorter than [`MIN_KEY_BYTES`].
    pub fn new(material: impl Into<Vec<u8>>) -> Result<Self, KeyTooShort> {
        let material = material.into();
        if material.len() < MIN_KEY_BYTES {
            return Err(KeyTooShort {
                offered: material.len(),
            });
        }
        Ok(Self(material))
    }

    /// The tag over `bytes`.
    #[must_use]
    pub(crate) fn tag(&self, bytes: &[u8]) -> [u8; TAG_BYTES] {
        let mut mac = Hmac::<Sha256>::new_from_slice(&self.0)
            .expect("HMAC accepts a key of any length, and this one is bounded below");
        mac.update(bytes);
        mac.finalize().into_bytes().into()
    }

    /// Whether `tag` is this key's tag over `bytes`.
    ///
    /// Uses the MAC's own `verify_slice`, which compares in constant time. Not
    /// because a timing oracle on a tag is the likeliest attack here, but
    /// because the alternative is writing `==` over a secret-dependent value
    /// and inviting the next person to copy it somewhere it does matter.
    #[must_use]
    pub(crate) fn verify(&self, bytes: &[u8], tag: &[u8]) -> bool {
        if tag.len() != TAG_BYTES {
            return false;
        }
        let mut mac = Hmac::<Sha256>::new_from_slice(&self.0)
            .expect("HMAC accepts a key of any length, and this one is bounded below");
        mac.update(bytes);
        mac.verify_slice(tag).is_ok()
    }
}

impl std::fmt::Debug for EnvelopeKey {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str("EnvelopeKey(****)")
    }
}

#[derive(Debug, thiserror::Error, PartialEq, Eq)]
#[error("an envelope key must be at least {MIN_KEY_BYTES} bytes; {offered} were offered")]
pub struct KeyTooShort {
    pub offered: usize,
}
