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

use std::io::BufRead as _;
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

/// Wait for `READY <port>`, or fail loudly with everything the child said.
///
/// # Why a deadline exists at all
///
/// It did not, and codex named the consequence before it happened: "any live
/// startup stall will hang indefinitely". It then happened — twice — as CI runs
/// that sat silent for twenty and ninety minutes on a check that takes
/// microseconds locally. A harness that can hang converts an unknown bug into
/// an unknowable one: the step times out with no output and the cause dies with
/// the runner.
///
/// Thirty seconds is generous for a process whose job is to print one line
/// after binding a port, and the panic carries the child's stderr, so the next
/// stall names itself in the CI log instead of being reconstructed from
/// timestamps.
fn spawn_ready(mut command: Command) -> (Child, u16) {
    use std::io::Read as _;
    use std::sync::mpsc;

    let mut child = command
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .expect("the binary spawns");
    let stdout = child.stdout.take().expect("piped stdout");
    let mut stderr = child.stderr.take().expect("piped stderr");

    // The read happens on its own thread so the deadline is real: a blocked
    // read on the main thread IS the hang this function exists to prevent.
    let (sender, receiver) = mpsc::channel();
    std::thread::spawn(move || {
        let mut lines = std::io::BufReader::new(stdout).lines();
        let first = lines.next().map(|line| line.expect("readable stdout"));
        let _ = sender.send(first);
        // Keep draining so the child never blocks on a full stdout pipe.
        for _ in lines {}
    });

    match receiver.recv_timeout(std::time::Duration::from_secs(30)) {
        Ok(Some(line)) => {
            let port = line
                .strip_prefix("READY ")
                .unwrap_or_else(|| panic!("expected READY, got {line:?}"))
                .parse()
                .expect("a port");
            (child, port)
        }
        Ok(None) => {
            // The child exited without a READY line: stdout hit EOF. Its
            // stderr says why, and that is the error worth reading.
            let _ = child.wait();
            let mut said = String::new();
            let _ = stderr.read_to_string(&mut said);
            panic!("the child exited before READY. stderr:\n{said}");
        }
        Err(_) => {
            // Still alive, still silent. Kill it, then report everything it
            // wrote — the difference between this panic and a hung CI step is
            // the entire reason this arm exists.
            let _ = child.kill();
            let _ = child.wait();
            let mut said = String::new();
            let _ = stderr.read_to_string(&mut said);
            panic!("no READY within 30s from a live child. stderr so far:\n{said}");
        }
    }
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

/// The address Lucy's channel is bound to — where a reply comes FROM.
const LUCY_ADDR: &str = "+447700900123";

/// Deposit an inbound reply's evidence at the ingress endpoint and return the
/// one-use receipt to forward with the answer (ADR-026). `msg` keeps the inbound
/// identity unique so a test can deposit more than one reply.
fn deposit_receipt(client: &reqwest::blocking::Client, url: &str, msg: &str) -> String {
    let resp = client
        .post(format!("{url}/inbound-evidence"))
        .header("authorization", AGENT)
        .json(&serde_json::json!({
            "provider": "sim",
            "account": "townhall",
            "message_id": msg,
            "address": LUCY_ADDR,
            "verified": true
        }))
        .send()
        .expect("the ingress answers");
    assert_eq!(
        resp.status().as_u16(),
        201,
        "the ingress deposits the reply's evidence and returns a receipt"
    );
    resp.json::<serde_json::Value>().expect("json")["receipt"]
        .as_str()
        .expect("a receipt")
        .to_owned()
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

    // The reply's evidence is deposited at the ingress; the workload forwards the
    // receipt, never the binding it claims. A wrong code does not consume it, so
    // the same receipt carries the retry.
    let receipt = deposit_receipt(&client, &lane.url, "reply-1");

    // 3. A wrong code is refused, and says how many tries are left.
    let wrong = client
        .post(format!("{}/approvals/{challenge}/reply", lane.url))
        .header("authorization", AGENT)
        .json(&serde_json::json!({
            "answer": "YES", "code": "0000", "receipt": receipt
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
            "answer": "YES", "code": code, "receipt": receipt
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

    // Two receipts deposited while the number still awaits the challenge: the
    // decline consumes one and clears the correlation, and the later YES rides
    // its own already-bound receipt.
    let no_receipt = deposit_receipt(&client, &lane.url, "no");
    let yes_receipt = deposit_receipt(&client, &lane.url, "yes");

    let declined = client
        .post(format!("{}/approvals/{challenge}/reply", lane.url))
        .header("authorization", AGENT)
        .json(&serde_json::json!({
            "answer": "NO", "code": code, "receipt": no_receipt
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
            "answer": "YES", "code": code, "receipt": yes_receipt
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

/// Raise and approve one booking for Lucy over HTTP, returning the delegation
/// reference. `booking` is distinct per call so idempotent-begin raises a fresh
/// challenge; `msg` keeps each reply deposit's transport identity unique.
fn approve(client: &reqwest::blocking::Client, url: &str, booking: &str, msg: &str) -> String {
    let raised: serde_json::Value = client
        .post(format!("{url}/approvals"))
        .header("authorization", AGENT)
        .json(&serde_json::json!({
            "booking": booking,
            "grantor": "lucy", "subject": "lucy",
            "binding_principal": "lucy", "binding_version": 1,
            "behaviours": ["SelectVenue", "VerifySlot", "Book", "Cancel"],
            "purpose": "community meeting",
            "requested_date": "2026-09-10", "from": "14:00", "to": "17:00",
            "attendees": 20, "wheelchair_accessible": true, "max_fee_pence": 5_000
        }))
        .send()
        .expect("the server answers")
        .json()
        .expect("json");
    let challenge = raised["challenge"].as_str().expect("a challenge id");
    let code = code_from(raised["preview"].as_str().expect("a preview"));
    let receipt = deposit_receipt(client, url, msg);
    let approved: serde_json::Value = client
        .post(format!("{url}/approvals/{challenge}/reply"))
        .header("authorization", AGENT)
        .json(&serde_json::json!({ "answer": "YES", "code": code, "receipt": receipt }))
        .send()
        .expect("the server answers")
        .json()
        .expect("json");
    approved["delegation"]
        .as_str()
        .expect("a delegation reference")
        .to_owned()
}

/// Post a control inbound (a REVOKE) to `/revocations` from `address`, returning
/// the raw response for the caller to assert status and body on.
fn post_revocation(
    client: &reqwest::blocking::Client,
    url: &str,
    address: &str,
    msg: &str,
) -> reqwest::blocking::Response {
    client
        .post(format!("{url}/revocations"))
        .header("authorization", AGENT)
        .json(&serde_json::json!({
            "provider": "sim",
            "account": "townhall",
            "message_id": msg,
            "address": address,
            "verified": true
        }))
        .send()
        .expect("the server answers")
}

/// T2 — a texted REVOKE stops EVERY grant over HTTP, and each stops authorizing.
///
/// Two grants for one number, one `POST /revocations`, `{"revoked": 2}` — the
/// bulk sweep through the real lane. Then a change under EACH reference is
/// refused: the count is not cosmetic, the rows are marked. This is the HTTP
/// mirror of the unit sweep, and it fails a "count only" revoke that marks
/// nothing or marks the wrong rows.
#[test]
fn a_texted_revoke_stops_every_grant_and_each_stops_authorizing() {
    let lane = lane();
    bind_lucy(&lane);
    let client = http();

    let r1 = approve(&client, &lane.url, "BKG-REVOKE-1", "reply-1");
    let r2 = approve(&client, &lane.url, "BKG-REVOKE-2", "reply-2");

    // r1 authorizes a change BEFORE the revoke — the grant is real and live.
    let before = client
        .post(format!("{}/booking-intents", lane.url))
        .header("authorization", AGENT)
        .header("x-bld-principal", "lucy")
        .header("x-bld-delegation", &r1)
        .json(&create_body("BKG-REVOKE-1"))
        .send()
        .expect("answer");
    assert_eq!(
        before.status().as_u16(),
        201,
        "the grant authorizes before revoke"
    );

    let revoked = post_revocation(&client, &lane.url, LUCY_ADDR, "revoke-1");
    assert_eq!(revoked.status().as_u16(), 200);
    let body: serde_json::Value = revoked.json().expect("json");
    assert_eq!(
        body["revoked"].as_u64(),
        Some(2),
        "one REVOKE stops both grants the number authorized"
    );

    // Both references are dead now — a change under either is refused.
    for (reference, booking) in [(&r1, "BKG-AFTER-1"), (&r2, "BKG-AFTER-2")] {
        let after = client
            .post(format!("{}/booking-intents", lane.url))
            .header("authorization", AGENT)
            .header("x-bld-principal", "lucy")
            .header("x-bld-delegation", reference)
            .json(&create_body(booking))
            .send()
            .expect("answer");
        assert_eq!(
            after.status().as_u16(),
            401,
            "a revoked reference authorizes nothing"
        );
    }
}

/// T3 (HTTP) — a REVOKE from an unbound number stops nothing and cannot touch a
/// victim's grants.
///
/// The anti-DoS core over the wire: the sweep resolves the sender to a live
/// binding, so a number bound to no one is a `403` that sweeps nothing. Lucy's
/// grant, from her own bound number, still authorizes after. This fails an impl
/// that swept on the forgeable `claimed_sender` without resolving it — the exact
/// hole the first ADR-026 draft had.
#[test]
fn a_revoke_from_an_unbound_number_stops_nothing() {
    let lane = lane();
    bind_lucy(&lane);
    let client = http();

    let reference = approve(&client, &lane.url, "BKG-VICTIM", "reply-1");

    let refused = post_revocation(&client, &lane.url, "+447700900999", "forged-1");
    assert_eq!(
        refused.status().as_u16(),
        403,
        "an unbound number resolves to no binding — it may sweep nothing"
    );

    // The victim's grant, from her OWN bound number, still authorizes a change.
    let still = client
        .post(format!("{}/booking-intents", lane.url))
        .header("authorization", AGENT)
        .header("x-bld-principal", "lucy")
        .header("x-bld-delegation", &reference)
        .json(&create_body("BKG-VICTIM"))
        .send()
        .expect("answer");
    assert_eq!(
        still.status().as_u16(),
        201,
        "the forged REVOKE left the victim's grant untouched"
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
