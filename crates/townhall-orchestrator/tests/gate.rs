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

    // The complete ordered schedule. Fewer requests than the plan's table —
    // each turn's RESPONSE carries the fresh version, so no separate reload GET
    // precedes a proposal that follows one (the reload rule is satisfied by
    // reading the server's own last answer); and every freeform turn reads the
    // cancellable set as the proposer's projected context. Named in the
    // acceptance doc as the deviation it is.
    let id = "sms-"; // the derived id's prefix — the exact digest varies by turn number
    let expected: Vec<(&str, &str)> = vec![
        // BOOK
        ("GET", "/booking-intents?cancellable=true"), // proposer context
        ("POST", "/booking-intents"),                 // create, 201
        ("GET", "/venues"),
        ("POST", "/behaviours/select-venue"),
        ("POST", "/behaviours/verify-slot"),
        // STATUS
        ("GET", "/booking-intents/sms-"),
        // CONFIRM
        ("GET", "/booking-intents?cancellable=true"), // proposer context
        ("GET", "/booking-intents/sms-"),             // the walk's reload
        ("POST", "/behaviours/book"),
        ("GET", "/booking-intents/sms-"), // the outcome read for the reply text
        // STATUS
        ("GET", "/booking-intents/sms-"),
        // cancel it
        ("GET", "/booking-intents?cancellable=true"), // proposer context
        ("GET", "/booking-intents?cancellable=true"), // the referent resolution
        ("POST", "/behaviours/cancel"),
    ];
    let requests = proxy.requests();
    assert_eq!(
        requests.len(),
        expected.len(),
        "the schedule is exact — an extra request is drift, a missing one is a \
         skipped authority: {requests:#?}"
    );
    for (line, (method, fragment)) in requests.iter().zip(expected.iter()) {
        assert!(
            line.starts_with(method) && line.contains(fragment),
            "schedule diverged: expected {method} …{fragment}…, got {line:?}\n{requests:#?}"
        );
    }
    let _ = id;

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

    // CONFIRM under the fault: acknowledgement now, outcome after !followups.
    let faulted = journey::Script::parse(
        "> +447700900123 CONFIRM\n\
         < Booking now.\n\
         !followups\n\
         < Booked. Council ref",
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
    // The follow-up was an Automated message, not a reply.
    let outbox = channel.outbox();
    let last = outbox.last().expect("the outcome");
    assert_eq!(last.class, townhall_channel::OutboundClass::Automated);
    assert_eq!(council_count(&world, "SELECT COUNT(*) FROM bookings"), 1);
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
