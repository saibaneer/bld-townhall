//! The secondary tripwire behind the crate-graph boundary (ADR-021): the
//! adapter crate's sources never NAME a mutation primitive, a store type, or
//! an evidence constructor. The structural proof is `townhall-http`'s Cargo
//! manifest — this scan exists to catch a future re-plumbing that quietly
//! adds the dependency back, and it is documented as the weaker of the two
//! nets (a determined refactor can defeat a grep; it cannot defeat the
//! dependency graph without showing up in the manifest diff).

#[test]
fn the_adapter_sources_name_no_mutation_primitive() {
    let root = std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("../../crates/townhall-http");
    let forbidden = [
        "prepare_effect",
        "finalize_effect",
        "handoff_effect",
        "load_effect",
        "claim_effect",
        "mark_escalated",
        "note_attempt",
        "release_lease",
        "assert_verified",
        "resolve_fact",
        "resolve_proposal",
        "SystemEvent",
        "BookingRepository",
        "SqliteBookingRepository",
        "townhall_store",
        "council_client",
        "sqlx",
        ".repository(",
        ".capability(",
        // NOT ".create(" or ".commit(": the facade's own `create` is the
        // sanctioned mutation, and no commit exists on any surface this crate
        // can reach — the crate-graph half below is what forbids the store's.
    ];
    let mut scanned = 0usize;
    for entry in walk(&root.join("src")) {
        let source = std::fs::read_to_string(&entry).expect("readable source");
        for name in forbidden {
            assert!(
                !source.contains(name),
                "{} names {name:?} — the adapter must reach the boundary only \
                 through BookingFacade and ReconcilerHandle",
                entry.display()
            );
        }
        scanned += 1;
    }
    assert!(scanned >= 2, "the scan found the adapter's sources");

    // The manifest half: the structural boundary, asserted from the file that
    // enforces it.
    let manifest = std::fs::read_to_string(root.join("Cargo.toml")).expect("the adapter manifest");
    for name in ["townhall-store", "council-client", "sqlx", "bld-kernel"] {
        assert!(
            !manifest.contains(name),
            "townhall-http's manifest must not depend on {name}"
        );
    }
}

fn walk(dir: &std::path::Path) -> Vec<std::path::PathBuf> {
    let mut files = Vec::new();
    for entry in std::fs::read_dir(dir).expect("a directory") {
        let path = entry.expect("an entry").path();
        if path.is_dir() {
            files.extend(walk(&path));
        } else if path.extension().is_some_and(|extension| extension == "rs") {
            files.push(path);
        }
    }
    files
}
