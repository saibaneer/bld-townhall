//! The manifest the server PUBLISHES and the routes the server MATCHES must agree
//! on every behaviour segment (M9/ADR-029, review Finding 7).
//!
//! A generic client reads a behaviour's segment out of the manifest (keyed by the
//! `PascalCase` name a projection published) and posts it. If the manifest
//! published a segment the router's `parse_proposal` does not resolve, that client
//! would 404 on a behaviour that genuinely exists — the discovery contract broken
//! silently. Both sides read `bld_types::Behaviour` today, so they cannot drift;
//! this pins that they cannot, so a future edit that hard-codes a segment in the
//! generator (instead of asking the table) fails here rather than in the field.

use bld_types::Behaviour;
use townhall_http::discovery::booking_manifest;

#[test]
fn every_published_segment_routes_back_to_its_behaviour() {
    let manifest = booking_manifest();
    let link = manifest
        .resource_links
        .get("booking-intents")
        .expect("the manifest describes the booking-intents resource");

    // The template a client fills must actually carry the segment placeholder, or
    // the segment it looked up would go nowhere.
    assert!(
        link.behaviour_template.contains("{segment}"),
        "the behaviour template must interpolate the segment: {:?}",
        link.behaviour_template
    );

    for behaviour in Behaviour::ALL {
        let published = link
            .behaviours
            .get(behaviour.name())
            .unwrap_or_else(|| panic!("the manifest omits the {} behaviour", behaviour.name()));
        // The published segment is the one the table spells...
        assert_eq!(
            published.segment,
            behaviour.segment(),
            "manifest segment for {} drifted from the table",
            behaviour.name()
        );
        // ...and the router resolves that same segment back to this behaviour.
        assert_eq!(
            Behaviour::from_segment(&published.segment),
            Some(behaviour),
            "the router does not resolve the published segment {:?} back to {}",
            published.segment,
            behaviour.name()
        );
    }

    // No stray behaviours beyond the seven — the manifest publishes the whole
    // table and nothing invented.
    assert_eq!(
        link.behaviours.len(),
        Behaviour::ALL.len(),
        "the manifest publishes exactly the behaviour table"
    );
}
