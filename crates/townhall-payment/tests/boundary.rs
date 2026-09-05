//! The payment verifier's trust posture, on the resolved graph (ADR-025, ADR-030).
//!
//! This crate is trusted — it is where a Stripe signature is checked and (later)
//! where a verified webhook becomes a `Verified` fact. But it must never reach the
//! MUTATION surface: it verifies and hands a fact to the boundary; it does not
//! persist or drive. So it forbids the store, the service coordinator, and the
//! HTTP adapter, even as its later layers come to name `townhall-domain` (to build
//! the fact type, as `council-client`'s verifier does).

use townhall_testkit::resolved_dependencies;

#[test]
fn the_verifier_cannot_name_the_mutation_surface() {
    let forbidden = [
        "townhall-store",
        "townhall-service",
        "townhall-http",
        "sqlx",
    ];
    let deps = resolved_dependencies("townhall-payment", &["normal"]);
    for name in forbidden {
        assert!(
            !deps.iter().any(|dep| dep == name),
            "townhall-payment must not depend on {name} in its normal graph"
        );
    }
}
