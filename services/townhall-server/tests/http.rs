#![cfg(feature = "dev-authority")]

//! M5's gate, over the real wire: the server binary and the council binary,
//! both spawned, driven with HTTP requests — and, for the gate's literal
//! clause, with the actual `curl` binary.
//!
//! Wall-clock honesty: the server runs on `SystemStoreClock`, which no test
//! can move, so convergence tests configure a SHORT retry cadence and poll
//! with a bounded deadline. That is a stated exception to the no-sleep rule
//! (PLAN-M5 §2), not a violation of it.

use std::io::BufRead as _;
use std::process::{Child, Command, Stdio};

const KEY_HEX: &str = "0707070707070707070707070707070707070707070707070707070707070707";
const LUCY: &str = "Bearer dev-lucy";
const MARCO: &str = "Bearer dev-marco-restricted";

struct World {
    dir: tempfile::TempDir,
    council: Child,
    council_url: String,
    council_db: std::path::PathBuf,
    server: Option<Child>,
    server_url: String,
}

impl Drop for World {
    fn drop(&mut self) {
        if let Some(mut server) = self.server.take() {
            let _ = server.kill();
            let _ = server.wait();
        }
        let _ = self.council.kill();
        let _ = self.council.wait();
    }
}

fn spawn_ready(mut command: Command) -> (Child, u16) {
    let mut child = command
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .spawn()
        .expect("spawn");
    let stdout = child.stdout.take().expect("piped stdout");
    let mut lines = std::io::BufReader::new(stdout).lines();
    let ready = lines.next().expect("a line").expect("readable");
    let port: u16 = ready
        .strip_prefix("READY ")
        .unwrap_or_else(|| panic!("expected READY, got {ready:?}"))
        .parse()
        .expect("a port");
    // Keep draining stdout so the child never blocks on a full pipe.
    std::thread::spawn(move || for _ in lines {});
    (child, port)
}

fn build_binaries() {
    let status = Command::new(env!("CARGO"))
        .args(["build", "-p", "mock-council", "--features", "test-faults"])
        .status()
        .expect("build council");
    assert!(status.success());
}

fn spawn_council(dir: &std::path::Path) -> (Child, String, std::path::PathBuf) {
    let binary =
        std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("../../target/debug/mock-council");
    let db = dir.join("council.sqlite");
    let mut command = Command::new(binary);
    command
        .arg("--db")
        .arg(&db)
        .args(["--key-hex", KEY_HEX, "--port", "0"]);
    let (child, port) = spawn_ready(command);
    (child, format!("http://127.0.0.1:{port}"), db)
}

fn spawn_server(world: &mut World, extra: &[&str]) {
    let mut command = Command::new(env!("CARGO_BIN_EXE_townhall-server"));
    command
        .arg("--db")
        .arg(world.dir.path().join("townhall.sqlite"))
        .arg("--denials-db")
        .arg(world.dir.path().join("denials.sqlite"))
        .args(["--council-url", &world.council_url])
        .args(["--key-hex", KEY_HEX, "--port", "0", "--dev-authority"])
        .args(["--retry-cadence-ms", "100"])
        .args(["--reconcile-interval-ms", "50"])
        .args(extra);
    let (child, port) = spawn_ready(command);
    world.server = Some(child);
    world.server_url = format!("http://127.0.0.1:{port}");
}

fn world() -> World {
    build_binaries();
    let dir = tempfile::tempdir().expect("tempdir");
    let (council, council_url, council_db) = spawn_council(dir.path());
    let mut world = World {
        dir,
        council,
        council_url,
        council_db,
        server: None,
        server_url: String::new(),
    };
    spawn_server(&mut world, &[]);
    world
}

// --------------------------------------------------------------- http client

fn http() -> reqwest::blocking::Client {
    reqwest::blocking::Client::builder()
        .timeout(std::time::Duration::from_secs(15))
        .build()
        .expect("client")
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

struct Reply {
    status: u16,
    etag: Option<String>,
    retry_after: Option<String>,
    request_id: Option<String>,
    body: serde_json::Value,
}

fn call(
    world: &World,
    method: &str,
    path: &str,
    bearer: &str,
    if_match: Option<&str>,
    body: Option<&serde_json::Value>,
) -> Reply {
    let client = http();
    let url = format!("{}{path}", world.server_url);
    let mut request = match method {
        "GET" => client.get(&url),
        "POST" => client.post(&url),
        other => panic!("unsupported method {other}"),
    };
    if !bearer.is_empty() {
        request = request.header("authorization", bearer);
    }
    if let Some(version) = if_match {
        request = request.header("if-match", version);
    }
    if let Some(json) = body {
        request = request.json(json);
    }
    let response = request.send().expect("the server answers");
    let status = response.status().as_u16();
    let header = |name: &str| {
        response
            .headers()
            .get(name)
            .and_then(|value| value.to_str().ok())
            .map(str::to_owned)
    };
    let etag = header("etag");
    let retry_after = header("retry-after");
    let request_id = header("x-request-id");
    let body: serde_json::Value = response.json().unwrap_or(serde_json::Value::Null);
    Reply {
        status,
        etag,
        retry_after,
        request_id,
        body,
    }
}

fn etag_version(reply: &Reply) -> String {
    reply.etag.clone().expect("an ETag")
}

/// Drive one booking to `AwaitingBooking` over HTTP; returns the current `ETag`.
fn awaiting(world: &World, id: &str) -> String {
    let created = call(
        world,
        "POST",
        "/booking-intents",
        LUCY,
        None,
        Some(&create_body(id)),
    );
    assert_eq!(created.status, 201, "{:?}", created.body);
    let mut etag = etag_version(&created);
    for (behaviour, body) in [
        (
            "select-venue",
            Some(serde_json::json!({"venue_id": "TH-A", "slot_id": "SLOT-A"})),
        ),
        ("verify-slot", None),
    ] {
        let reply = call(
            world,
            "POST",
            &format!("/booking-intents/{id}/behaviours/{behaviour}"),
            LUCY,
            Some(&etag),
            body.as_ref(),
        );
        assert_eq!(reply.status, 200, "{behaviour}: {:?}", reply.body);
        etag = etag_version(&reply);
    }
    etag
}

fn council_count(world: &World, sql: &str) -> i64 {
    let db = world.council_db.clone();
    let sql = sql.to_owned();
    std::thread::spawn(move || rusqlite_shim(&db, &sql))
        .join()
        .expect("count thread")
}

/// A tiny SQLite read without adding a driver dependency: sqlite3 CLI ships
/// with macOS and the CI image.
fn rusqlite_shim(db: &std::path::Path, sql: &str) -> i64 {
    let output = Command::new("sqlite3")
        .arg(db)
        .arg(sql)
        .output()
        .expect("sqlite3 available");
    String::from_utf8_lossy(&output.stdout)
        .trim()
        .parse::<i64>()
        .unwrap_or(0)
}

fn arm_fault(world: &World, effect: &str, route: &str, fault: &str) -> u64 {
    let response: serde_json::Value = http()
        .post(format!("{}/test/faults", world.council_url))
        .json(&serde_json::json!({
            "effect_intent_id": effect,
            "route": route,
            "fault": fault,
        }))
        .send()
        .expect("arm")
        .json()
        .expect("json");
    response["fault_id"].as_u64().expect("id")
}

/// Poll a projection until the predicate holds — bounded wall-clock, stated.
fn poll_until(world: &World, id: &str, deadline: std::time::Duration, want: &str) -> Reply {
    let start = std::time::Instant::now();
    loop {
        let reply = call(
            world,
            "GET",
            &format!("/booking-intents/{id}"),
            LUCY,
            None,
            None,
        );
        if reply.body["state"] == want {
            return reply;
        }
        assert!(
            start.elapsed() < deadline,
            "never reached {want}: {:?}",
            reply.body
        );
        std::thread::sleep(std::time::Duration::from_millis(50));
    }
}

// -------------------------------------------------------------------- gates

/// The M5 gate's first clause, literally: create → select → verify → book →
/// cancel, driven by the REAL `curl` binary, every mutation guarded by the
/// `ETag` carried forward from the previous response's headers — no internal
/// IDs, no store access, no Rust helpers on the wire path.
#[test]
fn the_whole_journey_is_possible_with_curl_alone() {
    fn curl(args: &[&str]) -> (u16, String, String) {
        let output = Command::new("curl")
            .args(["-s", "-D", "-", "-o", "-"])
            .args(args)
            .output()
            .expect("curl available");
        let raw = String::from_utf8_lossy(&output.stdout).to_string();
        let (headers, body) = raw.split_once("\r\n\r\n").unwrap_or((raw.as_str(), ""));
        let status: u16 = headers
            .lines()
            .next()
            .and_then(|line| line.split_whitespace().nth(1))
            .and_then(|code| code.parse().ok())
            .expect("a status line");
        let etag = headers
            .lines()
            .find_map(|line| line.strip_prefix("etag: "))
            .unwrap_or_default()
            .trim()
            .to_owned();
        (status, etag, body.to_owned())
    }

    let world = world();
    let base = &world.server_url;
    let auth = "authorization: Bearer dev-lucy";
    let json = "content-type: application/json";

    let (status, mut etag, _) = curl(&[
        "-X",
        "POST",
        &format!("{base}/booking-intents"),
        "-H",
        auth,
        "-H",
        json,
        "--data",
        &create_body("BKG-CURL").to_string(),
    ]);
    assert_eq!(status, 201);

    for (behaviour, data) in [
        (
            "select-venue",
            Some(r#"{"venue_id":"TH-A","slot_id":"SLOT-A"}"#),
        ),
        ("verify-slot", None),
        ("book", None),
    ] {
        let mut args = vec!["-X", "POST"];
        let url = format!("{base}/booking-intents/BKG-CURL/behaviours/{behaviour}");
        args.push(&url);
        args.extend_from_slice(&["-H", auth]);
        let if_match = format!("if-match: {etag}");
        args.extend_from_slice(&["-H", &if_match]);
        if let Some(data) = data {
            // The JSON content-type travels only WITH a body — a bodyless
            // request claiming JSON content is a malformed request, and the
            // server refuses it (axum's extractor, correctly).
            args.extend_from_slice(&["-H", json, "--data", data]);
        }
        let (status, next_etag, body) = curl(&args);
        assert_eq!(status, 200, "{behaviour}: {body}");
        etag = next_etag;
        assert!(!etag.is_empty(), "{behaviour} must return an ETag");
    }

    let (status, _, body) = curl(&[&format!("{base}/booking-intents/BKG-CURL"), "-H", auth]);
    assert_eq!(status, 200);
    assert!(body.contains("\"Booked\""), "{body}");

    let url = format!("{base}/booking-intents/BKG-CURL/behaviours/cancel");
    let if_match = format!("if-match: {etag}");
    let (status, _, body) = curl(&[
        "-X",
        "POST",
        &url,
        "-H",
        auth,
        "-H",
        json,
        "-H",
        &if_match,
        "--data",
        r#"{"reason":"done with curl"}"#,
    ]);
    assert_eq!(status, 200, "{body}");
    assert!(body.contains("\"Cancelled\""), "{body}");

    assert_eq!(
        council_count(&world, "SELECT COUNT(*) FROM bookings"),
        1,
        "one booking, ever"
    );
    assert_eq!(
        council_count(
            &world,
            "SELECT COUNT(*) FROM bookings WHERE cancelled_by IS NOT NULL"
        ),
        1,
        "and one cancellation"
    );
}

/// The M4 acceptance scenario over the wire (ADR-019's 202 rule): the answer
/// is eaten, book answers 202 with the store's own Retry-After, the loop
/// converges it, and polling GET reaches Booked with exactly one booking.
#[test]
fn a_dropped_response_answers_202_and_the_loop_converges_it() {
    let mut world = world();
    // A DISTINCTIVE cadence, so "derived from the store's schedule" is
    // witnessed at the wire: 7 300 ms must ceiling to exactly 8 seconds — a
    // constant Retry-After: 1 dies here (battery audit).
    if let Some(mut server) = world.server.take() {
        let _ = server.kill();
        let _ = server.wait();
    }
    spawn_server(&mut world, &["--retry-cadence-ms", "7300"]);
    let etag = awaiting(&world, "BKG-202");
    // The book effect departs from version 2 — the identity is derivable, so
    // the fault is armed exactly (E's discipline).
    let fault = arm_fault(&world, "EFF-BKG-202-BOOK-2", "create", "drop_response");

    let reply = call(
        &world,
        "POST",
        "/booking-intents/BKG-202/behaviours/book",
        LUCY,
        Some(&etag),
        None,
    );
    assert_eq!(reply.status, 202, "{:?}", reply.body);
    let retry_after: i64 = reply
        .retry_after
        .expect("202 carries Retry-After")
        .parse()
        .expect("seconds");
    assert_eq!(
        retry_after, 8,
        "the store's 7300ms schedule, ceiling-rounded — never a constant"
    );

    let consumed: serde_json::Value = http()
        .get(format!("{}/test/faults/{fault}", world.council_url))
        .send()
        .expect("status")
        .json()
        .expect("json");
    assert_eq!(consumed["consumed"], 1, "the drop genuinely fired");

    let converged = poll_until(
        &world,
        "BKG-202",
        std::time::Duration::from_secs(10),
        "Booked",
    );
    assert_eq!(converged.status, 200);
    assert_eq!(
        council_count(&world, "SELECT COUNT(*) FROM bookings"),
        1,
        "exactly one booking — the loop converged, nothing duplicated"
    );
}

/// 412, the winner-commits-first schedule, over the wire — and the loser
/// changed nothing: same version, same audit length, no council arrival.
#[test]
fn a_stale_if_match_is_refused_with_the_fresh_etag() {
    let world = world();
    let stale = awaiting(&world, "BKG-412");

    let winner = call(
        &world,
        "POST",
        "/booking-intents/BKG-412/behaviours/update-requirements",
        LUCY,
        Some(&stale),
        Some(&serde_json::json!({})),
    );
    assert_eq!(winner.status, 200);
    let fresh = etag_version(&winner);
    let audit_before = call(
        &world,
        "GET",
        "/booking-intents/BKG-412/audit",
        LUCY,
        None,
        None,
    );
    let rows_before = audit_before.body["audit"].as_array().expect("rows").len();

    let loser = call(
        &world,
        "POST",
        "/booking-intents/BKG-412/behaviours/book",
        LUCY,
        Some(&stale),
        None,
    );
    assert_eq!(loser.status, 412, "{:?}", loser.body);
    assert_eq!(
        etag_version(&loser),
        fresh,
        "the refusal ships the CURRENT ETag so the client can re-read"
    );
    let after = call(&world, "GET", "/booking-intents/BKG-412", LUCY, None, None);
    assert_eq!(etag_version(&after), fresh, "the loser changed nothing");
    let audit_after = call(
        &world,
        "GET",
        "/booking-intents/BKG-412/audit",
        LUCY,
        None,
        None,
    );
    assert_eq!(
        audit_after.body["audit"].as_array().expect("rows").len(),
        rows_before
    );
    assert_eq!(
        council_count(&world, "SELECT COUNT(*) FROM bookings"),
        0,
        "and nothing touched the wire"
    );
}

/// A booking's inertness witness: the `ETag` and the audit length, snapshotted.
fn snapshot(world: &World, id: &str) -> (String, usize) {
    let read = call(
        world,
        "GET",
        &format!("/booking-intents/{id}"),
        LUCY,
        None,
        None,
    );
    let audit = call(
        world,
        "GET",
        &format!("/booking-intents/{id}/audit"),
        LUCY,
        None,
        None,
    );
    (
        etag_version(&read),
        audit.body["audit"].as_array().expect("rows").len(),
    )
}

fn assert_inert(world: &World, id: &str, before: &(String, usize), when: &str) {
    let after = snapshot(world, id);
    assert_eq!(after.0, before.0, "{when}: the version moved");
    assert_eq!(after.1, before.1, "{when}: an audit row appeared");
}

/// Every refusal class in one sweep, each asserted INERT: no version movement,
/// no audit row, no council booking — snapshotted around EVERY refusal, not
/// inferred from a final count (battery audit).
// One long sweep, deliberately: the refusal classes are one table in the spec
// (§10.2), and this test IS that table read left to right.
#[allow(clippy::too_many_lines)]
#[test]
fn refusals_answer_their_spec_status_and_change_nothing() {
    let world = world();
    let etag = awaiting(&world, "BKG-REFUSE");

    // 401: no bearer, unknown bearer.
    for bearer in ["", "Bearer nobody"] {
        let reply = call(
            &world,
            "GET",
            "/booking-intents/BKG-REFUSE",
            bearer,
            None,
            None,
        );
        assert_eq!(reply.status, 401);
    }
    // 400: the reserved delegation header.
    let reply = http()
        .get(format!("{}/booking-intents/BKG-REFUSE", world.server_url))
        .header("authorization", LUCY)
        .header("x-bld-delegation", "not-yet")
        .send()
        .expect("answer");
    assert_eq!(reply.status().as_u16(), 400);

    // 403: restricted principal proposing a booking.
    let refuse_before = snapshot(&world, "BKG-REFUSE");
    let reply = call(
        &world,
        "POST",
        "/booking-intents/BKG-REFUSE/behaviours/book",
        MARCO,
        Some(&etag),
        None,
    );
    assert_eq!(reply.status, 403, "{:?}", reply.body);
    assert_eq!(reply.body["error"], "BookingAuthorityRequired");
    assert_inert(&world, "BKG-REFUSE", &refuse_before, "403");

    // 404: unknown booking, read and audit alike.
    for path in [
        "/booking-intents/BKG-NOBODY",
        "/booking-intents/BKG-NOBODY/audit",
    ] {
        let reply = call(&world, "GET", path, LUCY, None, None);
        assert_eq!(reply.status, 404);
    }

    // 409 Undefined: booking from a state whose menu lacks it (fresh Draft).
    let created = call(
        &world,
        "POST",
        "/booking-intents",
        LUCY,
        None,
        Some(&create_body("BKG-DRAFT")),
    );
    let draft_etag = etag_version(&created);
    let draft_before = snapshot(&world, "BKG-DRAFT");
    let reply = call(
        &world,
        "POST",
        "/booking-intents/BKG-DRAFT/behaviours/book",
        LUCY,
        Some(&draft_etag),
        None,
    );
    assert_eq!(reply.status, 409, "{:?}", reply.body);
    assert_eq!(
        reply.body["available_behaviours"],
        serde_json::json!(["SelectVenue", "Cancel"]),
        "the 409 teaches the menu"
    );
    assert_inert(&world, "BKG-DRAFT", &draft_before, "409 Undefined");

    // 409 duplicate create, carrying the existing ETag.
    let duplicate = call(
        &world,
        "POST",
        "/booking-intents",
        LUCY,
        None,
        Some(&create_body("BKG-REFUSE")),
    );
    assert_eq!(duplicate.status, 409);
    assert!(duplicate.etag.is_some());

    // 428 missing If-Match; 400 wildcard, weak, multiple, and misplaced.
    let reply = call(
        &world,
        "POST",
        "/booking-intents/BKG-REFUSE/behaviours/book",
        LUCY,
        None,
        None,
    );
    assert_eq!(reply.status, 428);
    for bad in ["*", "W/\"2\""] {
        let reply = call(
            &world,
            "POST",
            "/booking-intents/BKG-REFUSE/behaviours/book",
            LUCY,
            Some(bad),
            None,
        );
        assert_eq!(reply.status, 400, "If-Match {bad}");
    }
    assert_inert(
        &world,
        "BKG-REFUSE",
        &refuse_before,
        "428/400 header refusals",
    );
    let reply = http()
        .post(format!(
            "{}/booking-intents/BKG-REFUSE/behaviours/book",
            world.server_url
        ))
        .header("authorization", LUCY)
        .header("if-match", "\"1\", \"2\"")
        .send()
        .expect("answer");
    assert_eq!(reply.status().as_u16(), 400, "multi-valued If-Match");
    let reply = call(
        &world,
        "POST",
        "/booking-intents",
        LUCY,
        Some("\"0\""),
        Some(&create_body("BKG-NEVER")),
    );
    assert_eq!(reply.status, 400, "If-Match where no precondition applies");

    // 422, the guard stories: capacity at verify-slot (TH-D holds 12), and
    // the REQUIREMENT fee ceiling (TH-C at £90 vs a £50 requirement under a
    // generous... lucy's authority is £50, so TH-C is the AUTHORITY ceiling:
    let mut tight = create_body("BKG-CAP");
    tight["attendees"] = serde_json::json!(20);
    let created = call(&world, "POST", "/booking-intents", LUCY, None, Some(&tight));
    let mut cap_etag = etag_version(&created);
    let reply = call(
        &world,
        "POST",
        "/booking-intents/BKG-CAP/behaviours/select-venue",
        LUCY,
        Some(&cap_etag),
        Some(&serde_json::json!({"venue_id": "TH-D", "slot_id": "SLOT-A"})),
    );
    cap_etag = etag_version(&reply);
    let cap_before = snapshot(&world, "BKG-CAP");
    let reply = call(
        &world,
        "POST",
        "/booking-intents/BKG-CAP/behaviours/verify-slot",
        LUCY,
        Some(&cap_etag),
        None,
    );
    assert_eq!(reply.status, 422, "{:?}", reply.body);
    assert_eq!(reply.body["error"], "CapacityInsufficient");
    assert_inert(&world, "BKG-CAP", &cap_before, "422 capacity");

    // 403 vs 422 on the fee, split by WHICH ceiling refused (ADR-021):
    // TH-C's £90 exceeds lucy's £50 authority → 403 …
    let created = call(
        &world,
        "POST",
        "/booking-intents",
        LUCY,
        None,
        Some(&create_body("BKG-FEE-AUTH")),
    );
    let mut fee_etag = etag_version(&created);
    let reply = call(
        &world,
        "POST",
        "/booking-intents/BKG-FEE-AUTH/behaviours/select-venue",
        LUCY,
        Some(&fee_etag),
        Some(&serde_json::json!({"venue_id": "TH-C", "slot_id": "SLOT-A"})),
    );
    fee_etag = etag_version(&reply);
    let reply = call(
        &world,
        "POST",
        "/booking-intents/BKG-FEE-AUTH/behaviours/verify-slot",
        LUCY,
        Some(&fee_etag),
        None,
    );
    assert_eq!(reply.status, 403, "{:?}", reply.body);
    assert_eq!(reply.body["error"], "FeeExceededAuthority");

    // … while TH-A's £45 over a £40 REQUIREMENT (authority still £50) → 422.
    let mut frugal = create_body("BKG-FEE-REQ");
    frugal["max_fee_pence"] = serde_json::json!(4_000);
    let created = call(
        &world,
        "POST",
        "/booking-intents",
        LUCY,
        None,
        Some(&frugal),
    );
    let mut req_etag = etag_version(&created);
    let reply = call(
        &world,
        "POST",
        "/booking-intents/BKG-FEE-REQ/behaviours/select-venue",
        LUCY,
        Some(&req_etag),
        Some(&serde_json::json!({"venue_id": "TH-A", "slot_id": "SLOT-A"})),
    );
    req_etag = etag_version(&reply);
    let reply = call(
        &world,
        "POST",
        "/booking-intents/BKG-FEE-REQ/behaviours/verify-slot",
        LUCY,
        Some(&req_etag),
        None,
    );
    assert_eq!(reply.status, 422, "{:?}", reply.body);
    assert_eq!(reply.body["error"], "FeeExceededRequirement");

    // Nothing above caused anything.
    assert_eq!(council_count(&world, "SELECT COUNT(*) FROM bookings"), 0);
}

/// The 503/local pair over the wire: a dead council refuses the ask as 503 —
/// and STILL cannot hold Lucy's withdrawal hostage.
#[test]
fn a_dead_council_answers_503_for_asks_but_cancel_still_commits() {
    let mut world = world();
    let etag = awaiting(&world, "BKG-DEAD");
    // Book with the answer eaten so the booking is genuinely in flight.
    let effect = "EFF-BKG-DEAD-BOOK-2";
    arm_fault(&world, effect, "create", "drop_response");
    let reply = call(
        &world,
        "POST",
        "/booking-intents/BKG-DEAD/behaviours/book",
        LUCY,
        Some(&etag),
        None,
    );
    assert_eq!(reply.status, 202);
    let etag = etag_version(&reply);

    // The council dies.
    let _ = world.council.kill();
    let _ = world.council.wait();

    // Asks: 503 — the catalogue, the slot, and the fact-needing proposal.
    let reply = call(&world, "GET", "/venues", LUCY, None, None);
    assert_eq!(reply.status, 503);
    let reply = call(&world, "GET", "/venues/TH-A/slots/SLOT-A", LUCY, None, None);
    assert_eq!(reply.status, 503);

    // The withdrawal: local, committed, provider not consulted.
    let reply = call(
        &world,
        "POST",
        "/booking-intents/BKG-DEAD/behaviours/cancel",
        LUCY,
        Some(&etag),
        Some(&serde_json::json!({"reason": "council is down, mind is made up"})),
    );
    assert_eq!(reply.status, 200, "{:?}", reply.body);
    assert_eq!(reply.body["state"], "CancellationRequested");
}

/// 429, deterministically: a server composed with a zero re-classification
/// budget answers Contended — with Retry-After (RFC 6585) — for any turn that
/// must classify a fact.
#[test]
fn a_zero_reclassification_budget_answers_429_with_retry_after() {
    let mut world = world();
    if let Some(mut server) = world.server.take() {
        let _ = server.kill();
        let _ = server.wait();
    }
    spawn_server(&mut world, &["--reclassify-attempts", "0"]);
    let etag = awaiting(&world, "BKG-429");
    let reply = call(
        &world,
        "POST",
        "/booking-intents/BKG-429/behaviours/book",
        LUCY,
        Some(&etag),
        None,
    );
    assert_eq!(reply.status, 429, "{:?}", reply.body);
    assert_eq!(reply.retry_after.as_deref(), Some("1"));
}

/// The trigger genuinely RACING the running loop (battery audit): the loop is
/// live at a 50ms interval while the trigger is spammed through the same
/// convergence — the store's claims arbitrate, and the council's file holds
/// exactly one booking.
#[test]
fn the_trigger_racing_the_live_loop_duplicates_nothing() {
    let world = world();
    let etag = awaiting(&world, "BKG-RACE");
    arm_fault(&world, "EFF-BKG-RACE-BOOK-2", "create", "drop_response");
    let reply = call(
        &world,
        "POST",
        "/booking-intents/BKG-RACE/behaviours/book",
        LUCY,
        Some(&etag),
        None,
    );
    assert_eq!(reply.status, 202);

    // Spam the trigger while the loop chases the same intent.
    let start = std::time::Instant::now();
    loop {
        let reply = call(
            &world,
            "POST",
            "/booking-intents/BKG-RACE/behaviours/reconcile",
            LUCY,
            None,
            None,
        );
        assert_eq!(reply.status, 200, "{:?}", reply.body);
        let read = call(&world, "GET", "/booking-intents/BKG-RACE", LUCY, None, None);
        if read.body["state"] == "Booked" {
            break;
        }
        assert!(
            start.elapsed() < std::time::Duration::from_secs(10),
            "never converged: {:?}",
            read.body
        );
    }
    assert_eq!(
        council_count(&world, "SELECT COUNT(*) FROM bookings"),
        1,
        "loop and trigger raced; the claims let exactly one arrival happen"
    );
}

/// The reconcile trigger alone: exempt from If-Match by classification,
/// converges a dropped answer WITHOUT the loop (parked at an hour) — and on a
/// settled booking it attends nothing and changes nothing.
#[test]
fn the_reconcile_trigger_converges_and_races_the_loop_safely() {
    let mut world = world();
    if let Some(mut server) = world.server.take() {
        let _ = server.kill();
        let _ = server.wait();
    }
    // The loop effectively out of the way: a huge interval.
    spawn_server(&mut world, &["--reconcile-interval-ms", "3600000"]);
    let etag = awaiting(&world, "BKG-TRIGGER");
    let effect = "EFF-BKG-TRIGGER-BOOK-2";
    arm_fault(&world, effect, "create", "drop_response");
    let reply = call(
        &world,
        "POST",
        "/booking-intents/BKG-TRIGGER/behaviours/book",
        LUCY,
        Some(&etag),
        None,
    );
    assert_eq!(reply.status, 202);

    // A present If-Match on the trigger is a false belief, refused.
    let reply = call(
        &world,
        "POST",
        "/booking-intents/BKG-TRIGGER/behaviours/reconcile",
        LUCY,
        Some("\"3\""),
        None,
    );
    assert_eq!(reply.status, 400);

    // Wait out the (100ms) cadence, then trigger until settled — bounded.
    let start = std::time::Instant::now();
    loop {
        let reply = call(
            &world,
            "POST",
            "/booking-intents/BKG-TRIGGER/behaviours/reconcile",
            LUCY,
            None,
            None,
        );
        assert_eq!(reply.status, 200, "{:?}", reply.body);
        let read = call(
            &world,
            "GET",
            "/booking-intents/BKG-TRIGGER",
            LUCY,
            None,
            None,
        );
        if read.body["state"] == "Booked" {
            break;
        }
        assert!(
            start.elapsed() < std::time::Duration::from_secs(10),
            "the trigger never converged: {:?}",
            read.body
        );
        std::thread::sleep(std::time::Duration::from_millis(60));
    }
    assert_eq!(
        council_count(&world, "SELECT COUNT(*) FROM bookings"),
        1,
        "spamming the trigger duplicated nothing — the claims arbitrate"
    );

    // Settled means nothing to attend: the trigger reports an empty turn and
    // moves nothing (battery audit — over the wire, not only at the facade).
    let settled = snapshot(&world, "BKG-TRIGGER");
    let reply = call(
        &world,
        "POST",
        "/booking-intents/BKG-TRIGGER/behaviours/reconcile",
        LUCY,
        None,
        None,
    );
    assert_eq!(reply.status, 200);
    assert_eq!(reply.body["attended"], serde_json::json!([]));
    assert_inert(
        &world,
        "BKG-TRIGGER",
        &settled,
        "reconcile on a settled booking",
    );
}

/// The loop survives its process: kill the server mid-chase, restart over the
/// same files, and the chase resumes to convergence — the store is the queue.
#[test]
fn the_chase_survives_a_server_restart() {
    let mut world = world();
    let etag = awaiting(&world, "BKG-RESTART");
    let effect = "EFF-BKG-RESTART-BOOK-2";
    arm_fault(&world, effect, "create", "drop_response");
    let reply = call(
        &world,
        "POST",
        "/booking-intents/BKG-RESTART/behaviours/book",
        LUCY,
        Some(&etag),
        None,
    );
    assert_eq!(reply.status, 202);

    // The server dies before the loop can converge it.
    if let Some(mut server) = world.server.take() {
        let _ = server.kill();
        let _ = server.wait();
    }
    spawn_server(&mut world, &[]);
    let converged = poll_until(
        &world,
        "BKG-RESTART",
        std::time::Duration::from_secs(10),
        "Booked",
    );
    assert_eq!(converged.status, 200);
    assert_eq!(council_count(&world, "SELECT COUNT(*) FROM bookings"), 1);
}

/// Menu honesty over the wire, both directions, for every state the journey
/// reaches: everything listed maps to a non-409; everything unlisted maps to
/// 409 — including a terminal state at its MATCHING version.
// One long sweep, deliberately: four states times seven behaviours, each on a
// fresh booking so no probe advances the state under the others.
#[allow(clippy::too_many_lines)]
#[test]
fn the_menu_never_lies_in_either_direction() {
    let world = world();
    let all = [
        "select-venue",
        "verify-slot",
        "change-venue",
        "update-requirements",
        "revalidate-venue",
        "book",
        "cancel",
    ];
    let body_for = |behaviour: &str| -> Option<serde_json::Value> {
        match behaviour {
            "select-venue" => Some(serde_json::json!({"venue_id": "TH-A", "slot_id": "SLOT-A"})),
            "update-requirements" => Some(serde_json::json!({})),
            "cancel" => Some(serde_json::json!({"reason": "menu sweep"})),
            _ => None,
        }
    };

    // Walk the journey; at each state, probe EVERY behaviour on a throwaway
    // clone of the booking (server-side state cannot be forked, so the sweep
    // uses distinct bookings driven to the same state).
    let mut sweep = 0u32;
    for stop_at in ["Draft", "VenueSelected", "AwaitingBooking", "Booked"] {
        sweep += 1;
        let id = format!("BKG-MENU-{sweep}");
        let created = call(
            &world,
            "POST",
            "/booking-intents",
            LUCY,
            None,
            Some(&create_body(&id)),
        );
        let mut etag = etag_version(&created);
        let steps: &[(&str, Option<serde_json::Value>)] = match stop_at {
            "Draft" => &[],
            "VenueSelected" => &[(
                "select-venue",
                Some(serde_json::json!({"venue_id": "TH-A", "slot_id": "SLOT-A"})),
            )],
            "AwaitingBooking" => &[
                (
                    "select-venue",
                    Some(serde_json::json!({"venue_id": "TH-A", "slot_id": "SLOT-A"})),
                ),
                ("verify-slot", None),
            ],
            _ => &[
                (
                    "select-venue",
                    Some(serde_json::json!({"venue_id": "TH-A", "slot_id": "SLOT-A"})),
                ),
                ("verify-slot", None),
                ("book", None),
            ],
        };
        for (behaviour, body) in steps {
            let reply = call(
                &world,
                "POST",
                &format!("/booking-intents/{id}/behaviours/{behaviour}"),
                LUCY,
                Some(&etag),
                body.as_ref(),
            );
            assert_eq!(reply.status, 200, "{stop_at}/{behaviour}: {:?}", reply.body);
            etag = etag_version(&reply);
        }
        let projection = call(
            &world,
            "GET",
            &format!("/booking-intents/{id}"),
            LUCY,
            None,
            None,
        );
        assert_eq!(projection.body["state"], stop_at);
        let menu: Vec<String> = projection.body["available_behaviours"]
            .as_array()
            .expect("menu")
            .iter()
            .map(|value| {
                value
                    .as_str()
                    .expect("name")
                    .split(|c: char| c.is_uppercase())
                    .count();
                value.as_str().expect("name").to_owned()
            })
            .collect();

        for behaviour in all {
            // The route name maps to the proposal name.
            let proposal_name = match behaviour {
                "select-venue" => "SelectVenue",
                "verify-slot" => "VerifySlot",
                "change-venue" => "ChangeVenue",
                "update-requirements" => "UpdateRequirements",
                "revalidate-venue" => "RevalidateVenue",
                "book" => "Book",
                "cancel" => "Cancel",
                _ => unreachable!(),
            };
            let listed = menu.iter().any(|name| name == proposal_name);
            // Probe on a FRESH clone so the sweep never advances the state:
            // cancel/select would commit. Each probe uses its own booking.
            sweep += 1;
            let probe_id = format!("BKG-MENU-{sweep}");
            let created = call(
                &world,
                "POST",
                "/booking-intents",
                LUCY,
                None,
                Some(&create_body(&probe_id)),
            );
            let mut probe_etag = etag_version(&created);
            for (step, body) in steps {
                let reply = call(
                    &world,
                    "POST",
                    &format!("/booking-intents/{probe_id}/behaviours/{step}"),
                    LUCY,
                    Some(&probe_etag),
                    body.as_ref(),
                );
                probe_etag = etag_version(&reply);
            }
            let reply = call(
                &world,
                "POST",
                &format!("/booking-intents/{probe_id}/behaviours/{behaviour}"),
                LUCY,
                Some(&probe_etag),
                body_for(behaviour).as_ref(),
            );
            if listed {
                assert_ne!(
                    reply.status, 409,
                    "{stop_at}: {behaviour} is on the menu and must not be Undefined"
                );
            } else {
                assert_eq!(
                    reply.status, 409,
                    "{stop_at}: {behaviour} is NOT on the menu and must be 409, got {} {:?}",
                    reply.status, reply.body
                );
            }
        }
    }
}

/// Headers earn their keep: X-Request-ID echoed and minted; Idempotency-Key
/// accepted and inert; the browse catalogue filters.
#[test]
fn headers_and_browse_behave() {
    let world = world();
    // Echoed.
    let reply = http()
        .get(format!("{}/venues", world.server_url))
        .header("authorization", LUCY)
        .header("x-request-id", "req-mine-7")
        .send()
        .expect("answer");
    assert_eq!(
        reply
            .headers()
            .get("x-request-id")
            .unwrap()
            .to_str()
            .unwrap(),
        "req-mine-7"
    );
    // Minted.
    let reply = call(&world, "GET", "/venues", LUCY, None, None);
    assert!(reply.request_id.expect("minted").starts_with("req-"));

    // Idempotency-Key accepted and INERT — proven, not narrated (battery
    // audit): the same key does not dedupe a different id, and a different
    // key grants no replay of the same id.
    let with_key = |key: &str, body: &serde_json::Value| {
        http()
            .post(format!("{}/booking-intents", world.server_url))
            .header("authorization", LUCY)
            .header("idempotency-key", key)
            .json(body)
            .send()
            .expect("answer")
            .status()
            .as_u16()
    };
    assert_eq!(with_key("key-1", &create_body("BKG-IDEM")), 201);
    assert_eq!(
        with_key("key-1", &create_body("BKG-IDEM-2")),
        201,
        "the same key deduplicates nothing — a second id is a second booking"
    );
    assert_eq!(
        with_key("key-2", &create_body("BKG-IDEM")),
        409,
        "a fresh key replays nothing — the duplicate id is still a duplicate"
    );

    // Browse filters: TH-D (capacity 12) drops out for 20 attendees; TH-B
    // (inaccessible) drops out for accessible=true; TH-C (£90) drops out
    // under a 5000p ceiling — leaving exactly TH-A.
    let reply = call(
        &world,
        "GET",
        "/venues?attendees=20&accessible=true&max_fee_pence=5000",
        LUCY,
        None,
        None,
    );
    assert_eq!(reply.status, 200);
    let venues = reply.body["venues"].as_array().expect("rows");
    assert_eq!(venues.len(), 1, "{venues:?}");
    assert_eq!(venues[0]["venue_id"], "TH-A");
    assert_eq!(reply.body["browse_only"], true);
    // And the slot read carries no grant, ever.
    let reply = call(&world, "GET", "/venues/TH-A/slots/SLOT-A", LUCY, None, None);
    assert_eq!(reply.status, 200);
    assert!(
        reply.body.get("grant").is_none(),
        "grants never ride responses"
    );
}

/// GET mid-escalation (ADR-019's deliberate shape, documented and pinned):
/// 200, the in-flight state, the SAME `ETag` as before escalation — the
/// pursuit projection is M6's, and this test is the contract until then.
#[test]
fn a_booking_mid_escalation_reads_as_its_inflight_truth() {
    let mut world = world();
    if let Some(mut server) = world.server.take() {
        let _ = server.kill();
        let _ = server.wait();
    }
    // Loop parked; short cadence so five failed attempts fit the deadline.
    spawn_server(&mut world, &["--reconcile-interval-ms", "3600000"]);
    let etag = awaiting(&world, "BKG-ESC");
    // Book with the answer eaten (the Book turn itself needs the availability
    // read, so the council must be ALIVE for it — killing it first would
    // honestly answer 503, which is its own test elsewhere)…
    arm_fault(&world, "EFF-BKG-ESC-BOOK-2", "create", "drop_response");
    let reply = call(
        &world,
        "POST",
        "/booking-intents/BKG-ESC/behaviours/book",
        LUCY,
        Some(&etag),
        None,
    );
    assert_eq!(reply.status, 202, "{:?}", reply.body);
    let etag_before = etag_version(&reply);

    // …then the council dies, so every ask errs and the budget spends.
    let _ = world.council.kill();
    let _ = world.council.wait();

    // Spend the budget through the trigger (bounded wall-clock).
    let start = std::time::Instant::now();
    let mut escalated = false;
    while !escalated {
        let reply = call(
            &world,
            "POST",
            "/booking-intents/BKG-ESC/behaviours/reconcile",
            LUCY,
            None,
            None,
        );
        assert_eq!(reply.status, 200);
        escalated = reply.body["attended"]
            .as_array()
            .is_some_and(|outcomes| outcomes.iter().any(|outcome| outcome == "Escalated"));
        assert!(
            start.elapsed() < std::time::Duration::from_secs(15),
            "never escalated: {:?}",
            reply.body
        );
        if !escalated {
            std::thread::sleep(std::time::Duration::from_millis(120));
        }
    }

    let read = call(&world, "GET", "/booking-intents/BKG-ESC", LUCY, None, None);
    assert_eq!(read.status, 200);
    assert_eq!(
        read.body["state"], "BookingInProgress",
        "the state is the story"
    );
    assert_eq!(
        etag_version(&read),
        etag_before,
        "escalation never touches the aggregate: same version, same ETag"
    );
}
