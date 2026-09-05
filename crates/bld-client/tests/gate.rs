//! The M9 gate (spec §12; ADR-029): a GENERIC BLD client discovers a service from
//! its signed manifest and drives it with NO hard-coded behaviour URLs beyond the
//! bootstrap.
//!
//! The load-bearing fact these witnesses turn on (§0): a projection publishes a
//! behaviour by its `PascalCase` name (`Cancel`), but the route matches a `kebab`
//! segment (`cancel`). A client that hard-coded either spelling — or mechanically
//! transformed one into the other — would drive the API without ever reading the
//! manifest, and the gate would be a decoration. So the DECISIVE witness is the
//! gold relabel-resign test below: a manifest whose `Cancel` segment is relabelled
//! to a well-formed but WRONG value and RE-SIGNED by the real publisher. A client
//! that obeys the manifest posts the wrong segment and is met with 404; a client
//! that hard-codes `cancel` would succeed. Only one of those passes here.

use bld_client::{BldClient, Catalogue, ClientError, Discovered, discover, discover_entry};
use bld_manifest::{SignedManifest, SigningKey, VerifyingKey, signing_key_from_hex};
use townhall_testkit::{MANIFEST_KEY_HEX, world_discoverable};

/// The dev bearer the discoverable world authenticates, and the principal it acts
/// for (see `DevAuthority`). The client prepends `Bearer `.
const BEARER: &str = "dev-lucy";
const PRINCIPAL: &str = "lucy";
const RESOURCE: &str = "booking-intents";

/// The publisher keypair the world serves its manifest under — the signing half
/// is the world's `--manifest-key`, the verifying half is what a client pins.
fn publisher_keys() -> (SigningKey, VerifyingKey) {
    let signing = signing_key_from_hex(MANIFEST_KEY_HEX).expect("64-hex signing key");
    let verifying = signing.verifying_key();
    (signing, verifying)
}

fn hex32(bytes: &[u8; 32]) -> String {
    use std::fmt::Write as _;
    let mut out = String::with_capacity(64);
    for byte in bytes {
        let _ = write!(out, "{byte:02x}");
    }
    out
}

fn create_body(id: &str) -> serde_json::Value {
    serde_json::json!({
        "id": id,
        "purpose": "community meeting",
        "requested_date": "2026-09-15",
        "from": "13:00",
        "to": "17:00",
        "attendees": 20,
        "wheelchair_accessible": true,
        "max_fee_pence": 5_000,
    })
}

/// W-happy: the whole loop, driven from the manifest. Discover → create → read →
/// drive `Cancel`. The client never spells `booking-intents` or `cancel` itself:
/// the resource path comes from the manifest, the behaviour NAME comes from the
/// projection's `available_behaviours`, and the segment is looked up in the
/// manifest by that name.
#[tokio::test]
async fn a_generic_client_discovers_and_drives_a_booking_end_to_end() {
    let world = world_discoverable();
    let (_signing, verifying) = publisher_keys();
    let http = reqwest::Client::new();

    let discovered = discover(&http, &world.server_url, &verifying)
        .await
        .expect("the served manifest verifies against the pinned key");
    assert_eq!(discovered.service(), "demo-town-hall-booking");

    let booking = "b-happy";
    // In the dev lane the delegation reference IS the booking id (DevAuthority).
    let client = BldClient::new(
        discovered,
        http,
        PRINCIPAL,
        BEARER,
        Some(booking.to_owned()),
    );

    let created = client
        .create(RESOURCE, create_body(booking))
        .await
        .expect("create, over the manifest's collection path");
    assert_eq!(created.state, "Draft");

    let read = client.read(RESOURCE, booking).await.expect("read");
    assert!(
        read.available_behaviours.iter().any(|b| b == "Cancel"),
        "the projection offers Cancel by name: {read:?}"
    );
    let version = read.version.expect("a read carries a version ETag");

    let cancelled = client
        .drive(
            RESOURCE,
            booking,
            "Cancel",
            serde_json::json!({ "reason": "no longer needed" }),
            Some(version),
        )
        .await
        .expect("Cancel drives via the segment the manifest maps its name to");
    assert_eq!(
        cancelled.state, "Cancelled",
        "the local Cancel transition landed: {cancelled:?}"
    );
}

/// W2 (DECISIVE): the gold relabel-resign test. The SAME client code, the SAME
/// server — only the manifest's `Cancel` segment is relabelled (to a well-formed
/// but non-existent route) and re-signed by the real publisher. The client posts
/// the relabelled segment, and the server answers 404 "no such behaviour route".
///
/// # Why this is the decisive proof, and what would defeat a weaker one
///
/// A client that hard-coded `cancel`, or derived it from the `PascalCase` name by
/// a kebab transform, would post `cancel` here and CANCEL THE BOOKING — the exact
/// success the happy-path test above shows. That it instead 404s proves the wire
/// segment came from the manifest and nowhere else. `create` and `read` succeed
/// first (their paths are untouched by the relabel), so the ONLY thing that fails
/// is the drive, and specifically because the segment is unknown to the router.
#[tokio::test]
async fn a_relabelled_resigned_segment_is_what_the_client_posts() {
    let world = world_discoverable();
    let (signing, verifying) = publisher_keys();
    let http = reqwest::Client::new();

    // Fetch the REAL signed manifest off the wire, then tamper + re-sign it as the
    // publisher. Re-signing is the point: an unsigned edit would be caught by
    // `verify`; a publisher-signed edit is one the client is BOUND to obey.
    let mut signed: SignedManifest = http
        .get(format!("{}/.well-known/bld", world.server_url))
        .send()
        .await
        .expect("discovery responds")
        .json()
        .await
        .expect("a signed manifest");
    signed
        .verify(&verifying)
        .expect("the served manifest is genuine before we tamper with it");

    let cancel = signed
        .manifest
        .resource_links
        .get_mut(RESOURCE)
        .expect("the manifest describes booking-intents")
        .behaviours
        .get_mut("Cancel")
        .expect("the manifest maps the Cancel behaviour");
    assert_eq!(
        cancel.segment, "cancel",
        "the real segment, before relabelling"
    );
    // Well-formed, but no such route exists.
    cancel.segment = "cancel-please".to_owned();

    let resigned = signed
        .manifest
        .clone()
        .sign(&signing)
        .expect("re-sign the tampered core as the publisher");
    let discovered = Discovered::verified(&world.server_url, resigned, &verifying)
        .expect("a publisher-signed manifest verifies, wrong segment and all");

    let booking = "b-gold";
    let client = BldClient::new(
        discovered,
        http,
        PRINCIPAL,
        BEARER,
        Some(booking.to_owned()),
    );

    // The untouched paths still work — so the failure below is about the segment,
    // not about a broken world.
    client
        .create(RESOURCE, create_body(booking))
        .await
        .expect("create still works — the collection path was not relabelled");
    let version = client
        .read(RESOURCE, booking)
        .await
        .expect("read still works — the item path was not relabelled")
        .version
        .expect("a version ETag");

    let refused = client
        .drive(
            RESOURCE,
            booking,
            "Cancel",
            serde_json::json!({ "reason": "no longer needed" }),
            Some(version),
        )
        .await
        .expect_err("the client posted the relabelled segment, which routes nowhere");
    match refused {
        ClientError::Refused { status, detail } => {
            assert_eq!(status, 404, "an unknown segment is not a route: {detail}");
            assert!(
                detail.contains("no such behaviour route"),
                "the server rejected the segment itself, not the auth or the body: {detail}"
            );
        }
        other => panic!("expected a 404 for the relabelled segment, got {other:?}"),
    }
}

/// W2b: the static half of the gate — the client SOURCE hard-codes no behaviour
/// segment. A source that contained one would let the gold test above be gamed by
/// spelling the real segment regardless of the manifest; this closes that by
/// reading the shipped source and refusing any behaviour-URL literal.
#[test]
fn the_client_source_hard_codes_no_behaviour_segment() {
    // Walk EVERY source file, not just lib.rs — a hard-coded segment hiding in a
    // future submodule must be caught too (the client is one file today, but the
    // scan should not silently narrow to it).
    let mut sources = Vec::new();
    collect_rs(
        std::path::Path::new(concat!(env!("CARGO_MANIFEST_DIR"), "/src")),
        &mut sources,
    );
    assert!(
        sources.iter().any(|(path, _)| path.ends_with("lib.rs")),
        "the scan found no source at all — a broken walk would pass vacuously"
    );

    // The seven kebab segments, the collection/route fragment, and the two
    // one-word behaviours as quoted literals (bare `book`/`cancel` are ordinary
    // English — `"book"`/`"cancel"` in source would be a hard-coded wire token).
    let forbidden = [
        "select-venue",
        "verify-slot",
        "change-venue",
        "update-requirements",
        "revalidate-venue",
        "/behaviours/",
        "booking-intents",
        "\"book\"",
        "\"cancel\"",
    ];
    for (path, source) in &sources {
        for needle in forbidden {
            assert!(
                !source.contains(needle),
                "{path} hard-codes {needle:?} — a behaviour/resource URL must come \
                 from the manifest, not the source"
            );
        }
    }
}

/// Read every `.rs` file under `dir`, recursively, as `(display path, contents)`.
fn collect_rs(dir: &std::path::Path, out: &mut Vec<(String, String)>) {
    for entry in std::fs::read_dir(dir).expect("src is readable").flatten() {
        let path = entry.path();
        if path.is_dir() {
            collect_rs(&path, out);
        } else if path.extension().is_some_and(|ext| ext == "rs") {
            let text = std::fs::read_to_string(&path).expect("a source file is readable");
            out.push((path.display().to_string(), text));
        }
    }
}

/// W3: the untrusted driver mutates NOTHING without a grant. The same discovered
/// client, but carrying no delegation, is refused at the door — a change needs an
/// approved grant, and a driver that resolves no authority of its own has one only
/// if it was handed one.
#[tokio::test]
async fn a_change_with_no_delegation_is_refused() {
    let world = world_discoverable();
    let (_signing, verifying) = publisher_keys();
    let http = reqwest::Client::new();

    let discovered = discover(&http, &world.server_url, &verifying)
        .await
        .expect("discovery verifies");

    // delegation: None — the client was handed no grant.
    let client = BldClient::new(discovered, http, PRINCIPAL, BEARER, None);

    let refused = client
        .create(RESOURCE, create_body("b-nogrant"))
        .await
        .expect_err("a create is a change, and a change needs a grant");
    match refused {
        ClientError::Refused { status, detail } => {
            assert_eq!(
                status, 401,
                "no grant is an authorization failure: {detail}"
            );
            // The reason, not just the code — so a rejected BEARER (also 401, but
            // "no verified caller identity") cannot pass this off as the same thing.
            assert!(
                detail.contains("delegation"),
                "the refusal is about the missing GRANT, not the bearer: {detail}"
            );
        }
        other => panic!("expected a 401 with no delegation, got {other:?}"),
    }
}

/// W7: discovery is OPT-IN. A server started WITHOUT `--manifest-key` serves no
/// `/.well-known/bld` — so the manifest route is a deliberate act of publishing,
/// not something every server leaks. The keyless world 404s the endpoint, and the
/// client's own `discover` surfaces that as a transport failure rather than a
/// silent empty manifest.
#[tokio::test]
async fn a_server_without_a_manifest_key_publishes_no_manifest() {
    // world(), not world_discoverable() — no --manifest-key.
    let world = townhall_testkit::world();
    let (_signing, verifying) = publisher_keys();
    let http = reqwest::Client::new();

    let raw = http
        .get(format!("{}/.well-known/bld", world.server_url))
        .send()
        .await
        .expect("the server answers the request");
    assert!(
        !raw.status().is_success(),
        "a keyless server must not serve a manifest, got {}",
        raw.status()
    );

    let refused = discover(&http, &world.server_url, &verifying)
        .await
        .expect_err("there is nothing to discover on a keyless server");
    assert!(
        matches!(refused, ClientError::Transport(_)),
        "a missing manifest is a transport failure, not a verified empty one: {refused:?}"
    );
}

/// W4: the local catalogue (§12) — a file of services, each with the key to trust
/// it by. The client reads the catalogue, discovers the one service it lists
/// (pinning that key), and drives it. The registry stand-in, resolving to a real
/// signed manifest and a real drive.
#[tokio::test]
async fn a_catalogue_entry_resolves_and_drives() {
    let world = world_discoverable();
    let (_signing, verifying) = publisher_keys();
    let http = reqwest::Client::new();

    let dir = tempfile::tempdir().expect("a temp dir");
    let path = dir.path().join("catalogue.json");
    let catalogue = serde_json::json!({
        "services": [
            { "base_url": world.server_url, "publisher_key": hex32(&verifying.to_bytes()) }
        ]
    });
    std::fs::write(&path, serde_json::to_vec(&catalogue).expect("json")).expect("write catalogue");

    let catalogue = Catalogue::from_file(&path).expect("the catalogue parses");
    let entry = catalogue
        .services()
        .first()
        .expect("the catalogue lists one service");

    let discovered = discover_entry(&http, entry)
        .await
        .expect("the catalogue entry resolves, pinning its own key");
    assert_eq!(discovered.service(), "demo-town-hall-booking");

    let booking = "b-catalogue";
    let client = BldClient::new(
        discovered,
        http,
        PRINCIPAL,
        BEARER,
        Some(booking.to_owned()),
    );
    let created = client
        .create(RESOURCE, create_body(booking))
        .await
        .expect("a service found through the catalogue drives like any other");
    assert_eq!(created.state, "Draft");
}

/// W5: a catalogue entry pinning the WRONG publisher key is refused. Discovery is
/// only as good as the pin — a manifest that does not verify against the entry's
/// key is one the client must not drive from, however well-formed it is.
#[tokio::test]
async fn a_catalogue_entry_with_the_wrong_key_is_rejected() {
    let world = world_discoverable();
    let http = reqwest::Client::new();

    // A valid, well-formed key — just not the publisher's.
    let impostor =
        signing_key_from_hex("0101010101010101010101010101010101010101010101010101010101010101")
            .expect("64-hex key")
            .verifying_key();

    let refused = discover(&http, &world.server_url, &impostor)
        .await
        .expect_err("the genuine manifest does not verify against an impostor key");
    assert!(
        matches!(refused, ClientError::Unverified),
        "a bad pin is an authenticity failure, not a transport one: {refused:?}"
    );
}

/// W6: a manifest whose `bld_version` major the client does not speak is refused
/// BEFORE any drive. Verification is crypto AND compatibility — a client that
/// checked the signature but not the version could parse fields it does not
/// understand and drive garbage. The manifest is re-signed at the new version, so
/// this is a genuine, authentic manifest the client still declines.
#[tokio::test]
async fn an_incompatible_bld_version_is_refused_before_driving() {
    let world = world_discoverable();
    let (signing, verifying) = publisher_keys();
    let http = reqwest::Client::new();

    let mut signed: SignedManifest = http
        .get(format!("{}/.well-known/bld", world.server_url))
        .send()
        .await
        .expect("discovery responds")
        .json()
        .await
        .expect("a signed manifest");

    // A different MAJOR, re-signed as the publisher — authentic, but not ours.
    signed.manifest.bld_version = "1.0".to_owned();
    let resigned = signed
        .manifest
        .clone()
        .sign(&signing)
        .expect("re-sign at the new version");

    let refused = Discovered::verified(&world.server_url, resigned, &verifying)
        .expect_err("a major this client does not speak is declined");
    match refused {
        ClientError::IncompatibleVersion { found } => {
            assert_eq!(found, "1.0", "the refusal names the version it saw");
        }
        other => panic!("expected an incompatible-version refusal, got {other:?}"),
    }
}
