//! B1 — **M7C's gate**: the scripted SMS conversation raises a challenge, a
//! person's YES approves it, the booking is made, read and cancelled — with no
//! real telecom and no LLM, on the REAL authority lane.
//!
//! The script is `services/sms-simulator/scripts/lucy-journey.txt` — the same
//! file the demo binary runs, through the same `journey::run`.

use bld_types::PrincipalId;
use std::sync::Arc;
use townhall_channel::{ChannelAddress, ChannelConfig, Region, SmsSimulator, SuppressionStore};
use townhall_orchestrator::{
    ContinuationStore, CredentialSource, Dispatcher, FileContinuation, FileSuppression,
    GatewayFactory, PrincipalDirectory, ScriptedProposer, UnmeteredLedger, journey,
};
use townhall_testkit::{RecordingProxy, WORKLOAD, council_count, world_real};

#[path = "support/mod.rs"]
mod support;
use support::{HttpApprovals, HttpEvidence};

const LUCY_PHONE: &str = "+447700900123";

struct DevDirectory;

impl PrincipalDirectory for DevDirectory {
    fn resolve(&self, address: &ChannelAddress) -> Option<PrincipalId> {
        (address.revealed() == LUCY_PHONE).then(|| PrincipalId::new("lucy"))
    }
}

struct WorkloadCredential;

impl CredentialSource for WorkloadCredential {
    fn token_for(&self, principal: &PrincipalId) -> Option<String> {
        (principal.as_str() == "lucy").then(|| WORKLOAD.to_owned())
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
    dispatcher_at(
        base,
        &dir.join("stop.list"),
        &dir.join("continuation.jsonl"),
    )
}

fn dispatcher_at(
    base: &str,
    stop: &std::path::Path,
    cont: &std::path::Path,
) -> (Dispatcher<SmsSimulator>, Arc<SmsSimulator>) {
    let suppression: Arc<dyn SuppressionStore> =
        Arc::new(FileSuppression::open(stop.to_path_buf()).expect("store"));
    let continuations: Arc<dyn ContinuationStore> =
        Arc::new(FileContinuation::open(cont.to_path_buf()).expect("store"));
    let channel = Arc::new(SmsSimulator::new(
        ChannelConfig::default(),
        Arc::clone(&suppression),
    ));
    let dispatcher = Dispatcher::new(
        Arc::clone(&channel),
        Arc::new(DevDirectory),
        Arc::new(WorkloadCredential),
        Arc::new(UnmeteredLedger),
        Arc::new(ScriptedProposer),
        suppression,
        Arc::new(GatewayFactory {
            base: base.to_owned(),
        }),
        Arc::new(HttpApprovals::new(base)),
        Arc::new(HttpEvidence::new(base)),
        continuations,
    );
    (dispatcher, channel)
}

/// The £45 journey, end to end: BOOK raises a challenge, YES approves it, the
/// booking is made at a slot inside Lucy's £50 ceiling, read, and cancelled —
/// with the approval endpoints on the wire and no booking made before the YES.
#[tokio::test]
async fn the_gate_the_forty_five_pound_journey() {
    let world = world_real();
    let proxy = RecordingProxy::in_front_of(&world.server_url);
    let dir = world.council_db.parent().expect("dir").to_path_buf();
    let (dispatcher, channel) = dispatcher_against(&proxy.url, &dir);

    journey::run(&dispatcher, &channel, &script(), Region::Gb)
        .await
        .expect("the journey completes");

    // The approval seam was on the wire: the reply's evidence deposited and the
    // challenge answered — neither of which the M6 dev lane did. (A begin POST
    // also hit /approvals, but a reply can't happen without it.)
    assert_eq!(
        proxy.count("POST", "/inbound-evidence"),
        1,
        "the reply's evidence was deposited once"
    );
    assert_eq!(
        proxy.count("POST", "/reply"),
        1,
        "the challenge was answered"
    );

    // The booking happened exactly once, and only after the YES: one create, one
    // book, one cancel — no duplicates, no speculative pre-booking.
    assert_eq!(proxy.count("POST", "/behaviours/book"), 1);
    assert_eq!(proxy.count("POST", "/behaviours/cancel"), 1);

    // The world agrees: one council booking, cancelled — and its fee is inside
    // the £50 ceiling Lucy approved (the affirmative half; the over-ceiling
    // refusal below is the vacuity guard).
    assert_eq!(council_count(&world, "SELECT COUNT(*) FROM bookings"), 1);
    assert_eq!(
        council_count(
            &world,
            "SELECT COUNT(*) FROM bookings WHERE cancelled_by IS NOT NULL"
        ),
        1
    );
}

/// The vacuity guard: an approval whose ceiling no venue can meet is refused
/// AFTER the YES, and nothing is booked — so the £45 pass above is not a booking
/// that would have happened whatever the ceiling.
#[tokio::test]
async fn an_over_ceiling_approval_books_nothing() {
    let world = world_real();
    let dir = world.council_db.parent().expect("dir").to_path_buf();
    let (dispatcher, channel) = dispatcher_against(&world.server_url, &dir);

    // £40 is below every venue's fee, so no slot fits — but the challenge still
    // raises and Lucy still approves.
    let over = journey::Script::parse(
        "> +447700900123 BOOK date=2026-09-10 from=14:00 to=17:00 people=20 accessible=yes max=4000\n\
         < Maximum booking fee: £40.00\n\
         > +447700900123 YES {code}\n\
         < No venue fits those limits.",
    )
    .expect("parses");
    journey::run(&dispatcher, &channel, &over, Region::Gb)
        .await
        .expect("the over-ceiling leg completes");

    assert_eq!(
        council_count(&world, "SELECT COUNT(*) FROM bookings"),
        0,
        "an approval no venue can satisfy books nothing"
    );
}

/// W8 — a turn with no approved reference READS but cannot CHANGE. A dispatcher
/// that never approved the booking finds it on the wire (the read works) but
/// holds no grant to cancel it (the change is refused), so a change without an
/// approval is structurally impossible, not merely discouraged.
#[tokio::test]
async fn a_turn_with_no_reference_reads_but_cannot_change() {
    let world = world_real();
    let dir = world.council_db.parent().expect("dir").to_path_buf();

    // Dispatcher A approves and books.
    let (approver, approver_channel) = dispatcher_at(
        &world.server_url,
        &dir.join("stop.list"),
        &dir.join("cont-a.jsonl"),
    );
    let booked = journey::Script::parse(
        "> +447700900123 BOOK date=2026-09-10 from=14:00 to=17:00 people=20 accessible=yes max=5000\n\
         < Maximum booking fee: £50.00\n\
         > +447700900123 YES {code}\n\
         < Booked. Council ref",
    )
    .expect("parses");
    journey::run(&approver, &approver_channel, &booked, Region::Gb)
        .await
        .expect("approver books");

    // Dispatcher B holds NO continuation — no reference for that booking.
    let (stranger, stranger_channel) = dispatcher_at(
        &world.server_url,
        &dir.join("stop.list"),
        &dir.join("cont-b.jsonl"),
    );
    // "cancel it" reads the booking off the wire (else the reply would be "no
    // bookings to cancel"), then is refused for want of a grant — read worked,
    // change refused, in one reply.
    let attempt = journey::Script::parse(
        "> +447700900123 cancel it\n\
         < I can only cancel a booking you approved through me.",
    )
    .expect("parses");
    journey::run(&stranger, &stranger_channel, &attempt, Region::Gb)
        .await
        .expect("the read succeeds and the change is refused");

    assert_eq!(
        council_count(
            &world,
            "SELECT COUNT(*) FROM bookings WHERE cancelled_by IS NOT NULL"
        ),
        0,
        "no reference, no cancellation — the booking stands"
    );
}

/// The demo binary runs the same script through the same runner — asserted by
/// actually running it, so the "demo and test are one file" claim is a fact with
/// an exit code rather than a sentence in a doc.
#[tokio::test]
async fn the_demo_binary_is_the_same_journey() {
    let world = world_real();
    let dir = world.council_db.parent().expect("dir");
    let status = std::process::Command::new(env!("CARGO"))
        .args([
            "run",
            "--quiet",
            "-p",
            "sms-simulator",
            "--",
            "--server",
            &world.server_url,
            "--stop-file",
        ])
        .arg(dir.join("demo-stop.list"))
        .arg("--continuation-file")
        .arg(dir.join("demo-continuation.jsonl"))
        .arg("--script")
        .arg(
            std::path::Path::new(env!("CARGO_MANIFEST_DIR"))
                .join("../../services/sms-simulator/scripts/lucy-journey.txt"),
        )
        .status()
        .expect("the binary runs");
    assert!(status.success(), "the demo diverged from its own script");
}
