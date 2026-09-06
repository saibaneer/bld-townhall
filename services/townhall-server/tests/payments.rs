#![cfg(feature = "dev-authority")]

//! M10's human-payment handoff, over the real wire (spec §17, ADR-030).
//!
//! Three binaries, all spawned: `mock-council` (the world), `mock-stripe` (the
//! Checkout double) and `townhall-server --enable-payments`, driven with HTTP.
//! Nothing here is a unit-test stand-in — the server verifies real HMAC
//! signatures, maps real `payment_intents` rows, and books at the real council
//! double.
//!
//! The load-bearing witness is [`a_forged_webhook_advances_nothing`]: a webhook
//! signed with the wrong secret is a 400 and changes NO state. If the handler
//! skipped verification, that test would advance the booking and fail. And
//! [`a_completed_but_unpaid_webhook_does_not_advance`] holds the other half —
//! a verified event is only PAID evidence when Stripe says `paid`. Everything
//! else proves the handoff works; those two prove it cannot be forged or faked.
//!
//! Wall-clock honesty: the server runs on a clock no test can move, so the two
//! convergence points (reaching payment, and booking after payment) use a SHORT
//! cadence and a bounded poll — the stated PLAN exception, not a sleep.

use std::io::BufRead as _;
use std::process::{Child, Command, Stdio};
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

use townhall_payment::{WebhookSecret, sign_webhook};

const KEY_HEX: &str = "0707070707070707070707070707070707070707070707070707070707070707";
const AUTHORITY_KEY_HEX: &str = "a7a7a7a7a7a7a7a7a7a7a7a7a7a7a7a7a7a7a7a7a7a7a7a7a7a7a7a7a7a7a7a7";
const LUCY: &str = "Bearer dev-lucy";

/// The endpoint secret the server verifies against and the test signs with —
/// ONE value, passed to the child as `STRIPE_WEBHOOK_SECRET` and used by
/// `sign_webhook`. A test that used a different one here would fail the happy
/// path, so this shared constant is what keeps the forged-webhook witness honest.
const WHSEC: &str = "whsec_e2e_town_hall_endpoint_secret";
/// mock-stripe ignores the key, but the server refuses to start without one.
const STRIPE_KEY: &str = "sk_test_town_hall_e2e";
/// A threshold below every bookable slot, so Lucy's £45 TH-A routes to payment.
const PAY_THRESHOLD_PENCE: u64 = 3_000;

// ------------------------------------------------------------------ the world

struct World {
    dir: tempfile::TempDir,
    council: Child,
    council_url: String,
    council_db: std::path::PathBuf,
    stripe: Child,
    stripe_url: String,
    server: Option<Child>,
    server_url: String,
}

impl Drop for World {
    fn drop(&mut self) {
        if let Some(mut server) = self.server.take() {
            let _ = server.kill();
            let _ = server.wait();
        }
        let _ = self.stripe.kill();
        let _ = self.stripe.wait();
        let _ = self.council.kill();
        let _ = self.council.wait();
    }
}

/// Wait for `READY <port>` or fail loudly — a copy of the http.rs guard, for the
/// same reason: a harness that can hang turns an unknown bug into an unknowable
/// one. Thirty seconds is generous for a process that prints one line after bind.
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

    let (sender, receiver) = mpsc::channel();
    std::thread::spawn(move || {
        let mut lines = std::io::BufReader::new(stdout).lines();
        let first = lines.next().map(|line| line.expect("readable stdout"));
        let _ = sender.send(first);
        for _ in lines {}
    });

    match receiver.recv_timeout(Duration::from_secs(30)) {
        Ok(Some(line)) => {
            let port = line
                .strip_prefix("READY ")
                .unwrap_or_else(|| panic!("expected READY, got {line:?}"))
                .parse()
                .expect("a port");
            (child, port)
        }
        Ok(None) => {
            let _ = child.wait();
            let mut said = String::new();
            let _ = stderr.read_to_string(&mut said);
            panic!("the child exited before READY. stderr:\n{said}");
        }
        Err(_) => {
            let _ = child.kill();
            let _ = child.wait();
            let mut said = String::new();
            let _ = stderr.read_to_string(&mut said);
            panic!("no READY within 30s from a live child. stderr so far:\n{said}");
        }
    }
}

fn build_binaries() {
    for (package, features) in [("mock-council", "test-faults"), ("mock-stripe", "")] {
        let mut args = vec!["build", "-p", package];
        if !features.is_empty() {
            args.extend(["--features", features]);
        }
        let status = Command::new(env!("CARGO"))
            .args(&args)
            .status()
            .unwrap_or_else(|error| panic!("build {package}: {error}"));
        assert!(status.success(), "build {package}");
    }
}

fn target_binary(name: &str) -> std::path::PathBuf {
    std::path::Path::new(env!("CARGO_MANIFEST_DIR"))
        .join("../../target/debug")
        .join(name)
}

fn spawn_council(dir: &std::path::Path) -> (Child, String, std::path::PathBuf) {
    let db = dir.join("council.sqlite");
    let mut command = Command::new(target_binary("mock-council"));
    command
        .arg("--db")
        .arg(&db)
        .args(["--key-hex", KEY_HEX, "--port", "0"]);
    let (child, port) = spawn_ready(command);
    (child, format!("http://127.0.0.1:{port}"), db)
}

fn spawn_stripe() -> (Child, String) {
    let mut command = Command::new(target_binary("mock-stripe"));
    command.args(["--port", "0"]);
    let (child, port) = spawn_ready(command);
    (child, format!("http://127.0.0.1:{port}"))
}

/// Spawn the payments-enabled server. The two Stripe secrets go through the
/// ENVIRONMENT (never argv — a secret in `ps` is the secret), exactly as the
/// composition root reads them. `threshold_pence` of `None` leaves the default
/// £100 policy, so a below-threshold journey books directly.
fn spawn_server(world: &mut World, threshold_pence: Option<u64>) {
    let mut command = Command::new(env!("CARGO_BIN_EXE_townhall-server"));
    command
        .arg("--db")
        .arg(world.dir.path().join("townhall.sqlite"))
        .arg("--denials-db")
        .arg(world.dir.path().join("denials.sqlite"))
        .args(["--council-url", &world.council_url])
        .args(["--key-hex", KEY_HEX, "--port", "0", "--dev-authority"])
        .args(["--authority-key", AUTHORITY_KEY_HEX])
        .args(["--retry-cadence-ms", "100"])
        .args(["--reconcile-interval-ms", "50"])
        .arg("--enable-payments")
        // The Stripe secrets and the mock's base URL — per-child, so the value
        // never leaks into this test process's own environment.
        .env("STRIPE_SECRET_KEY", STRIPE_KEY)
        .env("STRIPE_WEBHOOK_SECRET", WHSEC)
        .env("STRIPE_BASE_URL", &world.stripe_url)
        .env("STRIPE_SUCCESS_URL", "https://townhall.test/paid")
        .env("STRIPE_CANCEL_URL", "https://townhall.test/cancelled");
    if let Some(pence) = threshold_pence {
        command.args(["--payment-threshold-pence", &pence.to_string()]);
    }
    let (child, port) = spawn_ready(command);
    world.server = Some(child);
    world.server_url = format!("http://127.0.0.1:{port}");
}

fn paying_world(threshold_pence: Option<u64>) -> World {
    build_binaries();
    let dir = tempfile::tempdir().expect("tempdir");
    let (council, council_url, council_db) = spawn_council(dir.path());
    let (stripe, stripe_url) = spawn_stripe();
    let mut world = World {
        dir,
        council,
        council_url,
        council_db,
        stripe,
        stripe_url,
        server: None,
        server_url: String::new(),
    };
    spawn_server(&mut world, threshold_pence);
    world
}

// ------------------------------------------------------------------ http drive

fn http() -> reqwest::blocking::Client {
    reqwest::blocking::Client::builder()
        .timeout(Duration::from_secs(15))
        .build()
        .expect("client")
}

struct Reply {
    status: u16,
    etag: Option<String>,
    body: serde_json::Value,
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

/// The booking a request is about — from the path, or from a create's body — so
/// the dev lane's `X-BLD-Delegation` (whose reference IS the booking id) is set.
fn booking_of(path: &str, body: Option<&serde_json::Value>) -> Option<String> {
    if let Some(rest) = path.strip_prefix("/booking-intents/") {
        let id = rest
            .split(['/', '?'])
            .next()
            .filter(|segment| !segment.is_empty())?;
        return Some(id.to_owned());
    }
    body?.get("id")?.as_str().map(str::to_owned)
}

fn call(
    world: &World,
    method: &str,
    path: &str,
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
    request = request.header("authorization", LUCY);
    request = request.header("x-bld-principal", "lucy");
    if let Some(booking) = booking_of(path, body) {
        request = request.header("x-bld-delegation", booking);
    }
    if let Some(version) = if_match {
        request = request.header("if-match", version);
    }
    if let Some(json) = body {
        request = request.json(json);
    }
    let response = request.send().expect("the server answers");
    let status = response.status().as_u16();
    let etag = response
        .headers()
        .get("etag")
        .and_then(|value| value.to_str().ok())
        .map(str::to_owned);
    let body = response.json().unwrap_or(serde_json::Value::Null);
    Reply { status, etag, body }
}

fn state_of(reply: &Reply) -> String {
    reply.body["state"].as_str().unwrap_or("").to_owned()
}

/// Drive one booking (TH-A, £45) from create through `book`. With the payment
/// threshold below £45 the `book` proposal routes through payment and lands in
/// `AwaitingHumanPayment`; with a higher threshold it books directly. Returns the
/// booking's projection after the `book` proposal settles.
fn drive_through_book(world: &World, id: &str) -> Reply {
    let created = call(
        world,
        "POST",
        "/booking-intents",
        None,
        Some(&create_body(id)),
    );
    assert_eq!(created.status, 201, "create: {:?}", created.body);
    let mut etag = created.etag.clone().expect("an ETag on create");
    for (behaviour, data) in [
        (
            "select-venue",
            Some(serde_json::json!({"venue_id": "TH-A", "slot_id": "SLOT-A"})),
        ),
        ("verify-slot", None),
        ("book", None),
    ] {
        let reply = call(
            world,
            "POST",
            &format!("/booking-intents/{id}/behaviours/{behaviour}"),
            Some(&etag),
            data.as_ref(),
        );
        // verify-slot and book reach outside: normally they settle synchronously
        // (200), but under load the coordinator can fall back to the async path
        // (202 Accepted), converged by the reconciler — both correct. Accept
        // either, wait for the booking to leave any in-flight state, then carry
        // that settled version forward. (Asserting a synchronous 200 here is what
        // made this setup flaky in CI.)
        assert!(
            reply.status == 200 || reply.status == 202,
            "{behaviour}: {:?}",
            reply.body
        );
        etag = settle(world, id).etag.clone().unwrap_or(etag);
    }
    settle(world, id)
}

fn read_booking(world: &World, id: &str) -> Reply {
    call(world, "GET", &format!("/booking-intents/{id}"), None, None)
}

/// The transient in-flight states a turn passes through while an external effect
/// is outstanding.
const IN_FLIGHT_STATES: &[&str] = &[
    "VerifyingSlot",
    "CheckoutPrepared",
    "BookingInProgress",
    "PaidBookingInProgress",
    "CancellingBooking",
    "CancellationRequested",
];

/// Read `id` until it is NOT in an in-flight state, then return that settled
/// projection — the load-robust way to await an outside-reaching step's result
/// (`AwaitingHumanPayment` is a settled waiting state, not in-flight).
fn settle(world: &World, id: &str) -> Reply {
    let start = Instant::now();
    loop {
        let reply = read_booking(world, id);
        if !IN_FLIGHT_STATES.contains(&state_of(&reply).as_str()) {
            return reply;
        }
        assert!(
            start.elapsed() < Duration::from_secs(10),
            "booking never left an in-flight state: {:?}",
            reply.body
        );
        std::thread::sleep(Duration::from_millis(50));
    }
}

fn poll_state(world: &World, id: &str, want: &str, deadline: Duration) -> String {
    let start = Instant::now();
    loop {
        let state = state_of(&read_booking(world, id));
        if state == want || start.elapsed() > deadline {
            return state;
        }
        std::thread::sleep(Duration::from_millis(50));
    }
}

// --------------------------------------------------------------- stripe / hook

/// The Stripe session id inside a Checkout URL (`…/cs_test_00000001`).
fn session_id_from_url(checkout_url: &str) -> String {
    checkout_url
        .rsplit('/')
        .next()
        .expect("a session id in the checkout URL")
        .to_owned()
}

/// GET the session mock-stripe created — the way real Stripe would hold it — to
/// learn the exact `metadata.payment_intent_id` the webhook must echo.
fn fetch_session(world: &World, session_id: &str) -> serde_json::Value {
    http()
        .get(format!(
            "{}/v1/checkout/sessions/{session_id}",
            world.stripe_url
        ))
        .send()
        .expect("mock-stripe answers")
        .json()
        .expect("a session JSON")
}

fn now_secs() -> i64 {
    i64::try_from(
        SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .expect("after the epoch")
            .as_secs(),
    )
    .expect("fits i64")
}

/// The bytes of a `checkout.session.completed` event for this session — exactly
/// the fields the handler reads: the session id, its `payment_intent_id`, and the
/// `payment_status` that decides whether it is genuinely PAID evidence (a real
/// completed card payment carries `"paid"`; a delayed method can complete a
/// session still `"unpaid"`).
fn completed_event(event_id: &str, session: &serde_json::Value, payment_status: &str) -> Vec<u8> {
    let body = serde_json::json!({
        "id": event_id,
        "type": "checkout.session.completed",
        "data": { "object": {
            "id": session["id"],
            "payment_status": payment_status,
            "metadata": { "payment_intent_id": session["metadata"]["payment_intent_id"] },
        }},
    });
    serde_json::to_vec(&body).expect("serializable event")
}

/// POST raw bytes to `/webhooks/stripe` with the given signature header, and
/// return the HTTP status. The body is sent verbatim (Stripe signs the bytes).
fn post_webhook(world: &World, raw_body: &[u8], signature: &str) -> u16 {
    http()
        .post(format!("{}/webhooks/stripe", world.server_url))
        .header("stripe-signature", signature)
        .header("content-type", "application/json")
        .body(raw_body.to_vec())
        .send()
        .expect("the server answers the webhook")
        .status()
        .as_u16()
}

fn valid_signature(raw_body: &[u8]) -> String {
    sign_webhook(&WebhookSecret::new(WHSEC), raw_body, now_secs())
}

// -------------------------------------------------------------------- witnesses

/// A high-value booking prepares the checkout and parks in `AwaitingHumanPayment`
/// with the human's link surfaced — the agent never touched money, and the state
/// carries the URL a person pays at.
#[test]
fn a_high_value_booking_awaits_human_payment_with_a_link() {
    let world = paying_world(Some(PAY_THRESHOLD_PENCE));
    let booking = drive_through_book(&world, "BKG-PAY-1");

    assert_eq!(
        state_of(&booking),
        "AwaitingHumanPayment",
        "£45 over a £30 threshold must route to payment: {:?}",
        booking.body
    );
    let url = booking.body["checkout_url"]
        .as_str()
        .expect("a checkout_url on AwaitingHumanPayment");
    assert!(
        url.starts_with("https://checkout.stripe.test/"),
        "the surfaced link is the Stripe Checkout URL: {url}"
    );
}

/// A genuinely-signed `checkout.session.completed` advances the booking all the
/// way to `Booked` — verified provider evidence, not an agent claim, is what
/// resumes the workflow, and the resume books at the council.
#[test]
fn a_valid_signed_webhook_advances_the_booking() {
    let world = paying_world(Some(PAY_THRESHOLD_PENCE));
    let booking = drive_through_book(&world, "BKG-PAY-2");
    assert_eq!(state_of(&booking), "AwaitingHumanPayment");

    let session_id = session_id_from_url(booking.body["checkout_url"].as_str().unwrap());
    let session = fetch_session(&world, &session_id);
    let raw = completed_event("evt_pay_2", &session, "paid");
    let status = post_webhook(&world, &raw, &valid_signature(&raw));
    assert_eq!(status, 200, "a verified event is accepted");

    let final_state = poll_state(&world, "BKG-PAY-2", "Booked", Duration::from_secs(10));
    assert_eq!(
        final_state, "Booked",
        "verified payment resumes the workflow and books at the council"
    );
    assert_eq!(
        council_count(&world, "SELECT COUNT(*) FROM bookings"),
        1,
        "exactly one council booking resulted"
    );
}

/// THE load-bearing witness. A webhook signed with the WRONG secret — the shape
/// an attacker who cannot know the endpoint secret can produce — is a 400 and
/// changes NOTHING: the booking is still awaiting payment, and nothing booked.
/// This is what makes "only verified evidence advances a payment" a fact and not
/// a hope: no valid signature, no advance.
///
/// Mutation check: delete the `verify_webhook` gate in the handler and this
/// test's booking advances past `AwaitingHumanPayment` — the assert fails.
/// (It witnesses that verification GATES the advance; the verify-BEFORE-parse
/// ordering is a separate property the handler's structure enforces.)
#[test]
fn a_forged_webhook_advances_nothing() {
    let world = paying_world(Some(PAY_THRESHOLD_PENCE));
    let booking = drive_through_book(&world, "BKG-PAY-3");
    assert_eq!(state_of(&booking), "AwaitingHumanPayment");

    let session_id = session_id_from_url(booking.body["checkout_url"].as_str().unwrap());
    let session = fetch_session(&world, &session_id);
    let raw = completed_event("evt_forged", &session, "paid");

    // A signature that is well-formed but computed with the WRONG secret — the
    // shape an attacker who cannot know the endpoint secret can produce.
    let forged = sign_webhook(
        &WebhookSecret::new("whsec_attacker_does_not_know_the_secret"),
        &raw,
        now_secs(),
    );
    let status = post_webhook(&world, &raw, &forged);
    assert_eq!(status, 400, "an unverified webhook is refused");

    // And nothing moved. Give the reconciler the same window the happy path used
    // to reach Booked, then confirm the booking is STILL awaiting payment.
    let state = poll_state(&world, "BKG-PAY-3", "Booked", Duration::from_secs(2));
    assert_eq!(
        state, "AwaitingHumanPayment",
        "a forged webhook must not advance the booking"
    );
    assert_eq!(
        council_count(&world, "SELECT COUNT(*) FROM bookings"),
        0,
        "and it must not have booked at the council"
    );
}

/// A verified `checkout.session.completed` that is NOT paid — a delayed / async
/// payment method that finished the session while the money is still settling —
/// is a 200 (Stripe should stop redelivering it), but advances NOTHING. "Resumes
/// only after verified payment evidence" (spec §17) means paid, not merely a
/// finished Checkout.
///
/// Mutation check: drop the `payment_status == "paid"` gate in the handler and
/// this booking advances to `Booked` on an unpaid completion — the assert fails.
#[test]
fn a_completed_but_unpaid_webhook_does_not_advance() {
    let world = paying_world(Some(PAY_THRESHOLD_PENCE));
    let booking = drive_through_book(&world, "BKG-PAY-5");
    assert_eq!(state_of(&booking), "AwaitingHumanPayment");

    let session_id = session_id_from_url(booking.body["checkout_url"].as_str().unwrap());
    let session = fetch_session(&world, &session_id);
    // A genuinely-signed completed event whose session is still UNPAID.
    let raw = completed_event("evt_unpaid", &session, "unpaid");
    let status = post_webhook(&world, &raw, &valid_signature(&raw));
    assert_eq!(
        status, 200,
        "a verified event is accepted (and here, parked)"
    );

    let state = poll_state(&world, "BKG-PAY-5", "Booked", Duration::from_secs(2));
    assert_eq!(
        state, "AwaitingHumanPayment",
        "an unpaid completion is not payment evidence — the booking must not advance"
    );
    assert_eq!(
        council_count(&world, "SELECT COUNT(*) FROM bookings"),
        0,
        "and nothing booked at the council"
    );
}

/// Stripe redelivers events; a second copy of a verified event must converge, not
/// double-book. Both deliveries are 200, and exactly one council booking exists.
#[test]
fn a_duplicate_webhook_is_idempotent() {
    let world = paying_world(Some(PAY_THRESHOLD_PENCE));
    let booking = drive_through_book(&world, "BKG-PAY-4");
    assert_eq!(state_of(&booking), "AwaitingHumanPayment");

    let session_id = session_id_from_url(booking.body["checkout_url"].as_str().unwrap());
    let session = fetch_session(&world, &session_id);
    let raw = completed_event("evt_pay_4", &session, "paid");

    // The same signed event, delivered twice.
    assert_eq!(post_webhook(&world, &raw, &valid_signature(&raw)), 200);
    let final_state = poll_state(&world, "BKG-PAY-4", "Booked", Duration::from_secs(10));
    assert_eq!(final_state, "Booked");
    assert_eq!(post_webhook(&world, &raw, &valid_signature(&raw)), 200);

    // Still one booking, still Booked — the redelivery converged.
    assert_eq!(state_of(&read_booking(&world, "BKG-PAY-4")), "Booked");
    assert_eq!(
        council_count(&world, "SELECT COUNT(*) FROM bookings"),
        1,
        "a redelivered event must not book twice"
    );
}

/// Payments ENABLED, but a fee below the configured threshold takes the ordinary
/// path: it books directly, with no checkout link and no payment involvement.
/// The default £100 threshold leaves TH-A's £45 under it.
#[test]
fn below_threshold_books_directly_with_payments_enabled() {
    let world = paying_world(None); // default £100 policy
    let booking = drive_through_book(&world, "BKG-DIRECT-1");

    assert_eq!(
        state_of(&booking),
        "Booked",
        "£45 under the £100 threshold books without payment: {:?}",
        booking.body
    );
    assert!(
        booking.body["checkout_url"].is_null(),
        "a directly-booked booking has no checkout link: {:?}",
        booking.body
    );
    assert_eq!(
        council_count(&world, "SELECT COUNT(*) FROM bookings"),
        1,
        "it booked at the council directly"
    );
}

// ------------------------------------------------------------------- sqlite peek

/// Count rows in the council's SQLite the way http.rs does — the external world's
/// own ledger, read directly, so "did it book?" is answered by the council, not
/// by the server we are testing.
fn council_count(world: &World, sql: &str) -> i64 {
    let output = Command::new("sqlite3")
        .arg(&world.council_db)
        .arg(sql)
        .output()
        .expect("sqlite3 runs");
    assert!(
        output.status.success(),
        "sqlite3: {}",
        String::from_utf8_lossy(&output.stderr)
    );
    String::from_utf8_lossy(&output.stdout)
        .trim()
        .parse()
        .unwrap_or(0)
}
