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
// M7's identifiers (spec §9.1). Ids only — minting one of these grants nothing,
// which is why they live in the shared vocabulary while the envelope they name
// lives in `townhall-authority` behind a private constructor (ADR-025).
id_type!(ApprovalChallengeId);
id_type!(DelegationId);
id_type!(ServiceId);
// M7C-1's receipt for a deposited inbound-evidence row (ADR-026). An id names a
// row; it grants nothing. The untrusted proposer forwards this opaque handle in
// place of the transport evidence itself, and the verifier reads the row back.
id_type!(EvidenceReceiptId);
// M8's usage-metering ids (spec §16.1, ADR-027). Usage units are £0 and bound
// resource consumption only — they grant NO authority — so, like every id here,
// minting one permits nothing. `UsageIntentId` is the retry-stable meter key:
// derived from the inbound message's transport identity (as `BookingId` is), so
// a redelivery — even across a restart — names the same intent and meters once.
id_type!(UsageAccountId);
id_type!(UsageIntentId);

/// One thing an authority may permit, named independently of the domain.
///
/// # Why this is not `BookingProposal::name()`
///
/// ADR-025 puts `townhall-authority` BELOW `townhall-domain` in the graph, so
/// the issuer cannot name a proposal. The alternative to a shared enum was a
/// second list of behaviour names inside the authority crate — two vocabularies
/// for one set of permissions, and the drift between them would be silent until
/// a grant permitted a behaviour the domain had renamed.
///
/// So the names live here once, and the domain maps its proposal onto this.
/// A closed enum rather than a string: an authority carrying `"Bok"` should not
/// be constructible, let alone storable.
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
pub enum Behaviour {
    SelectVenue,
    VerifySlot,
    ChangeVenue,
    UpdateRequirements,
    RevalidateVenue,
    Book,
    Cancel,
}

impl Behaviour {
    /// The wire and audit name, matching `BookingProposal::name()`'s spelling.
    ///
    /// Pinned by a test in `townhall-domain`, where both types are visible: the
    /// two lists agreeing is the whole reason this enum exists rather than a
    /// private copy.
    #[must_use]
    pub const fn name(self) -> &'static str {
        match self {
            Self::SelectVenue => "SelectVenue",
            Self::VerifySlot => "VerifySlot",
            Self::ChangeVenue => "ChangeVenue",
            Self::UpdateRequirements => "UpdateRequirements",
            Self::RevalidateVenue => "RevalidateVenue",
            Self::Book => "Book",
            Self::Cancel => "Cancel",
        }
    }

    /// The kebab-case URL segment this behaviour is driven by, e.g.
    /// `/booking-intents/{id}/behaviours/select-venue`.
    ///
    /// This is the ONE home for the wire segment (M9/ADR-029). The name the
    /// projection publishes (`Self::name`, `PascalCase`) is NOT the segment the
    /// route matches (kebab), and a generic client cannot derive one from the
    /// other — so the manifest publishes this mapping and the router matches on
    /// it, both reading HERE rather than each spelling kebab in its own literals.
    #[must_use]
    pub const fn segment(self) -> &'static str {
        match self {
            Self::SelectVenue => "select-venue",
            Self::VerifySlot => "verify-slot",
            Self::ChangeVenue => "change-venue",
            Self::UpdateRequirements => "update-requirements",
            Self::RevalidateVenue => "revalidate-venue",
            Self::Book => "book",
            Self::Cancel => "cancel",
        }
    }

    /// Resolve a wire segment back to its behaviour — the router's parse step and
    /// the manifest generator share this, so the segment spelling has one source.
    #[must_use]
    pub fn from_segment(segment: &str) -> Option<Self> {
        Self::ALL.into_iter().find(|b| b.segment() == segment)
    }

    /// Every behaviour, in a stable order — for the manifest's behaviour table and
    /// for `from_segment`'s search.
    pub const ALL: [Self; 7] = [
        Self::SelectVenue,
        Self::VerifySlot,
        Self::ChangeVenue,
        Self::UpdateRequirements,
        Self::RevalidateVenue,
        Self::Book,
        Self::Cancel,
    ];
}

impl fmt::Display for Behaviour {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(self.name())
    }
}

/// One attempt at one external effect: its identity, and the deadline the
/// provider must bind.
///
/// # Why the deadline travels with the identity
///
/// The provider records `expires_at_ms` on first sight of an identity and treats
/// it as immutable (ADR-016 §1). So the value it binds must be the one the
/// *durable intent* holds — and the only code that can know that is whatever
/// loaded the intent.
///
/// Before this type, [`crate::EffectIntentId`] travelled alone and a capability
/// adapter had to obtain the deadline some other way. Every available way was
/// wrong: recomputing it binds a value the intent does not hold, so every later
/// reconciliation lookup sends the persisted one and is refused as a conflict —
/// permanently, because neither value ever changes again. Caching it beside the
/// call is the same defect with a race in front of it.
///
/// Pairing them makes the correct thing the only representable thing: an adapter
/// receives the deadline it must send, and never has an opportunity to source
/// one.
///
/// A struct rather than a second parameter because the resolve path needs the
/// same pair, and because the next thing this protocol needs would otherwise be
/// a fourth positional argument.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct EffectAttempt {
    pub id: EffectIntentId,
    /// Wall-clock milliseconds. Read from the persisted intent, never derived.
    pub expires_at_ms: i64,
}

/// A provider's warrant for one observation it made, opaque to the holder.
///
/// The provider issues this alongside facts it owns, signs it over whatever it
/// needs to re-check later, and we hand it back when acting on those facts. We
/// never parse it, compare it, or evaluate any deadline inside it — there is no
/// accessor here that would let us, and that is the point.
///
/// # Why opacity is the mechanism, not modesty
///
/// The alternative is for the holder to check freshness itself, which fails in
/// both directions: a holder clock running fast refuses live facts, and one
/// running slow accepts dead ones. Only the issuer can compare its own deadline
/// against its own clock and its own current state. So the holder's job is
/// reduced to *carrying* — and a type with no readable interior cannot acquire a
/// larger job by accident.
///
/// Serde is present, unlike [`BoundedString`], because this must survive in a
/// persisted canonical plan: it is issued during one turn and spent during a
/// later one, and the durable plan is the only honest place for it to wait.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(transparent)]
pub struct AvailabilityGrant(String);

impl AvailabilityGrant {
    #[must_use]
    pub fn new(token: impl Into<String>) -> Self {
        Self(token.into())
    }

    /// The token, for putting on the wire and nothing else.
    ///
    /// Deliberately not named `as_str`: this is not text to read, and the name
    /// is the only thing stopping a caller from treating it as such.
    #[must_use]
    pub fn on_the_wire(&self) -> &str {
        &self.0
    }
}

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
