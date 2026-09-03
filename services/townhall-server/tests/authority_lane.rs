//! The lane with NO dev feature: a grant exists because somebody was asked.
//!
//! # Why this file is not `#![cfg(feature = "dev-authority")]`
//!
//! Because `tests/http.rs` is, in its entirety. ADR-025's amendment recorded
//! that as a gap with teeth: a test asserting "the dev lane is closed" cannot
//! live inside a file that only exists when the dev lane is open. Everything
//! here compiles and runs in a default build, which is the build a deployment
//! would make.
//!
//! What it covers:
//!
//! 1. The real approval flow end to end — challenge, reply, reference, booking
//!    — with no dev token anywhere.
//! 2. ADR-025's two amendment tests, which are two properties and not one: the
//!    flag is unavailable in a build without the feature, and the running real
//!    resolver refuses a `dev-*` token.

use std::io::{BufRead as _, BufReader};
use std::process::{Child, Command, Stdio};

const COUNCIL_KEY_HEX: &str = "0707070707070707070707070707070707070707070707070707070707070707";
const AUTHORITY_KEY_HEX: &str = "a7a7a7a7a7a7a7a7a7a7a7a7a7a7a7a7a7a7a7a7a7a7a7a7a7a7a7a7a7a7a7a7";

/// The one workload credential the real resolver knows.
///
/// A workload, not a person — which is the whole point of M7B's split. It
/// authenticates the caller and authorizes nothing.
const AGENT: &str = "Bearer agent-townhall";

struct Lane {
    _dir: tempfile::TempDir,
    council: Child,
    server: Child,
    url: String,
    db: std::path::PathBuf,
}

impl Drop for Lane {
    fn drop(&mut self) {
        let _ = self.server.kill();
        let _ = self.council.kill();
    }
}

fn build_binaries() {
    let status = Command::new(env!("CARGO"))
        .args(["build", "-p", "mock-council"])
        .status()
        .expect("cargo runs");
    assert!(status.success(), "the council binary must build");
}

fn spawn_ready(mut command: Command) -> (Child, u16) {
    let mut child = command
        .stdout(Stdio::piped())
        .spawn()
        .expect("the binary spawns");
    let stdout = child.stdout.take().expect("piped stdout");
    let line = BufReader::new(stdout)
        .lines()
        .next()
        .expect("a line")
        .expect("readable");
    let port = line
        .strip_prefix("READY ")
        .unwrap_or_else(|| panic!("expected READY, got {line:?}"))
        .parse()
        .expect("a port");
    (child, port)
}

/// A world with the REAL resolver: no `--dev-authority`, so nothing can mint.
fn lane() -> Lane {
    build_binaries();
    let dir = tempfile::tempdir().expect("tempdir");
    let council_db = dir.path().join("council.sqlite");
    let mut council = Command::new(
        std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("../../target/debug/mock-council"),
    );
    council
        .arg("--db")
        .arg(&council_db)
        .args(["--key-hex", COUNCIL_KEY_HEX, "--port", "0"]);
    let (council, council_port) = spawn_ready(council);

    let db = dir.path().join("townhall.sqlite");
    let mut server = Command::new(env!("CARGO_BIN_EXE_townhall-server"));
    server
        .arg("--db")
        .arg(&db)
        .arg("--denials-db")
        .arg(dir.path().join("denials.sqlite"))
        .args([
            "--council-url",
            &format!("http://127.0.0.1:{council_port}"),
            "--key-hex",
            COUNCIL_KEY_HEX,
            "--authority-key",
            AUTHORITY_KEY_HEX,
            "--port",
            "0",
            "--reconcile-interval-ms",
            "50",
        ]);
    let (server, port) = spawn_ready(server);

    Lane {
        _dir: dir,
        council,
        server,
        url: format!("http://127.0.0.1:{port}"),
        db,
    }
}

fn http() -> reqwest::blocking::Client {
    reqwest::blocking::Client::builder()
        .timeout(std::time::Duration::from_secs(10))
        .build()
        .expect("a client")
}

/// Bind Lucy's phone, so a READ can be scoped to her.
///
/// Written through the store because binding a phone is M7C's work, with the
/// verification flow that earns the assurance level. What M7B needs is only
/// that the row exists — the read gate checks it rather than believing a
/// header.
fn bind_lucy(lane: &Lane) {
    let db = lane.db.clone();
    std::thread::spawn(move || {
        tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .expect("a runtime")
            .block_on(async move {
                let repository = townhall_store::SqliteBookingRepository::open(&db)
                    .await
                    .expect("the same database the server opened");
                let store =
                    townhall_store::authority::SqlApprovalStore::new(repository.pool().clone());
                store
                    .bind_channel(
                        &townhall_store::authority::ChannelBinding {
                            id: "binding-lucy".to_owned(),
                            address: "+447700900123".to_owned(),
                            principal: bld_types::PrincipalId::new("lucy"),
                            version: 1,
                            assurance: townhall_authority::AssuranceLevel::SmsReply,
                            withdrawn: false,
                        },
                        Some("test lane"),
                        1_700_000_000_000,
                    )
                    .await
                    .expect("the binding is written");
            });
    })
    .join()
    .expect("the binding thread did not panic");
}

/// A change requires a challenge, answered against a live binding.
///
/// # What this proves, and what it does NOT
///
/// It was called `a_booking_needs_an_approval_that_somebody_actually_answered`,
/// and review was right that it showed no such thing: it reads the code out of
/// the HTTP response to its OWN request and posts it back with the same
/// credential. That is a workload approving its own request — the opposite of
/// what the old name claimed.
///
/// What it does prove, and what is worth proving:
///
/// - a change with no delegation is refused, first, before anything else;
/// - the reference is produced by a challenge and is NOT the booking id;
/// - the same grant is reused across the whole workflow, once;
/// - revocation stops the next change and does not unmake the last one.
///
/// What it cannot prove, because M7B has no channel: that a PERSON answered.
/// The verifier now checks the claimed binding against a live row rather than
/// against the caller's own earlier claim, so a binding cannot be invented —
/// but the code still travels through this workload, and a workload holding the
/// credential can still relay it to itself. Closing that needs evidence from
/// the channel adapter, which arrives with M7C, and is named in the M7B
/// acceptance record.
#[test]
// One sweep, deliberately. Each step depends on the one before it — a reference
// cannot be presented before it has been issued, and a revocation cannot be
// tested before there is something to revoke. Split into seven tests, six of
// them would have to re-do the approval, and the ordering they exist to prove
// would stop being visible in one place.
#[allow(clippy::too_many_lines)]
fn a_change_requires_a_challenge_answered_against_a_live_binding() {
    let lane = lane();
    bind_lucy(&lane);
    let client = http();
    let booking = "BKG-APPROVED";

    // 1. Without a grant, a change is refused. Not "eventually" — first.
    let refused = client
        .post(format!("{}/booking-intents", lane.url))
        .header("authorization", AGENT)
        .header("x-bld-principal", "lucy")
        .json(&create_body(booking))
        .send()
        .expect("the server answers");
    assert_eq!(
        refused.status().as_u16(),
        401,
        "a change with no delegation must be refused before anything else"
    );

    // 2. Ask Lucy.
    let raised = client
        .post(format!("{}/approvals", lane.url))
        .header("authorization", AGENT)
        .json(&serde_json::json!({
            "booking": booking,
            "grantor": "lucy",
            "subject": "lucy",
            "binding_principal": "lucy",
            "binding_version": 1,
            "behaviours": ["SelectVenue", "VerifySlot", "Book", "Cancel"],
            "purpose": "community meeting",
            "requested_date": "2026-09-10",
            "from": "14:00",
            "to": "17:00",
            "attendees": 20,
            "wheelchair_accessible": true,
            "max_fee_pence": 5_000
        }))
        .send()
        .expect("the server answers");
    assert_eq!(raised.status().as_u16(), 201);
    let raised: serde_json::Value = raised.json().expect("json");
    let challenge = raised["challenge"].as_str().expect("a challenge id");
    let preview = raised["preview"].as_str().expect("a preview");

    // The preview is what Lucy would read, and the code is inside it. Reading it
    // back out HERE is not what she does — she reads it off a phone — and that
    // difference is the limit this test cannot cross (see the header).
    assert!(
        preview.contains("Maximum booking fee: £50.00"),
        "the preview must state the ceiling she is approving: {preview}"
    );
    let code = code_from(preview);

    // 3. A wrong code is refused, and says how many tries are left.
    let wrong = client
        .post(format!("{}/approvals/{challenge}/reply", lane.url))
        .header("authorization", AGENT)
        .json(&serde_json::json!({
            "answer": "YES", "code": "0000",
            "binding_principal": "lucy", "binding_version": 1
        }))
        .send()
        .expect("answer");
    assert_eq!(
        wrong.status().as_u16(),
        403,
        "a wrong code is heard and refused"
    );

    // 4. The right code, from the bound channel.
    let approved = client
        .post(format!("{}/approvals/{challenge}/reply", lane.url))
        .header("authorization", AGENT)
        .json(&serde_json::json!({
            "answer": "YES", "code": code,
            "binding_principal": "lucy", "binding_version": 1
        }))
        .send()
        .expect("answer");
    assert_eq!(approved.status().as_u16(), 201);
    let reference = approved.json::<serde_json::Value>().expect("json")["delegation"]
        .as_str()
        .expect("a delegation reference")
        .to_owned();
    assert_ne!(
        reference, booking,
        "a real reference is not the booking id — that is the dev lane's \
         stand-in, and this lane does not use it"
    );

    // 5. Now the same change is permitted, presenting the reference.
    let created = client
        .post(format!("{}/booking-intents", lane.url))
        .header("authorization", AGENT)
        .header("x-bld-principal", "lucy")
        .header("x-bld-delegation", &reference)
        .json(&create_body(booking))
        .send()
        .expect("answer");
    assert_eq!(created.status().as_u16(), 201, "the approved change lands");

    // 6. And the booking walks to Booked under that same grant, reused — one
    //    challenge, one grant, many calls (ADR-025's distinction).
    let mut version = 0_u64;
    for (behaviour, body) in [
        (
            "select-venue",
            serde_json::json!({"venue_id": "TH-A", "slot_id": "SLOT-A"}),
        ),
        ("verify-slot", serde_json::json!({})),
        ("book", serde_json::json!({})),
    ] {
        let reply = client
            .post(format!(
                "{}/booking-intents/{booking}/behaviours/{behaviour}",
                lane.url
            ))
            .header("authorization", AGENT)
            .header("x-bld-principal", "lucy")
            .header("x-bld-delegation", &reference)
            .header("if-match", format!("\"{version}\""))
            .json(&body)
            .send()
            .expect("answer");
        assert_eq!(reply.status().as_u16(), 200, "{behaviour}");
        version = reply
            .headers()
            .get("etag")
            .and_then(|value| value.to_str().ok())
            .and_then(|value| value.trim_matches('"').parse().ok())
            .unwrap_or_else(|| panic!("{behaviour} must return an ETag"));
    }

    let read = client
        .get(format!("{}/booking-intents/{booking}", lane.url))
        .header("authorization", AGENT)
        .header("x-bld-principal", "lucy")
        .send()
        .expect("answer");
    assert_eq!(read.status().as_u16(), 200);
    let body = read.text().expect("text");
    assert!(body.contains("\"Booked\""), "{body}");

    // 7. Revoked, and the same reference stops working — while the booking it
    //    already made stands. Revocation blocks the NEXT change; it does not
    //    unmake a committed one (ADR-025).
    let revoked = client
        .post(format!("{}/delegations/{reference}/revoke", lane.url))
        .header("authorization", AGENT)
        .send()
        .expect("answer");
    assert_eq!(revoked.status().as_u16(), 200);

    let after = client
        .post(format!(
            "{}/booking-intents/{booking}/behaviours/cancel",
            lane.url
        ))
        .header("authorization", AGENT)
        .header("x-bld-principal", "lucy")
        .header("x-bld-delegation", &reference)
        .header("if-match", format!("\"{version}\""))
        .json(&serde_json::json!({"reason": "after revocation"}))
        .send()
        .expect("answer");
    assert_eq!(
        after.status().as_u16(),
        401,
        "a revoked reference authorizes nothing"
    );

    let still = client
        .get(format!("{}/booking-intents/{booking}", lane.url))
        .header("authorization", AGENT)
        .header("x-bld-principal", "lucy")
        .send()
        .expect("answer")
        .text()
        .expect("text");
    assert!(
        still.contains("\"Booked\""),
        "revocation must not unmake a committed booking: {still}"
    );
}

/// `NO` is terminal, and a later `YES` does not revive it.
#[test]
fn a_declined_request_cannot_be_approved_afterwards() {
    let lane = lane();
    // Bound, because declining is still ANSWERING: the verifier checks the
    // claimed binding against a row before it reads the answer at all.
    bind_lucy(&lane);
    let client = http();

    let raised = client
        .post(format!("{}/approvals", lane.url))
        .header("authorization", AGENT)
        .json(&serde_json::json!({
            "booking": "BKG-DECLINED",
            "grantor": "lucy", "subject": "lucy",
            "binding_principal": "lucy", "binding_version": 1,
            "behaviours": ["Book"],
            "purpose": "community meeting",
            "requested_date": "2026-09-10", "from": "14:00", "to": "17:00",
            "attendees": 20, "wheelchair_accessible": true, "max_fee_pence": 5_000
        }))
        .send()
        .expect("answer")
        .json::<serde_json::Value>()
        .expect("json");
    let challenge = raised["challenge"].as_str().expect("id").to_owned();
    let code = code_from(raised["preview"].as_str().expect("preview"));

    let declined = client
        .post(format!("{}/approvals/{challenge}/reply", lane.url))
        .header("authorization", AGENT)
        .json(&serde_json::json!({
            "answer": "NO", "code": code,
            "binding_principal": "lucy", "binding_version": 1
        }))
        .send()
        .expect("answer");
    assert_eq!(declined.status().as_u16(), 200);
    assert!(
        declined.text().expect("text").contains("rejected"),
        "declining is an outcome of asking, not an error"
    );

    let revived = client
        .post(format!("{}/approvals/{challenge}/reply", lane.url))
        .header("authorization", AGENT)
        .json(&serde_json::json!({
            "answer": "YES", "code": code,
            "binding_principal": "lucy", "binding_version": 1
        }))
        .send()
        .expect("answer");
    assert_eq!(
        revived.status().as_u16(),
        410,
        "a declined request is gone, and a later YES must not revive it"
    );
}

/// ADR-025's amendment, first property: the running real resolver refuses a
/// `dev-*` token.
///
/// # Why this is a separate test from the one below
///
/// Because they are separate properties, and the ADR says so in as many words:
/// "do not call the test 'dev token refused' if the server never starts."
/// This server IS running, with the real resolver, and refuses the token.
#[test]
fn the_real_resolver_refuses_a_dev_token() {
    let lane = lane();
    let client = http();

    for token in [
        "Bearer dev-lucy",
        "Bearer dev-marco-restricted",
        "Bearer dev-priya-nobook",
    ] {
        let reply = client
            .get(format!("{}/booking-intents/BKG-ANY", lane.url))
            .header("authorization", token)
            .header("x-bld-principal", "lucy")
            .send()
            .expect("the server answers");
        assert_eq!(
            reply.status().as_u16(),
            401,
            "{token} must not authenticate against the real resolver"
        );
    }

    // And the same request with the real workload credential is admitted —
    // so the refusals above are about the token and not about the request.
    bind_lucy(&lane);
    let reply = client
        .get(format!("{}/booking-intents/BKG-ANY", lane.url))
        .header("authorization", AGENT)
        .header("x-bld-principal", "lucy")
        .send()
        .expect("answer");
    assert_eq!(
        reply.status().as_u16(),
        404,
        "the real credential is admitted and simply finds no such booking"
    );
}

/// ADR-025's amendment, second property: the flag itself is unavailable.
///
/// Compiled only WITHOUT the feature, because that is the property — in a
/// feature-enabled build `--dev-authority` legitimately works, and asserting
/// otherwise would be asserting the build's own configuration back at itself.
#[cfg(not(feature = "dev-authority"))]
#[test]
fn the_dev_authority_flag_does_not_exist_in_this_build() {
    let dir = tempfile::tempdir().expect("tempdir");
    let output = Command::new(env!("CARGO_BIN_EXE_townhall-server"))
        .arg("--db")
        .arg(dir.path().join("townhall.sqlite"))
        .arg("--denials-db")
        .arg(dir.path().join("denials.sqlite"))
        .args([
            "--council-url",
            "http://127.0.0.1:1",
            "--key-hex",
            COUNCIL_KEY_HEX,
            "--authority-key",
            AUTHORITY_KEY_HEX,
            "--port",
            "0",
            "--dev-authority",
        ])
        .output()
        .expect("the binary runs");

    assert!(
        !output.status.success(),
        "a build without the feature must refuse to start with --dev-authority"
    );
    let complaint = String::from_utf8_lossy(&output.stderr);
    assert!(
        complaint.contains("requires building with the dev-authority feature"),
        "the refusal must say why: {complaint}"
    );
}

fn create_body(id: &str) -> serde_json::Value {
    serde_json::json!({
        "id": id,
        "purpose": "community meeting",
        "requested_date": "2026-09-10",
        "from": "14:00",
        "to": "17:00",
        "attendees": 20,
        "wheelchair_accessible": true,
        "max_fee_pence": 5_000
    })
}

/// The code, read out of the preview exactly as a person reads it.
fn code_from(preview: &str) -> String {
    preview
        .lines()
        .find_map(|line| line.strip_prefix("Reply YES "))
        .and_then(|rest| rest.split_whitespace().next())
        .unwrap_or_else(|| panic!("the preview must offer a code: {preview}"))
        .to_owned()
}
