//! BLD discovery (M9, spec §12; ADR-029): the manifest a generic client bootstraps
//! from, served signed at `GET /.well-known/bld`.
//!
//! # Why the manifest is built from the ONE segment table
//!
//! The behaviour name→segment mapping the manifest publishes is read from
//! `bld_types::Behaviour` — the same source the router's `parse_proposal` reads
//! (M9/ADR-029). So the router, the wire spelling and the manifest cannot drift:
//! a behaviour renamed in one place is renamed in all three.
//!
//! This crate BUILDS the (unsigned) manifest; the composition root signs it with
//! the publisher key and hands the signed bytes back here to serve. Discovery is
//! deliberately UNAUTHENTICATED — it precedes credentials, and the manifest
//! carries only public routing facts and its own signature.

use crate::mapping;
use axum::{Router, extract::State, response::Response, routing::get};
use bld_manifest::{BehaviourLink, Manifest, ResourceLink};
use bld_types::Behaviour;
use std::collections::BTreeMap;
use std::sync::Arc;

/// The prebuilt, signed manifest JSON the discovery route serves.
#[derive(Clone)]
pub struct DiscoveryState {
    pub manifest: Arc<serde_json::Value>,
}

/// The one route: `GET /.well-known/bld`. No auth, no `ETag` — discovery is what a
/// client does BEFORE it has credentials or a resource version.
pub fn discovery_router(state: DiscoveryState) -> Router {
    Router::new()
        .route("/.well-known/bld", get(manifest))
        .with_state(state)
}

async fn manifest(State(state): State<DiscoveryState>) -> Response {
    mapping::json_response(axum::http::StatusCode::OK, &state.manifest)
}

/// The town-hall service's manifest CORE (unsigned), built from the one behaviour
/// table so it cannot drift from the router. The caller signs it.
///
/// `resource_links` carries `booking-intents` — the resource the gate drives. The
/// behaviour table maps each behaviour's discovery name (`PascalCase`, as the
/// projection publishes it) to its wire segment (kebab, as the route matches it)
/// plus a body-field hint.
#[must_use]
pub fn booking_manifest() -> Manifest {
    let behaviours: BTreeMap<String, BehaviourLink> = Behaviour::ALL
        .into_iter()
        .map(|b| {
            (
                b.name().to_owned(),
                BehaviourLink {
                    segment: b.segment().to_owned(),
                    body: body_hint(b),
                },
            )
        })
        .collect();

    let mut resource_links = BTreeMap::new();
    resource_links.insert(
        "booking-intents".to_owned(),
        ResourceLink {
            collection: "/booking-intents".to_owned(),
            item: "/booking-intents/{id}".to_owned(),
            behaviour_template: "/booking-intents/{id}/behaviours/{segment}".to_owned(),
            behaviours,
        },
    );

    Manifest {
        bld_version: "0.2".to_owned(),
        service: "demo-town-hall-booking".to_owned(),
        publisher: "demo-council".to_owned(),
        resources: vec!["booking-intents".to_owned()],
        concurrency: "etag-if-match".to_owned(),
        authority_profile: "bld-demo-delegation-v1".to_owned(),
        resource_links,
    }
}

/// The request-body field names each behaviour expects — a hint so a client can
/// assemble a body from discovery (ADR-029). The server knows its own bodies; the
/// spellings match `parse_proposal`'s `serde` structs.
fn body_hint(behaviour: Behaviour) -> Vec<String> {
    match behaviour {
        Behaviour::SelectVenue => vec!["venue_id".to_owned(), "slot_id".to_owned()],
        Behaviour::UpdateRequirements => vec!["attendees".to_owned()],
        Behaviour::Cancel => vec!["reason".to_owned()],
        // Boundary-derived; empty body (spec §10).
        Behaviour::VerifySlot
        | Behaviour::ChangeVenue
        | Behaviour::RevalidateVenue
        | Behaviour::Book => Vec::new(),
    }
}
