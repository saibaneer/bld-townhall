//! What a person is asked to approve — as data, not as a hash and not as text.
//!
//! # Why the scope is stored, and not only its digest
//!
//! A hash proves equality and reconstructs nothing. If the challenge held only
//! a digest, resuming after the approval would mean holding the request in
//! session memory or re-parsing the original SMS — and spec §2's "durable state
//! is not conversational memory" forbids both. So the canonical scope is a
//! durable object; the digest is derived from it, never stored in its place.
//!
//! # Why the preview is a method on it
//!
//! The property that makes approval mean anything is that the scope a person
//! was SHOWN is the scope that was HASHED. Nothing detects drift between two
//! system-generated strings: both are "correct" outputs of code nobody
//! tampered with, so a "tampered challenge" test passes while the human
//! approved something else entirely (ADR-025).
//!
//! Rendering is therefore a method on the value that hashes itself. There is no
//! way to hold a preview for one scope and a digest for another, because
//! producing either requires the same `self`.

use crate::codec::{Reader, push_field};
use bld_types::{Behaviour, BookingId, BookingRequirements, Money, ServiceId};
use sha2::{Digest as _, Sha256};
use std::fmt;

/// The digest's domain tag. Bumped when the encoding changes, so a scope hashed
/// under v1 can never collide with the same bytes read under a later scheme.
const SCOPE_ENCODING_VERSION: &[u8] = b"bld.scope.v1";

/// The behaviours an approval covers, held in one fixed order.
///
/// # Why not a `HashSet`
///
/// The digest must be stable across runs and processes. A hash-set iteration
/// order is neither, so the same permissions could hash two different ways and
/// a perfectly valid approval would fail its own scope check on a restart —
/// intermittently, which is the worst way for it to fail.
///
/// Sorted and deduplicated at construction, so there is no unordered moment.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct BehaviourSet(Vec<Behaviour>);

impl BehaviourSet {
    /// Build from anything iterable, in any order.
    #[must_use]
    pub fn new(behaviours: impl IntoIterator<Item = Behaviour>) -> Self {
        let mut held: Vec<Behaviour> = behaviours.into_iter().collect();
        held.sort_unstable();
        held.dedup();
        Self(held)
    }

    #[must_use]
    pub fn permits(&self, behaviour: Behaviour) -> bool {
        self.0.contains(&behaviour)
    }

    #[must_use]
    pub fn as_slice(&self) -> &[Behaviour] {
        &self.0
    }

    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.0.is_empty()
    }
}

/// A canonical scope's digest.
#[derive(Clone, Copy, PartialEq, Eq, Hash)]
pub struct ScopeHash([u8; 32]);

impl ScopeHash {
    #[must_use]
    pub fn as_bytes(&self) -> &[u8; 32] {
        &self.0
    }

    /// Rebuild a digest from the bytes an encoder wrote.
    ///
    /// `pub(crate)` — a digest arriving from outside is a claim about a scope
    /// nobody in this process hashed, and the only in-crate caller is the
    /// envelope decoder reading what the envelope encoder wrote.
    pub(crate) fn from_bytes(bytes: [u8; 32]) -> Self {
        Self(bytes)
    }

    /// Read a digest back from its hex spelling, or refuse.
    #[must_use]
    pub fn parse_hex(text: &str) -> Option<Self> {
        if text.len() != 64 {
            return None;
        }
        let mut bytes = [0u8; 32];
        for (index, slot) in bytes.iter_mut().enumerate() {
            *slot = u8::from_str_radix(text.get(2 * index..2 * index + 2)?, 16).ok()?;
        }
        Some(Self(bytes))
    }
}

impl fmt::Display for ScopeHash {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        for byte in self.0 {
            write!(f, "{byte:02x}")?;
        }
        Ok(())
    }
}

/// `Debug` is the `Display` hex, not a byte array: a digest printed as 32
/// separate numbers is unreadable in exactly the log line where it matters.
impl fmt::Debug for ScopeHash {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "ScopeHash({self})")
    }
}

/// Everything an approval covers.
///
/// # Why the booking id is here, before any booking exists
///
/// ADR-024 derives a booking's id from the message that requested it, which was
/// right when the request and the creation were the same turn. Spec §23.1 puts
/// approval FIRST, so the turn that creates the booking is the `YES` reply — a
/// different message, with a different identity, deriving a different id. A
/// booking created from it would be a second booking, and the one the person
/// approved would never exist.
///
/// So the id is minted from the original request and carried here. The same
/// move gives "approve the cancellation of that booking" something to name
/// before a row exists (ADR-025).
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct CanonicalScope {
    pub service: ServiceId,
    /// The agent named in the preview — who the person is permitting to act.
    pub agent: String,
    /// The exact resource. Everything this approval permits, it permits here
    /// and nowhere else.
    pub booking: BookingId,
    pub behaviours: BehaviourSet,
    pub requirements: BookingRequirements,
    /// The deadline for ANSWERING — after this, the offer is gone.
    ///
    /// Not the booking's date, and not how long the permission lasts.
    pub expires_at_ms: u64,
    /// How long the permission lasts once it has been given.
    ///
    /// # Why both deadlines are hashed and shown
    ///
    /// One deadline was the first design, and it was wrong in a way worth
    /// recording: if the reply window and the grant's lifetime are the same
    /// instant, then approving at the last second issues a grant that has
    /// already expired. Splitting them and hashing only the reply deadline was
    /// worse — the person would approve a permission whose duration they were
    /// never told, which is the exact defect the "everything hashed is shown"
    /// rule exists to prevent.
    ///
    /// So both are in the scope, both are hashed, and both appear in the
    /// preview. The grant's clock starts when the approval arrives.
    pub grant_ttl_ms: u64,
}

impl CanonicalScope {
    /// The bytes the digest covers: length-prefixed, in a fixed field order.
    ///
    /// Length-prefixed rather than delimiter-joined — see [`crate::codec`] for
    /// why. A purpose of `"a|b"` with a date of `"c"` and a purpose of `"a"`
    /// with a date of `"b|c"` must not agree, and under a delimiter they would.
    #[must_use]
    pub fn encode(&self) -> Vec<u8> {
        let mut out = Vec::new();
        push_field(&mut out, SCOPE_ENCODING_VERSION);
        push_field(&mut out, self.service.as_str().as_bytes());
        push_field(&mut out, self.agent.as_bytes());
        push_field(&mut out, self.booking.as_str().as_bytes());

        push_field(
            &mut out,
            &(self.behaviours.as_slice().len() as u64).to_be_bytes(),
        );
        for behaviour in self.behaviours.as_slice() {
            push_field(&mut out, behaviour.name().as_bytes());
        }

        let requirements = &self.requirements;
        push_field(&mut out, requirements.purpose.as_bytes());
        push_field(&mut out, requirements.requested_date.as_bytes());
        push_field(&mut out, requirements.time_window.from.as_bytes());
        push_field(&mut out, requirements.time_window.to.as_bytes());
        push_field(&mut out, &requirements.attendees.to_be_bytes());
        push_field(&mut out, &[u8::from(requirements.wheelchair_accessible)]);
        push_field(&mut out, &requirements.max_fee.pence().to_be_bytes());
        push_field(&mut out, &self.expires_at_ms.to_be_bytes());
        push_field(&mut out, &self.grant_ttl_ms.to_be_bytes());
        out
    }

    /// Read a scope back from [`Self::encode`]'s bytes, or refuse.
    ///
    /// # Why this is public where the envelope's decoder is not
    ///
    /// A scope is a description of what was ASKED, not a grant. Reconstructing
    /// one confers nothing — the authority it might lead to still requires a
    /// challenge, a code and a bound channel. So the store may hold the bytes
    /// and read them back, which is what lets an approval resume after a
    /// restart without conversational memory.
    ///
    /// Every failure is `None`. A scope that half-decodes is not the scope
    /// anybody approved.
    #[must_use]
    pub fn decode(bytes: &[u8]) -> Option<Self> {
        let mut reader = Reader::new(bytes);
        if reader.field()? != SCOPE_ENCODING_VERSION {
            return None;
        }
        let service = ServiceId::new(reader.text()?);
        let agent = reader.text()?;
        let booking = BookingId::new(reader.text()?);

        // NOT `with_capacity`: the count comes off the bytes being distrusted.
        // See the same note in `crate::envelope`, where trusting it cost the
        // process a 72-petabyte allocation.
        let count = usize::try_from(reader.u64()?).ok()?;
        let mut behaviours = Vec::new();
        for _ in 0..count {
            behaviours.push(behaviour_named(&reader.text()?)?);
        }

        let requirements = BookingRequirements {
            purpose: reader.text()?,
            requested_date: reader.text()?,
            time_window: bld_types::TimeWindow {
                from: reader.text()?,
                to: reader.text()?,
            },
            attendees: u16::from_be_bytes(reader.field()?.try_into().ok()?),
            wheelchair_accessible: match reader.field()? {
                [0] => false,
                [1] => true,
                _ => return None,
            },
            max_fee: Money::from_pence(reader.u64()?),
        };
        let expires_at_ms = reader.u64()?;
        let grant_ttl_ms = reader.u64()?;

        if !reader.is_finished() {
            return None;
        }
        Some(Self {
            service,
            agent,
            booking,
            behaviours: BehaviourSet::new(behaviours),
            requirements,
            expires_at_ms,
            grant_ttl_ms,
        })
    }

    /// The digest of [`Self::encode`].
    #[must_use]
    pub fn digest(&self) -> ScopeHash {
        let mut hasher = Sha256::new();
        hasher.update(self.encode());
        ScopeHash(hasher.finalize().into())
    }

    /// The permission preview, rendered from this scope (spec §13.2).
    ///
    /// # The rule this rendering follows
    ///
    /// **Everything hashed is shown.** Not "everything the example showed" —
    /// every field [`Self::encode`] covers appears here in words, including
    /// `purpose`, which §13.2's example omits. A field that is hashed but not
    /// shown is a field a person did not approve, and no test downstream can
    /// tell the difference.
    ///
    /// `now_ms` is the only input from outside the scope, and it moves exactly
    /// one number: how long the permission has left.
    #[must_use]
    pub fn preview(&self, code: &str, now_ms: u64) -> String {
        let requirements = &self.requirements;
        let permissions: Vec<&str> = self
            .behaviours
            .as_slice()
            .iter()
            .map(|behaviour| phrase(*behaviour))
            .collect();

        format!(
            "BLD booking request\n\
             Service: {service}\n\
             Agent: {agent}\n\
             May: {permissions}\n\
             Purpose: {purpose}\n\
             Reference: {booking}\n\
             Date: {date}\n\
             Time: {from}-{to}\n\
             Attendees: <= {attendees}\n\
             Wheelchair access: {access}\n\
             Maximum booking fee: {fee}\n\
             {expiry}.\n\
             Permission then lasts {lasts}.\n\
             \n\
             Reply YES {code} to approve.\n\
             Reply NO {code} to reject.",
            service = self.service,
            agent = self.agent,
            permissions = if permissions.is_empty() {
                "nothing".to_owned()
            } else {
                permissions.join("; ")
            },
            purpose = requirements.purpose,
            booking = self.booking,
            date = requirements.requested_date,
            from = requirements.time_window.from,
            to = requirements.time_window.to,
            attendees = requirements.attendees,
            access = if requirements.wheelchair_accessible {
                "required"
            } else {
                "not required"
            },
            fee = pounds(requirements.max_fee),
            expiry = remaining(self.expires_at_ms, now_ms),
            lasts = duration(self.grant_ttl_ms),
        )
    }
}

/// A behaviour in the words of the person being asked, not the state machine's.
const fn phrase(behaviour: Behaviour) -> &'static str {
    match behaviour {
        Behaviour::SelectVenue => "choose the venue",
        Behaviour::VerifySlot => "check the slot is free",
        Behaviour::ChangeVenue => "move it to another venue",
        Behaviour::UpdateRequirements => "change what was asked for",
        Behaviour::RevalidateVenue => "re-check the venue",
        Behaviour::Book => "book one meeting room",
        Behaviour::Cancel => "cancel that booking",
    }
}

fn pounds(money: Money) -> String {
    format!("£{}.{:02}", money.pence() / 100, money.pence() % 100)
}

/// How long the permission has left, in the words an SMS reader needs.
///
/// Relative rather than §13.2's wall-clock ("17:00 Thu 20 Aug"), and recorded as
/// a deliberate deviation: rendering a calendar date means a date library or
/// hand-rolled civil-from-days arithmetic, and "expires in 9 minutes" is both
/// the thing a person acts on and impossible to get wrong by a timezone. The
/// absolute deadline still governs — the verifier reads `expires_at_ms`, never
/// this string.
fn remaining(expires_at_ms: u64, now_ms: u64) -> String {
    let Some(left_ms) = expires_at_ms.checked_sub(now_ms) else {
        // A whole clause, not a fragment: a caller pasting "has expired" into
        // "Reply within {}" would send "Reply within has expired".
        return "This request has expired".to_owned();
    };
    match left_ms / 60_000 {
        0 => "Reply within the next minute".to_owned(),
        1 => "Reply within 1 minute".to_owned(),
        many => format!("Reply within {many} minutes"),
    }
}

/// How long a permission lasts, in words.
fn duration(ttl_ms: u64) -> String {
    match ttl_ms / 60_000 {
        0 => "under a minute".to_owned(),
        1 => "1 minute".to_owned(),
        many => format!("{many} minutes"),
    }
}

/// The behaviour names' only reader on this side of the crate.
///
/// A closed lookup rather than a permissive fallback: a scope naming a
/// behaviour this build does not know must fail to decode, not decode into
/// something adjacent.
fn behaviour_named(name: &str) -> Option<Behaviour> {
    [
        Behaviour::SelectVenue,
        Behaviour::VerifySlot,
        Behaviour::ChangeVenue,
        Behaviour::UpdateRequirements,
        Behaviour::RevalidateVenue,
        Behaviour::Book,
        Behaviour::Cancel,
    ]
    .into_iter()
    .find(|behaviour| behaviour.name() == name)
}
