//! The BLD client's trust posture, on the RESOLVED dependency graph (ADR-025,
//! ADR-029).
//!
//! This crate is an UNTRUSTED driver. It proposes; it disposes of nothing. The
//! dependency list is how that stays true when nobody is looking: a socket is
//! its only route to booking state, and it cannot reach the segment table it is
//! meant to discover.

use townhall_testkit::resolved_dependencies;

/// The untrusted-driver set: the client must reach booking state, authority and
/// storage ONLY over the wire, never by naming the crate that holds them.
///
/// `bld-types` is on this list, and that is the load-bearing addition for M9. The
/// client's whole job is to discover the behaviour name→segment table from the
/// signed manifest. `bld_types::Behaviour::segment()` IS that table, in code — if
/// the client could name it, it could skip the manifest and derive the wire
/// segment mechanically, and the gate ("no hard-coded behaviour URLs") would be
/// unenforceable because the shortcut would be invisible. Forbidding the crate
/// closes it: the client works in strings, and a string it did not read from the
/// manifest is one it made up.
#[test]
fn the_client_cannot_name_the_services_insides_or_the_segment_table() {
    let forbidden = [
        "townhall-service",
        "townhall-store",
        "townhall-http",
        "townhall-domain",
        "townhall-authority",
        "bld-kernel",
        "sqlx",
        // The M9 addition — see the module note. The one shared artifact is
        // `bld-manifest` (the wire shape), which carries no behaviour knowledge
        // the manifest itself does not.
        "bld-types",
    ];
    let deps = resolved_dependencies("bld-client", &["normal"]);
    for name in forbidden {
        assert!(
            !deps.iter().any(|dep| dep == name),
            "bld-client must not depend on {name} in its normal graph"
        );
    }
}
