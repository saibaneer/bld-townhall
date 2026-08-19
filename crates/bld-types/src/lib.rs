#![forbid(unsafe_code)]

use serde::{Deserialize, Serialize};
use std::fmt;

macro_rules! id_type {
    ($name:ident) => {
        #[derive(Clone, Debug, PartialEq, Eq, Hash, Serialize, Deserialize)]
        #[serde(transparent)]
        pub struct $name(String);

        impl $name {
            #[must_use]
            pub fn new(value: impl Into<String>) -> Self {
                Self(value.into())
            }

            #[must_use]
            pub fn as_str(&self) -> &str {
                &self.0
            }
        }

        impl fmt::Display for $name {
            fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
                self.0.fmt(f)
            }
        }
    };
}

id_type!(BookingId);
id_type!(VenueId);
id_type!(SlotId);
id_type!(PrincipalId);
id_type!(ActorId);
id_type!(EffectIntentId);
id_type!(CouncilBookingRef);

#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, Serialize, Deserialize)]
pub struct Money {
    pence: u64,
}

impl Money {
    #[must_use]
    pub const fn from_pence(pence: u64) -> Self {
        Self { pence }
    }

    #[must_use]
    pub const fn pence(self) -> u64 {
        self.pence
    }
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct TimeWindow {
    pub from: String,
    pub to: String,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct BookingRequirements {
    pub purpose: String,
    pub requested_date: String,
    pub time_window: TimeWindow,
    pub attendees: u16,
    pub wheelchair_accessible: bool,
    pub max_fee: Money,
}

/// The most bytes a [`BoundedString`] will hold.
///
/// Bytes rather than characters, because bytes are what storage and log volume
/// actually cost.
pub const MAX_BOUNDED_STRING_BYTES: usize = 512;

/// Provider-supplied text with a hard length ceiling.
///
/// Exists for one reason: a rejection reason arrives from outside the boundary,
/// and gets persisted and logged. An unbounded external string is a way to fill
/// a disk.
///
/// # Truncating, not refusing
///
/// [`Self::truncating`] says so in its name, because silent loss deserves to be
/// visible at the call site. Losing the tail of a rejection reason is better than
/// losing the whole rejection — a refusal the operator cannot read at all is
/// worse than one that reads a little short.
///
/// # No `Serialize`, no `Deserialize`
///
/// Every other type in this crate has them; this one deliberately does not. It
/// lives inside `VerifiedProviderFact`, which must have neither — deserialising
/// verified evidence is the forgery that type exists to prevent (ADR-012). A
/// validating deserializer here would be a bypass with no caller. [`Self::as_str`]
/// is available if a later slice needs to persist one deliberately.
#[derive(Clone, Debug, PartialEq, Eq, Hash)]
pub struct BoundedString(String);

impl BoundedString {
    /// Take at most [`MAX_BOUNDED_STRING_BYTES`], cutting on a character
    /// boundary so the result is always valid UTF-8.
    #[must_use]
    pub fn truncating(value: impl Into<String>) -> Self {
        let mut text: String = value.into();
        if text.len() > MAX_BOUNDED_STRING_BYTES {
            // Walking back from the limit rather than counting forward: a
            // multi-byte character straddling the boundary must be dropped
            // whole. `is_char_boundary(0)` is always true, so this terminates.
            let mut cut = MAX_BOUNDED_STRING_BYTES;
            while !text.is_char_boundary(cut) {
                cut -= 1;
            }
            text.truncate(cut);
        }
        Self(text)
    }

    #[must_use]
    pub fn as_str(&self) -> &str {
        &self.0
    }

    #[must_use]
    pub fn len(&self) -> usize {
        self.0.len()
    }

    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.0.is_empty()
    }
}

impl fmt::Display for BoundedString {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        self.0.fmt(f)
    }
}

#[cfg(test)]
mod bounded_string {
    use super::{BoundedString, MAX_BOUNDED_STRING_BYTES};

    #[test]
    fn an_empty_reason_is_allowed() {
        assert_eq!(BoundedString::truncating("").as_str(), "");
    }

    #[test]
    fn a_reason_exactly_at_the_limit_is_kept_whole() {
        let exact = "x".repeat(MAX_BOUNDED_STRING_BYTES);
        let bounded = BoundedString::truncating(exact.clone());
        assert_eq!(bounded.as_str(), exact);
        assert_eq!(bounded.len(), MAX_BOUNDED_STRING_BYTES);
    }

    #[test]
    fn a_reason_over_the_limit_is_cut_to_it() {
        let bounded = BoundedString::truncating("x".repeat(MAX_BOUNDED_STRING_BYTES + 100));
        assert_eq!(bounded.len(), MAX_BOUNDED_STRING_BYTES);
    }

    /// The cut must land on a character boundary. A naive `truncate` at the byte
    /// limit would panic here rather than silently corrupting, but either way the
    /// council's reason must survive as valid text.
    #[test]
    fn a_multibyte_character_straddling_the_limit_is_dropped_whole() {
        // 'é' is two bytes, so 256 of them fill the limit exactly; adding one
        // more character means the limit falls mid-character.
        let padding = "é".repeat(MAX_BOUNDED_STRING_BYTES / 2);
        let bounded = BoundedString::truncating(format!("{padding}é"));

        assert_eq!(
            bounded.len(),
            MAX_BOUNDED_STRING_BYTES,
            "the straddling character must be dropped, not split"
        );
        assert_eq!(bounded.as_str(), padding);
        assert!(bounded.as_str().chars().all(|c| c == 'é'));
    }

    /// One byte short of the limit, with a two-byte character next. The cut has
    /// to walk back one byte, which is the case an off-by-one gets wrong.
    #[test]
    fn a_cut_one_byte_inside_a_character_walks_back() {
        let padding = "a".repeat(MAX_BOUNDED_STRING_BYTES - 1);
        let bounded = BoundedString::truncating(format!("{padding}é"));
        assert_eq!(bounded.as_str(), padding);
        assert_eq!(bounded.len(), MAX_BOUNDED_STRING_BYTES - 1);
    }
}

/// Which of the three provenance classes drove a transition (ADR-012).
///
/// Vocabulary-free on purpose: this crate must not know what a booking is, and
/// the audit trail only ever stores the class plus a name.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub enum Provenance {
    /// Someone wanted it.
    Proposal,
    /// Externally verified reality said so.
    Fact,
    /// The runtime concluded it about itself.
    SystemEvent,
}

impl Provenance {
    #[must_use]
    pub const fn name(self) -> &'static str {
        match self {
            Self::Proposal => "Proposal",
            Self::Fact => "Fact",
            Self::SystemEvent => "SystemEvent",
        }
    }

    /// Parse the persisted discriminator.
    ///
    /// # Errors
    /// Returns the unrecognised text if it is not a known provenance class.
    pub fn parse(text: &str) -> Result<Self, String> {
        match text {
            "Proposal" => Ok(Self::Proposal),
            "Fact" => Ok(Self::Fact),
            "SystemEvent" => Ok(Self::SystemEvent),
            other => Err(other.to_owned()),
        }
    }
}

/// Something that can drive a transition, and can say which door it came
/// through.
///
/// # Why a trait rather than an enum the caller fills in
///
/// The audit trail's whole job is to record *which provenance class* caused a
/// transition. An enum with public variants lets a caller label a proposal-driven
/// commit as fact-driven — which is the asserted-not-derived defect ADR-017
/// exists to remove, just relocated. Here the **type** answers, so a
/// `BookingProposal` cannot claim to be a verified fact: there is no argument
/// through which to lie.
///
/// Implemented by the domain for each of its three input vocabularies. This
/// crate stays vocabulary-free; the domain owns the types and therefore the
/// impls, so the orphan rule is satisfied without any crate depending on
/// something it should not.
pub trait TransitionDriver {
    /// Which door. Fixed by the implementing type, never by an argument.
    fn provenance(&self) -> Provenance;
    /// Which member of that vocabulary, for the audit row's detail column.
    fn driver_name(&self) -> &'static str;
}
