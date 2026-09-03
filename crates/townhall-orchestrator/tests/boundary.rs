//! The orchestrator's trust posture, on the resolved graph.
//!
//! Spec §3.2 marks this layer "may mutate authoritative booking state? No" —
//! and the crate graph is how that stays true when nobody is looking. Its only
//! route to a booking is the gateway's socket.

use townhall_testkit::resolved_dependencies;

#[test]
fn the_orchestrator_cannot_name_the_servers_insides() {
    let forbidden = [
        "townhall-service",
        "townhall-store",
        "townhall-http",
        // The issuer/verifier. The orchestrator holds the untrusted proposer
        // seat, so it must reach authority only through the wire, never by
        // naming the crate that mints grants (ADR-025). This was DOCUMENTED as
        // enforced by `townhall-http`'s approvals module — and it was not, until
        // this line: the list forbade everything but the one crate the claim
        // was about. Found reviewing the M7C plan, in already-merged code.
        "townhall-authority",
        "bld-kernel",
        "sqlx",
    ];
    let deps = resolved_dependencies("townhall-orchestrator", &["normal"]);
    for name in forbidden {
        assert!(
            !deps.iter().any(|dep| dep == name),
            "townhall-orchestrator must not depend on {name}"
        );
    }
}

/// The testkit is test machinery and must never enter a NORMAL graph — it
/// spawns processes and reads databases, which is exactly the toolkit
/// production code must not quietly inherit.
#[test]
fn the_testkit_stays_out_of_every_normal_graph() {
    for package in [
        "townhall-channel",
        "townhall-gateway",
        "townhall-orchestrator",
        "sms-simulator",
        "townhall-server",
        "townhall-service",
        "townhall-store",
    ] {
        let deps = resolved_dependencies(package, &["normal"]);
        assert!(
            !deps.iter().any(|dep| dep == "townhall-testkit"),
            "{package} links the testkit into its normal graph"
        );
    }
}
