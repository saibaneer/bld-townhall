//! The canonical scope's two properties: everything hashed is shown, and the
//! encoding says one thing.
//!
//! These are M7A's load-bearing tests. The acceptance gate's word "tampered"
//! only means something if the scope a person was shown is provably the scope
//! that was hashed — and no downstream test can catch drift between two
//! system-generated strings, because both are honest outputs of untampered code
//! (ADR-025).

use bld_types::{Behaviour, BookingId, BookingRequirements, Money, ServiceId, TimeWindow};
use townhall_authority::{BehaviourSet, CanonicalScope, ScopeHash};

const NOW: u64 = 1_700_000_000_000;
const CODE: &str = "7312";

fn scope() -> CanonicalScope {
    CanonicalScope {
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
    }
}

/// Every field the digest covers must appear in the preview, and vice versa.
///
/// # Why it is written as a mutation battery rather than a golden string
///
/// A golden-string assertion pins today's wording and proves nothing about
/// coverage: add a field to `encode` and forget it in `preview`, and the golden
/// string still matches. Here, each case changes exactly one field and requires
/// BOTH outputs to move. A field hashed but not shown fails the preview half; a
/// field shown but not hashed fails the digest half.
///
/// This is the test that would have caught the defect ADR-025 named: a scope
/// hashed with a fee ceiling the person was never told.
#[test]
fn everything_hashed_is_shown_and_everything_shown_is_hashed() {
    let base = scope();
    let base_digest = base.digest();
    let base_preview = base.preview(CODE, NOW);

    let mutations: Vec<(&str, CanonicalScope)> = vec![
        ("service", {
            let mut it = scope();
            it.service = ServiceId::new("some-other-council");
            it
        }),
        ("agent", {
            let mut it = scope();
            it.agent = "SomeOtherAgent".to_owned();
            it
        }),
        ("booking", {
            let mut it = scope();
            it.booking = BookingId::new("sms-lucy-0002");
            it
        }),
        ("behaviours", {
            let mut it = scope();
            it.behaviours = BehaviourSet::new([Behaviour::Book]);
            it
        }),
        ("purpose", {
            let mut it = scope();
            it.requirements.purpose = "wedding reception".to_owned();
            it
        }),
        ("requested_date", {
            let mut it = scope();
            it.requirements.requested_date = "2026-09-17".to_owned();
            it
        }),
        ("time_window.from", {
            let mut it = scope();
            it.requirements.time_window.from = "09:00".to_owned();
            it
        }),
        ("time_window.to", {
            let mut it = scope();
            it.requirements.time_window.to = "22:00".to_owned();
            it
        }),
        ("attendees", {
            let mut it = scope();
            it.requirements.attendees = 200;
            it
        }),
        ("wheelchair_accessible", {
            let mut it = scope();
            it.requirements.wheelchair_accessible = false;
            it
        }),
        ("max_fee", {
            let mut it = scope();
            it.requirements.max_fee = Money::from_pence(500_000);
            it
        }),
        ("expires_at_ms", {
            let mut it = scope();
            it.expires_at_ms = NOW + 900_000;
            it
        }),
        ("grant_ttl_ms", {
            let mut it = scope();
            it.grant_ttl_ms = 7_200_000;
            it
        }),
    ];

    for (field, mutated) in mutations {
        assert_ne!(
            base_digest,
            mutated.digest(),
            "changing {field} left the digest identical — the field is shown to \
             a person but not covered by the hash they are approving"
        );
        assert_ne!(
            base_preview,
            mutated.preview(CODE, NOW),
            "changing {field} left the preview identical — the field is hashed \
             but never shown, so nobody approved it"
        );
    }
}

/// The digest cannot depend on the order permissions were collected in.
///
/// A `HashSet` here would make a valid approval fail its own scope check after
/// a restart, intermittently — the worst available failure mode.
#[test]
fn the_behaviour_set_hashes_the_same_in_any_order() {
    let mut forwards = scope();
    forwards.behaviours = BehaviourSet::new([Behaviour::Book, Behaviour::Cancel]);
    let mut backwards = scope();
    backwards.behaviours = BehaviourSet::new([Behaviour::Cancel, Behaviour::Book]);

    assert_eq!(forwards.digest(), backwards.digest());
    assert_eq!(forwards.preview(CODE, NOW), backwards.preview(CODE, NOW));
}

/// Duplicates are not a different permission set.
#[test]
fn the_behaviour_set_dedupes() {
    let mut once = scope();
    once.behaviours = BehaviourSet::new([Behaviour::Book]);
    let mut twice = scope();
    twice.behaviours = BehaviourSet::new([Behaviour::Book, Behaviour::Book]);

    assert_eq!(once.digest(), twice.digest());
    assert_eq!(once.behaviours.as_slice().len(), 1);
}

/// Two scopes a delimiter-join would confuse must not agree.
///
/// # The defect this pins
///
/// Join the fields on `|` and these two scopes encode to the same string: the
/// first has a purpose ending in `|b` with a date of `c`, the second a purpose
/// of `a` and a date beginning `b|`. One approval would then satisfy the other's
/// scope check. ADR-023 recorded the same choice for the inbound identity's
/// derived id; length prefixes are why both are injective.
#[test]
fn length_prefixing_keeps_two_confusable_scopes_apart() {
    let mut first = scope();
    first.requirements.purpose = "a|b".to_owned();
    first.requirements.requested_date = "c".to_owned();

    let mut second = scope();
    second.requirements.purpose = "a".to_owned();
    second.requirements.requested_date = "b|c".to_owned();

    assert_ne!(first.digest(), second.digest());
}

/// The digest survives its own text form, and refuses anything else.
#[test]
fn the_digest_round_trips_through_hex_and_rejects_junk() {
    let digest = scope().digest();
    let text = digest.to_string();

    assert_eq!(text.len(), 64);
    assert_eq!(ScopeHash::parse_hex(&text), Some(digest));
    assert_eq!(ScopeHash::parse_hex("not a digest"), None);
    assert_eq!(
        ScopeHash::parse_hex(&text[..63]),
        None,
        "short hex accepted"
    );
    assert_eq!(
        ScopeHash::parse_hex(&"z".repeat(64)),
        None,
        "non-hex accepted"
    );
}

/// The preview reads as §13.2's does, and says what a person needs.
#[test]
fn the_preview_reads_as_the_spec_example_does() {
    let preview = scope().preview(CODE, NOW);

    assert!(preview.starts_with("BLD booking request\n"));
    assert!(preview.contains("Agent: TownHallAgent"));
    assert!(
        preview.contains("May: book one meeting room; cancel that booking"),
        "permissions must read as words, not as state-machine names: {preview}"
    );
    assert!(preview.contains("Attendees: <= 20"));
    assert!(preview.contains("Wheelchair access: required"));
    assert!(
        preview.contains("Maximum booking fee: £50.00"),
        "a fee ceiling must render as pounds and pence: {preview}"
    );
    assert!(preview.contains("Reply within 10 minutes."));
    assert!(
        preview.contains("Permission then lasts 60 minutes."),
        "a person must be told how long the permission lasts, not only how long \
         they have to answer: {preview}"
    );
    assert!(preview.ends_with("Reply YES 7312 to approve.\nReply NO 7312 to reject."));
    assert!(
        !preview.contains("Behaviour::"),
        "a debug spelling reached a human: {preview}"
    );
}

/// A permission with no time left says so rather than underflowing.
#[test]
fn an_elapsed_offer_says_it_has_expired() {
    let scope = scope();
    let past = scope.expires_at_ms + 1;

    assert!(
        scope
            .preview(CODE, past)
            .contains("This request has expired."),
        "an elapsed offer must read as a sentence, not as a fragment"
    );
    assert!(
        scope
            .preview(CODE, scope.expires_at_ms - 30_000)
            .contains("Reply within the next minute.")
    );
    assert!(
        scope
            .preview(CODE, scope.expires_at_ms - 90_000)
            .contains("Reply within 1 minute.")
    );
}

/// `NO` is offered, not only `YES`.
///
/// Spec §13.2's own preview carries both words. ADR-025 records the pair as
/// load-bearing: a rejected challenge must become terminal, which is
/// impossible if the person was never told they could reject.
#[test]
fn the_preview_offers_rejection() {
    let preview = scope().preview(CODE, NOW);
    assert!(preview.contains("Reply NO 7312 to reject."));
}
