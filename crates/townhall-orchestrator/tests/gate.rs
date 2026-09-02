//! B1 — **M6's gate**: the scripted SMS conversation creates, reads and cancels
//! a booking with no real telecom and no LLM.
//!
//! The script is `services/sms-simulator/scripts/lucy-journey.txt` — the same
//! file the demo binary runs, through the same `journey::run` — and the
//! recording proxy asserts the complete ordered request schedule, so an extra
//! speculative read fails as loudly as a missing one.

use bld_types::PrincipalId;
use std::sync::Arc;
use townhall_channel::{ChannelAddress, ChannelConfig, Region, SmsSimulator, SuppressionStore};
use townhall_gateway::Gateway;
use townhall_orchestrator::{
    CredentialSource, Dispatcher, FileSuppression, GatewayFactory, NoLedgerYet, PrincipalDirectory,
    ScriptedProposer, journey,
};
use townhall_testkit::{LUCY, RecordingProxy, arm_fault, council_count, fault_fired, world};

struct DevDirectory;

impl PrincipalDirectory for DevDirectory {
    fn resolve(&self, address: &ChannelAddress) -> Option<PrincipalId> {
        (address.revealed() == "+447700900123").then(|| PrincipalId::new("lucy"))
    }
}

struct DevCredentials;

impl CredentialSource for DevCredentials {
    fn token_for(&self, principal: &PrincipalId) -> Option<String> {
        (principal.as_str() == "lucy").then(|| "dev-lucy".to_owned())
    }
}

fn script() -> journey::Script {
    let path = std::path::Path::new(env!("CARGO_MANIFEST_DIR"))
        .join("../../services/sms-simulator/scripts/lucy-journey.txt");
    let text = std::fs::read_to_string(path).expect("the journey script exists");
    journey::Script::parse(&text).expect("the script parses")
}

fn dispatcher_against(
    base: &str,
    dir: &std::path::Path,
) -> (Dispatcher<SmsSimulator>, Arc<SmsSimulator>) {
    let suppression: Arc<dyn SuppressionStore> =
        Arc::new(FileSuppression::open(dir.join("stop.list")).expect("store"));
    let channel = Arc::new(SmsSimulator::new(
        ChannelConfig::default(),
        Arc::clone(&suppression),
    ));
    let dispatcher = Dispatcher::new(
        Arc::clone(&channel),
        Arc::new(DevDirectory),
        Arc::new(DevCredentials),
        Arc::new(NoLedgerYet),
        Arc::new(ScriptedProposer),
        suppression,
        Arc::new(GatewayFactory {
            base: base.to_owned(),
        }),
    );
    (dispatcher, channel)
}

/// The clean run: the council answers, every outcome settles synchronously, and
/// the wire schedule is EXACTLY this.
#[tokio::test]
async fn m6_gate_the_scripted_journey_clean() {
    let world = world();
    let proxy = RecordingProxy::in_front_of(&world.server_url);
    let dir = world.council_db.parent().expect("dir").to_path_buf();
    let (dispatcher, channel) = dispatcher_against(&proxy.url, &dir);

    journey::run(&dispatcher, &channel, &script(), Region::Gb)
        .await
        .expect("the journey completes");

    // The complete ordered schedule, as FULL request lines — the review found
    // the first version matching fragments, under which a changed query shape
    // or a request against the wrong booking still passed. The derived id is
    // extracted from the trace itself and every line is compared whole.
    let requests = proxy.requests();
    let id = requests
        .iter()
        .find_map(|line| {
            let start = line.find("/booking-intents/sms-")? + "/booking-intents/".len();
            // "sms-" plus 16 digest bytes as hex — the derived id's exact shape.
            line.get(start..start + 4 + 32).map(str::to_owned)
        })
        .expect("a derived id appears in the trace");
    let expected: Vec<String> = [
        // BOOK
        "GET /booking-intents?cancellable=true".to_owned(), // proposer context
        "POST /booking-intents".to_owned(),                 // create, 201
        "GET /venues".to_owned(),
        format!("POST /booking-intents/{id}/behaviours/select-venue"),
        format!("POST /booking-intents/{id}/behaviours/verify-slot"),
        // STATUS
        format!("GET /booking-intents/{id}"),
        // CONFIRM
        "GET /booking-intents?cancellable=true".to_owned(), // proposer context
        format!("GET /booking-intents/{id}"),               // the walk's reload
        format!("POST /booking-intents/{id}/behaviours/book"),
        format!("GET /booking-intents/{id}"), // the outcome read for the reply
        // STATUS
        format!("GET /booking-intents/{id}"),
        // cancel it
        "GET /booking-intents?cancellable=true".to_owned(), // proposer context
        "GET /booking-intents?cancellable=true".to_owned(), // referent resolution
        format!("POST /booking-intents/{id}/behaviours/cancel"),
    ]
    .into_iter()
    .collect();
    assert_eq!(
        requests, expected,
        "the wire schedule is exact — an extra request is drift, a missing one \
         is a skipped authority"
    );

    // The world agrees with the transcript: one council booking, cancelled.
    assert_eq!(council_count(&world, "SELECT COUNT(*) FROM bookings"), 1);
    assert_eq!(
        council_count(
            &world,
            "SELECT COUNT(*) FROM bookings WHERE cancelled_by IS NOT NULL"
        ),
        1
    );
}

/// The fault run: the book's answer is lost, so the outcome arrives as the
/// two-message shape — the immediate acknowledgement, then the automated
/// follow-up — and the behaviour is `POST`ed exactly once regardless.
#[tokio::test]
async fn m6_gate_the_scripted_journey_with_the_answer_lost() {
    let world = world();
    let proxy = RecordingProxy::in_front_of(&world.server_url);
    let dir = world.council_db.parent().expect("dir").to_path_buf();
    let (dispatcher, channel) = dispatcher_against(&proxy.url, &dir);

    // Walk to AwaitingBooking through the script's own opening.
    let opening = journey::Script::parse(
        "> +447700900123 BOOK date=2026-09-10 from=14:00 to=17:00 people=20 accessible=yes max=5000\n\
         < Maximum booking fee: £50.00.",
    )
    .expect("parses");
    journey::run(&dispatcher, &channel, &opening, Region::Gb)
        .await
        .expect("the opening completes");

    // Arm the drop against the booking the walk created.
    let gateway = Gateway::new(world.server_url.clone(), LUCY);
    let parked = gateway.cancellable().await.expect("lookup").remove(0);
    let effect = format!("EFF-{}-BOOK-{}", parked.id, parked.version);
    let fault = arm_fault(&world, &effect, "create", "drop_response").await;

    // CONFIRM under the fault: the acknowledgement is a REPLY now, the outcome
    // an AUTOMATED message after the follow-up turn — asserted as classes, not
    // just words, since a "Booking now." that arrived as Automated would be
    // silenceable by a STOP it has no business obeying.
    let faulted = journey::Script::parse(
        "> +447700900123 CONFIRM\n\
         < Booking now.\n\
         !followups\n\
         <! Booked. Council ref",
    )
    .expect("parses");
    journey::run(&dispatcher, &channel, &faulted, Region::Gb)
        .await
        .expect("the fault leg completes");

    assert_eq!(
        fault_fired(&world, fault).await,
        1,
        "the drop genuinely fired"
    );
    assert_eq!(
        proxy.count("POST", "/behaviours/book"),
        1,
        "the chase owns the effect — convergence never re-POSTs: {:?}",
        proxy.requests()
    );
    // The journey continues past the fault — STATUS reads the settled truth,
    // and the cancellation takes the same lost-answer path, so both halves of
    // the two-message shape are exercised end to end.
    let booked = gateway
        .read(&bld_types::BookingId::new(parked.id.clone()))
        .await
        .expect("read");
    let cancel_effect = format!("EFF-{}-CANCEL-{}", booked.id, booked.version);
    let cancel_fault = arm_fault(&world, &cancel_effect, "cancel", "drop_response").await;

    let closing = journey::Script::parse(
        "> +447700900123 STATUS\n\
         < Booked. Attendees 20. Council ref\n\
         > +447700900123 cancel it\n\
         < Cancelling now.\n\
         !followups\n\
         <! Cancelled. Council ref",
    )
    .expect("parses");
    journey::run(&dispatcher, &channel, &closing, Region::Gb)
        .await
        .expect("the closing completes");
    assert_eq!(fault_fired(&world, cancel_fault).await, 1);

    assert_eq!(
        proxy.count("POST", "/behaviours/cancel"),
        1,
        "one cancel POST, converged, never re-POSTed: {:?}",
        proxy.requests()
    );
    assert_eq!(
        council_count(
            &world,
            "SELECT COUNT(*) FROM bookings WHERE cancelled_by IS NOT NULL"
        ),
        1,
        "the council record ends cancelled"
    );
}

/// The moved-world variant of the clean gate — the leg the reviewed plan's
/// schedule contained and the demo script cannot: an out-of-band bump is a TEST
/// actor's move, not a message, so it lives here rather than in the script the
/// binary runs. STATUS must show the moved world; the walk must follow the
/// reloaded menu through revalidate and verify.
#[tokio::test]
async fn m6_gate_the_journey_with_the_world_moving_under_it() {
    let world = world();
    let proxy = RecordingProxy::in_front_of(&world.server_url);
    let dir = world.council_db.parent().expect("dir").to_path_buf();
    let (dispatcher, channel) = dispatcher_against(&proxy.url, &dir);

    let opening = journey::Script::parse(
        "> +447700900123 BOOK date=2026-09-10 from=14:00 to=17:00 people=20 accessible=yes max=5000\n\
         < Maximum booking fee: £50.00.\n\
         > +447700900123 STATUS\n\
         < AwaitingBooking. Attendees 20.",
    )
    .expect("parses");
    journey::run(&dispatcher, &channel, &opening, Region::Gb)
        .await
        .expect("the opening completes");

    // The out-of-band bump: attendees 20 → 24, Lucy's own credential,
    // AwaitingBooking → NeedsRevalidation (the domain insists the changed
    // count re-checks capacity).
    let gateway = Gateway::new(world.server_url.clone(), LUCY);
    let parked = gateway.cancellable().await.expect("lookup").remove(0);
    let id = bld_types::BookingId::new(parked.id.clone());
    let bumped = gateway
        .propose_at(
            &id,
            parked.version,
            "update-requirements",
            Some(serde_json::json!({"attendees": 24})),
        )
        .await
        .expect("bump");
    let townhall_gateway::Turn::Committed {
        version: bumped_version,
        ..
    } = bumped
    else {
        panic!("the bump commits: {bumped:?}");
    };

    // STATUS shows the MOVED world — a cached reply says 20 and fails — and
    // CONFIRM walks revalidate → verify → book off the reload. Counted within
    // the post-bump window: verify-slot legitimately ran once in the OPENING
    // walk too, which is the plan's own two-verify shape.
    let walk_start = proxy.requests().len();
    let closing = journey::Script::parse(
        "> +447700900123 STATUS\n\
         < NeedsRevalidation. Attendees 24.\n\
         > +447700900123 CONFIRM\n\
         < Booked. Council ref\n\
         > +447700900123 cancel it\n\
         < Cancelled. Council ref",
    )
    .expect("parses");
    journey::run(&dispatcher, &channel, &closing, Region::Gb)
        .await
        .expect("the closing completes");

    // The walk's steps: one each, none duplicated (a stale submit would retry),
    // and the revalidate departs from the bumped version.
    let walk: Vec<String> = proxy.requests()[walk_start..].to_vec();
    for behaviour in ["revalidate-venue", "verify-slot", "book", "cancel"] {
        let path = format!("/behaviours/{behaviour}");
        assert_eq!(
            walk.iter()
                .filter(|line| line.starts_with("POST") && line.contains(&path))
                .count(),
            1,
            "{behaviour} exactly once in the post-bump walk: {walk:?}"
        );
    }
    let audit = gateway.audit(&id).await.expect("audit");
    assert!(
        audit.iter().any(|row| row.from_version == bumped_version),
        "a walk step departs from the bumped version {bumped_version}"
    );
    assert_eq!(
        council_count(
            &world,
            "SELECT COUNT(*) FROM bookings WHERE cancelled_by IS NOT NULL"
        ),
        1
    );
}

/// The demo binary runs the same script through the same runner — asserted by
/// actually running it, so the "demo and test are one file" claim is a fact
/// with an exit code rather than a sentence in a doc.
#[tokio::test]
async fn the_demo_binary_is_the_same_journey() {
    let world = world();
    let status = std::process::Command::new(env!("CARGO"))
        .args([
            "run",
            "--quiet",
            "-p",
            "sms-simulator",
            "--",
            "--server",
            &world.server_url,
            "--script",
        ])
        .arg(
            std::path::Path::new(env!("CARGO_MANIFEST_DIR"))
                .join("../../services/sms-simulator/scripts/lucy-journey.txt"),
        )
        .status()
        .expect("the binary runs");
    assert!(status.success(), "the demo diverged from its own script");
}
