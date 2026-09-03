//! The delegation's durable form — written and read by the issuer, never by the
//! store.
//!
//! # Why this exists at all
//!
//! ADR-017 point 4, as amended by ADR-021, forbids `VerifiedAuthority` from
//! implementing `Serialize` or `Deserialize`: an envelope that can arrive as
//! JSON can be minted by anything that can write JSON. But §9's `delegations`
//! table has to hold the envelope, or expiry and revocation have nothing to
//! check.
//!
//! ADR-025 recorded the resolution and the reason the first plan was wrong: a
//! second representation appears either way, so the only question is who owns
//! it. If the store's decoder owned it, it would be a mirror of the envelope —
//! free to drift, and the no-serde assertion would keep passing while the
//! mirror became the real minting path. So the codec lives here, beside the
//! issuer, and [`crate::store::DelegationRecord`] carries the bytes opaquely.
//!
//! The round trip is pinned by a test that issues a grant, encodes it, decodes
//! it and compares against the **issued** value — never against a hand-built
//! one, which would assert that the codec agrees with the test's idea of a
//! grant rather than with the issuer's.
//!
//! # Why the envelope carries a MAC (M7B)
//!
//! M7A shipped these bytes unauthenticated, on the argument that the store is
//! trusted infrastructure: whoever can write its rows can write grants, exactly
//! as whoever can write the database can. That argument is true and it is not
//! enough — it makes row-write access equivalent to total authority, so a
//! backup restored from the wrong file, a migration run against the wrong
//! database, or one SQL injection anywhere in the process becomes a grant for
//! anything.
//!
//! The tag closes that. A row whose envelope was not written by an issuer
//! holding the key does not decode, so forging a `delegations` row now needs
//! the key rather than merely a connection. The key lives in the composition
//! root, which is why this could not land before M7B.
//!
//! What the tag does NOT do: protect against the key itself leaking, or against
//! an attacker who can call the issuer. It moves the requirement from "can
//! write a row" to "can sign", which is the whole of its claim.

use crate::assurance::AssuranceLevel;
use crate::codec::{Reader, push_field};
use crate::grant::{AuthorityConstraints, VerifiedAuthority};
use crate::key::EnvelopeKey;
use crate::scope::{BehaviourSet, ScopeHash};
use bld_types::{ActorId, Behaviour, BookingId, DelegationId, Money, PrincipalId, ServiceId};

/// Bumped when the field list changes, so a v1 row can never be read as v2.
const ENVELOPE_VERSION: &[u8] = b"bld.delegation.v1";

/// Write a grant to bytes, with an authentication tag over all of them.
pub(crate) fn encode(authority: &VerifiedAuthority, key: &EnvelopeKey) -> Vec<u8> {
    let mut out = body(authority);
    // The tag covers the ENTIRE body, including the version tag, so a v1 body
    // cannot be replayed under a later scheme with its tag intact.
    let tag = key.tag(&out);
    push_field(&mut out, &tag);
    out
}

fn body(authority: &VerifiedAuthority) -> Vec<u8> {
    let mut out = Vec::new();
    push_field(&mut out, ENVELOPE_VERSION);
    push_field(&mut out, authority.delegation().as_str().as_bytes());
    push_field(&mut out, authority.grantor().as_str().as_bytes());
    push_field(&mut out, authority.subject().as_str().as_bytes());
    push_field(&mut out, authority.actor().as_str().as_bytes());
    push_field(&mut out, authority.service().as_str().as_bytes());

    let behaviours = authority.behaviours().as_slice();
    push_field(&mut out, &(behaviours.len() as u64).to_be_bytes());
    for behaviour in behaviours {
        push_field(&mut out, behaviour.name().as_bytes());
    }

    let constraints = authority.constraints();
    push_field(&mut out, &constraints.max_fee().pence().to_be_bytes());
    push_field(&mut out, &constraints.max_attendees().to_be_bytes());
    let resources = constraints.resources();
    push_field(&mut out, &(resources.len() as u64).to_be_bytes());
    for resource in resources {
        push_field(&mut out, resource.as_str().as_bytes());
    }

    push_field(&mut out, authority.scope_hash().as_bytes());
    push_field(&mut out, &authority.issued_at_ms().to_be_bytes());
    push_field(&mut out, &authority.expires_at_ms().to_be_bytes());
    push_field(&mut out, authority.assurance().name().as_bytes());
    out
}

/// Read a grant back, or refuse.
///
/// Every failure is `None`. A partially-decoded grant is not a grant, and
/// filling a missing field with a default is how a corrupted row becomes a
/// permissive one — the same defect the M5.1 review found in a shim's
/// `unwrap_or(0)`.
pub(crate) fn decode(bytes: &[u8], key: &EnvelopeKey) -> Option<VerifiedAuthority> {
    // The tag is checked FIRST, before a single field is interpreted.
    //
    // Not for speed — because a decoder that parses untrusted bytes and then
    // asks whether to trust them has already done the parsing. Every
    // length-prefix and every enum lookup below runs only on bytes an issuer
    // signed.
    let (signed, tag) = split_tag(bytes)?;
    if !key.verify(signed, tag) {
        return None;
    }
    let mut reader = Reader::new(signed);
    if reader.field()? != ENVELOPE_VERSION {
        return None;
    }
    let delegation = DelegationId::new(reader.text()?);
    let grantor = PrincipalId::new(reader.text()?);
    let subject = PrincipalId::new(reader.text()?);
    let actor = ActorId::new(reader.text()?);
    let service = ServiceId::new(reader.text()?);

    let behaviour_count = usize::try_from(reader.u64()?).ok()?;
    // NOT `with_capacity(behaviour_count)`.
    //
    // The count comes off the row being decoded, so it is exactly as
    // trustworthy as the bytes this function exists to distrust. A single
    // edited byte in the length prefix asked for 72 petabytes and aborted the
    // process — found by the edit battery below, which is the whole reason it
    // walks every byte rather than a chosen few. The loop fails on the first
    // absent field instead, at whatever the real length turns out to be.
    let mut behaviours = Vec::new();
    for _ in 0..behaviour_count {
        behaviours.push(behaviour_from(&reader.text()?)?);
    }

    let max_fee = Money::from_pence(reader.u64()?);
    let max_attendees = u16::from_be_bytes(reader.field()?.try_into().ok()?);
    let resource_count = usize::try_from(reader.u64()?).ok()?;
    // Untrusted count, as above.
    let mut resources = Vec::new();
    for _ in 0..resource_count {
        resources.push(BookingId::new(reader.text()?));
    }

    let scope_hash = ScopeHash::from_bytes(reader.bytes32()?);
    let issued_at_ms = reader.u64()?;
    let expires_at_ms = reader.u64()?;
    let assurance = AssuranceLevel::parse(&reader.text()?)?;

    // Trailing bytes mean these are not the bytes this version wrote. Accepting
    // the prefix would be accepting a grant some other encoder produced.
    if !reader.is_finished() {
        return None;
    }

    Some(VerifiedAuthority::restore(
        delegation,
        grantor,
        subject,
        actor,
        service,
        BehaviourSet::new(behaviours),
        AuthorityConstraints::new(max_fee, max_attendees, resources),
        scope_hash,
        issued_at_ms,
        expires_at_ms,
        assurance,
    ))
}

/// Split the signed body from its trailing tag.
///
/// The tag is the last length-prefixed field, so this walks the fields to find
/// where the body ends rather than assuming a fixed offset — an assumption that
/// would break silently the first time a field was added to the body.
fn split_tag(bytes: &[u8]) -> Option<(&[u8], &[u8])> {
    let mut reader = Reader::new(bytes);
    let mut body_end = 0;
    let mut last = None;
    loop {
        let before = reader.consumed();
        let Some(field) = reader.field() else {
            break;
        };
        body_end = before;
        last = Some(field);
    }
    // A buffer that does not end on a field boundary is malformed, whatever its
    // last field looked like.
    if !reader.is_finished() {
        return None;
    }
    Some((bytes.get(..body_end)?, last?))
}

/// The behaviour names' only reader.
///
/// A closed match rather than a permissive fallback: an envelope naming a
/// behaviour this build does not know must fail to decode, not decode into
/// something adjacent.
fn behaviour_from(name: &str) -> Option<Behaviour> {
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

#[cfg(test)]
mod tests {
    use super::*;
    use crate::grant::VerifiedApproval;
    use crate::key::EnvelopeKey;
    use crate::scope::CanonicalScope;
    use bld_types::{BookingRequirements, TimeWindow};

    const NOW: u64 = 1_700_000_000_000;

    fn key() -> EnvelopeKey {
        EnvelopeKey::new(vec![7u8; 32]).expect("long enough")
    }

    fn other_key() -> EnvelopeKey {
        EnvelopeKey::new(vec![9u8; 32]).expect("long enough")
    }

    /// An in-crate test, because building a grant requires the private
    /// constructor these bytes are the durable form of. The privacy that stops
    /// other crates minting is exactly what puts this test here.
    fn issued() -> VerifiedAuthority {
        let scope = CanonicalScope {
            service: ServiceId::new("demo-council-town-hall"),
            agent: "TownHallAgent".to_owned(),
            booking: BookingId::new("sms-lucy-0001"),
            behaviours: BehaviourSet::new([Behaviour::Book, Behaviour::Cancel]),
            requirements: BookingRequirements {
                purpose: "town hall booking".to_owned(),
                requested_date: "2026-09-10".to_owned(),
                time_window: TimeWindow {
                    from: "14:00".to_owned(),
                    to: "17:00".to_owned(),
                },
                attendees: 20,
                wheelchair_accessible: true,
                max_fee: Money::from_pence(5_000),
            },
            expires_at_ms: NOW + 600_000,
            grant_ttl_ms: 3_600_000,
        };
        let approval = VerifiedApproval::new(
            bld_types::ApprovalChallengeId::new("challenge-1"),
            scope,
            crate::grant::BindingRef {
                principal: PrincipalId::new("lucy"),
                version: 1,
            },
            AssuranceLevel::SmsReply,
            NOW + 1_000,
        );
        VerifiedAuthority::issue(
            DelegationId::new("delegation-1"),
            &approval,
            PrincipalId::new("lucy"),
            PrincipalId::new("marco"),
            ActorId::new("agent:marco"),
            AssuranceLevel::SmsReply,
        )
    }

    #[test]
    fn the_round_trip_returns_the_grant_that_was_encoded() {
        let grant = issued();
        assert_eq!(decode(&encode(&grant, &key()), &key()), Some(grant));
    }

    /// Every single-bit edit must be refused, or read back as something else.
    ///
    /// # What this is really checking
    ///
    /// The envelope is not signed — it is protected by being inside the
    /// database, which is the POC's boundary. So the honest claim is narrower
    /// than "tamper-proof": an edit either fails to decode, or decodes into a
    /// grant that is visibly not the one issued. What must never happen is a
    /// decode that succeeds and returns the ORIGINAL grant, which would mean
    /// the edited field was ignored — a field nobody reads is a constraint
    /// nobody enforces.
    #[test]
    fn no_edited_byte_decodes_back_to_the_original_grant() {
        let grant = issued();
        let bytes = encode(&grant, &key());

        for index in 0..bytes.len() {
            for bit in [0x01u8, 0x80u8] {
                let mut edited = bytes.clone();
                edited[index] ^= bit;
                if let Some(decoded) = decode(&edited, &key()) {
                    assert_ne!(
                        decoded, grant,
                        "byte {index} bit {bit:#x} was edited and the decode \
                         still produced the original grant — that field is not \
                         being read"
                    );
                }
            }
        }
    }

    #[test]
    fn a_truncated_envelope_is_refused() {
        let bytes = encode(&issued(), &key());
        for cut in 0..bytes.len() {
            assert!(
                decode(&bytes[..cut], &key()).is_none(),
                "a {cut}-byte prefix decoded as a grant"
            );
        }
    }

    /// Trailing bytes mean these are not the bytes this version wrote.
    #[test]
    fn trailing_bytes_are_refused_rather_than_ignored() {
        let mut bytes = encode(&issued(), &key());
        bytes.push(0);
        assert!(decode(&bytes, &key()).is_none());
    }

    /// A v1 row must not be readable as some later scheme, or vice versa.
    #[test]
    fn a_foreign_version_tag_is_refused() {
        let grant = issued();
        let mut out = Vec::new();
        push_field(&mut out, b"bld.delegation.v2");
        out.extend_from_slice(&encode(&grant, &key())[ENVELOPE_VERSION.len() + 8..]);
        assert!(decode(&out, &key()).is_none());
    }

    /// An envelope written by someone without the key does not decode.
    ///
    /// # What this is really asserting
    ///
    /// That row-write access is no longer equivalent to total authority. The
    /// body here is a PERFECTLY WELL-FORMED grant — every field in the right
    /// order, every length prefix correct, a real behaviour set, a plausible
    /// expiry. It is exactly what someone with a database connection and this
    /// crate's source would write. Without the key it is not a grant.
    #[test]
    fn a_well_formed_envelope_without_the_key_is_not_a_grant() {
        let grant = issued();
        let unsigned = body(&grant);

        // No tag at all.
        assert!(decode(&unsigned, &key()).is_none());

        // A tag of the right shape, from the wrong key.
        let forged = encode(&grant, &other_key());
        assert!(
            decode(&forged, &key()).is_none(),
            "an envelope signed with another key must not resolve"
        );

        // And the same bytes DO decode under the key that wrote them, so the
        // refusals above are about the key and not about the body.
        assert_eq!(decode(&forged, &other_key()), Some(grant));
    }

    /// A tag lifted from one grant does not authenticate another.
    ///
    /// The tag covers the whole body, so this is arithmetic rather than
    /// cleverness — but it is the property the split-and-verify code could
    /// plausibly get wrong by signing only a prefix.
    #[test]
    fn a_tag_cannot_be_transplanted_between_grants() {
        let mine = issued();
        let mut theirs = issued();
        theirs = VerifiedAuthority::restore(
            DelegationId::new("delegation-2"),
            PrincipalId::new("mallory"),
            PrincipalId::new("mallory"),
            ActorId::new("agent:mallory"),
            theirs.service().clone(),
            theirs.behaviours().clone(),
            theirs.constraints().clone(),
            theirs.scope_hash(),
            theirs.issued_at_ms(),
            theirs.expires_at_ms(),
            theirs.assurance(),
        );

        let signed_mine = encode(&mine, &key());
        let (_, my_tag) = split_tag(&signed_mine).expect("well formed");

        let mut transplanted = body(&theirs);
        push_field(&mut transplanted, my_tag);

        assert!(
            decode(&transplanted, &key()).is_none(),
            "a tag over one grant must not authenticate a different one"
        );
    }

    /// A behaviour this build does not know must fail the decode, not decode
    /// into something adjacent.
    #[test]
    fn an_unknown_behaviour_name_is_refused() {
        assert_eq!(behaviour_from("Book"), Some(Behaviour::Book));
        assert_eq!(behaviour_from("Bok"), None);
        assert_eq!(behaviour_from(""), None);
        assert_eq!(behaviour_from("book"), None, "the spelling is exact");
    }
}
