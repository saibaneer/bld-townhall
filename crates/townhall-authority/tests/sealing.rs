//! What an outside crate cannot do to an authority.
//!
//! This file is deliberately an INTEGRATION test — a separate crate — because
//! the property under test is "what someone else can reach". A `#[cfg(test)]`
//! module inside the library would be inside the privacy boundary it is meant
//! to check, and would pass while proving nothing.
//!
//! # What is witnessed here, and what is witnessed by the compiler
//!
//! Witnessed here: the envelope cannot arrive as JSON, and cannot be handed to
//! a generic sink that would serialize it.
//!
//! Witnessed by the compiler: there is no public constructor, so this file
//! *cannot* build a `VerifiedAuthority` or a `VerifiedApproval` at all — the
//! attempt does not compile, which is a stronger guarantee than any assertion
//! and an unrunnable one. A `trybuild` lane that pins the compile error itself
//! is deferred to M7B and named in the acceptance record, so the absence is on
//! the books rather than assumed.

use static_assertions::assert_not_impl_any;
use townhall_authority::{AssuranceLevel, VerifiedApproval, VerifiedAuthority};

/// ADR-017 point 4's surviving half, extended to M7's two new evidence types.
///
/// `Serialize` matters as much as `Deserialize`: a type that can be serialized
/// can be handed to a generic sink, and the round trip through any format that
/// also deserializes is a minting path. ADR-021 recorded this for the verdict
/// and the provider fact; the approval evidence and the grant join them.
#[test]
fn neither_the_approval_nor_the_grant_can_cross_a_wire() {
    assert_not_impl_any!(VerifiedAuthority: serde::Serialize, serde::de::DeserializeOwned);
    assert_not_impl_any!(VerifiedApproval: serde::Serialize, serde::de::DeserializeOwned);
}

/// The assurance ordering is a real ordering, and the dev lane sits at the
/// bottom of it.
///
/// # Why this is asserted rather than assumed
///
/// ADR-025 pins dev grants to the lowest level because widening the envelope
/// forced the dev resolver to fabricate an assurance value like every other
/// field — and the value a careless implementation reaches for is the strongest
/// one, which makes a dev token a forged envelope with a straight face. If
/// someone reorders these variants, `Dev` silently becomes the stronger of the
/// two and every `meets` check inverts.
#[test]
fn dev_assurance_never_clears_an_sms_minimum() {
    assert!(AssuranceLevel::Dev < AssuranceLevel::SmsReply);
    assert!(!AssuranceLevel::Dev.meets(AssuranceLevel::SmsReply));
    assert!(AssuranceLevel::SmsReply.meets(AssuranceLevel::Dev));
    assert!(AssuranceLevel::SmsReply.meets(AssuranceLevel::SmsReply));
}

/// A level must survive a restart as the level it was issued at.
#[test]
fn every_assurance_level_round_trips_through_its_durable_name() {
    for level in [AssuranceLevel::Dev, AssuranceLevel::SmsReply] {
        assert_eq!(
            AssuranceLevel::parse(level.name()),
            Some(level),
            "{} does not read back as itself",
            level.name()
        );
    }
    assert_eq!(
        AssuranceLevel::parse("strong"),
        None,
        "an unknown level must refuse rather than default — defaulting weak \
         un-authorizes a real grant, defaulting strong promotes a corrupt row"
    );
}
