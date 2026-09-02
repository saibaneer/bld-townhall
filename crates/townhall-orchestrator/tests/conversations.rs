//! B2–B8, B10, B14, B15: whole conversations against the real server and
//! council, with the recording proxy counting what actually went over the wire.

use async_trait::async_trait;
use bld_types::PrincipalId;
use std::sync::Arc;
use std::sync::atomic::{AtomicUsize, Ordering};
use townhall_channel::{
    ChannelAddress, ChannelConfig, ChannelKind, HumanChannel as _, InboundIdentity, InboundMessage,
    MessageReceipt, OutboundClass, RawInbound, SmsSimulator, SuppressionStore, TransportEvidence,
};
use townhall_gateway::Gateway;
use townhall_orchestrator::{
    BookingWire, CredentialSource, Dispatcher, FileSuppression, GatewayFactory, NoLedgerYet,
    PrincipalDirectory, ProjectedContext, Proposed, Proposer, Request, ScriptedProposer,
    WireFactory,
};
use townhall_testkit::{LUCY, RecordingProxy, World, arm_fault, council_count, world};

const LUCY_PHONE: &str = "+447700900123";
const PRIYA_PHONE: &str = "+447700900456";

/// The dev bindings: addresses to principals, principals to the server's OWN
/// fixed allowlist — and nothing else, which is what keeps this source unable
/// to widen authority (asserted below).
struct DevDirectory;

impl PrincipalDirectory for DevDirectory {
    fn resolve(&self, address: &ChannelAddress) -> Option<PrincipalId> {
        match address.revealed() {
            LUCY_PHONE => Some(PrincipalId::new("lucy")),
            PRIYA_PHONE => Some(PrincipalId::new("priya")),
            _ => None,
        }
    }
}

struct DevCredentials;

impl DevCredentials {
    /// Everything this source can ever emit.
    const ALLOWLIST: [&'static str; 2] = ["dev-lucy", "dev-priya-nobook"];
}

impl CredentialSource for DevCredentials {
    fn token_for(&self, principal: &PrincipalId) -> Option<String> {
        match principal.as_str() {
            "lucy" => Some("dev-lucy".to_owned()),
            "priya" => Some("dev-priya-nobook".to_owned()),
            _ => None,
        }
    }
}

/// The credential source cannot mint: its whole output is the server's fixed
/// allowlist, enumerated. A source that could produce anything else would be an
/// authority issuer wearing a lookup's name.
#[test]
fn the_credential_source_is_bounded_by_the_allowlist() {
    let source = DevCredentials;
    for principal in ["lucy", "priya", "marco", "@orphan", ""] {
        if let Some(token) = source.token_for(&PrincipalId::new(principal)) {
            assert!(
                DevCredentials::ALLOWLIST.contains(&token.as_str()),
                "{token:?} is not on the server's allowlist"
            );
        }
    }
}

/// A wire factory that counts `converge` calls — B7b's witness that STOP skips
/// the TURN, not just its message.
struct CountingFactory {
    inner: GatewayFactory,
    converges: Arc<AtomicUsize>,
}

struct CountingConvergeWire {
    inner: Gateway,
    converges: Arc<AtomicUsize>,
}

#[async_trait]
impl BookingWire for CountingConvergeWire {
    async fn create(
        &self,
        id: &bld_types::BookingId,
        requirements: &bld_types::BookingRequirements,
    ) -> Result<townhall_gateway::Projection, townhall_gateway::GatewayError> {
        self.inner.create(id, requirements).await
    }
    async fn read(
        &self,
        id: &bld_types::BookingId,
    ) -> Result<townhall_gateway::Projection, townhall_gateway::GatewayError> {
        self.inner.read(id).await
    }
    async fn cancellable(
        &self,
    ) -> Result<Vec<townhall_gateway::Projection>, townhall_gateway::GatewayError> {
        self.inner.cancellable().await
    }
    async fn by_reference(
        &self,
        reference: &bld_types::CouncilBookingRef,
    ) -> Result<Vec<townhall_gateway::Projection>, townhall_gateway::GatewayError> {
        self.inner.by_reference(reference).await
    }
    async fn venues(
        &self,
    ) -> Result<Vec<townhall_gateway::VenueRow>, townhall_gateway::GatewayError> {
        self.inner.venues().await
    }
    async fn propose_at(
        &self,
        id: &bld_types::BookingId,
        expected_version: u64,
        behaviour: &str,
        body: Option<serde_json::Value>,
    ) -> Result<townhall_gateway::Turn, townhall_gateway::GatewayError> {
        self.inner
            .propose_at(id, expected_version, behaviour, body)
            .await
    }
    async fn converge(
        &self,
        id: &bld_types::BookingId,
        first_wait: std::time::Duration,
    ) -> Result<townhall_gateway::Projection, townhall_gateway::GatewayError> {
        self.converges.fetch_add(1, Ordering::SeqCst);
        self.inner.converge(id, first_wait).await
    }
}

impl WireFactory for CountingFactory {
    fn wire_for(&self, token: &str) -> Arc<dyn BookingWire> {
        let gateway = Gateway::new(self.inner.base.clone(), token);
        Arc::new(CountingConvergeWire {
            inner: gateway,
            converges: Arc::clone(&self.converges),
        })
    }
}

// ------------------------------------------------------------------ harness

struct Talk {
    dispatcher: Dispatcher<SmsSimulator>,
    channel: Arc<SmsSimulator>,
    counter: AtomicUsize,
    converges: Arc<AtomicUsize>,
    suppression_path: std::path::PathBuf,
    base: String,
}

fn talk(world: &World, base: &str) -> Talk {
    let suppression_path = world_suppression_path(world);
    talk_at(base, &suppression_path)
}

fn world_suppression_path(world: &World) -> std::path::PathBuf {
    // Anchor the suppression file to the world's temp dir so restarts within a
    // test share it and separate tests never do.
    world.council_db.parent().expect("dir").join("stop.list")
}

fn talk_at(base: &str, suppression_path: &std::path::Path) -> Talk {
    let suppression: Arc<dyn SuppressionStore> =
        Arc::new(FileSuppression::open(suppression_path.to_path_buf()).expect("suppression store"));
    let channel = Arc::new(SmsSimulator::new(
        ChannelConfig::default(),
        Arc::clone(&suppression),
    ));
    let converges = Arc::new(AtomicUsize::new(0));
    let dispatcher = Dispatcher::new(
        Arc::clone(&channel),
        Arc::new(DevDirectory),
        Arc::new(DevCredentials),
        Arc::new(NoLedgerYet),
        Arc::new(ScriptedProposer),
        suppression,
        Arc::new(CountingFactory {
            inner: GatewayFactory {
                base: base.to_owned(),
            },
            converges: Arc::clone(&converges),
        }),
    );
    Talk {
        dispatcher,
        channel,
        counter: AtomicUsize::new(0),
        converges,
        suppression_path: suppression_path.to_path_buf(),
        base: base.to_owned(),
    }
}

impl Talk {
    async fn say(&self, from: &str, body: &str) -> String {
        let n = self.counter.fetch_add(1, Ordering::SeqCst);
        let raw = RawInbound {
            identity: InboundIdentity::new("sim", "acct", format!("m-{from}-{n}")),
            channel: ChannelKind::SmsSimulator,
            from: from.to_owned(),
            body: body.to_owned(),
            received_at_ms: 0,
            evidence: TransportEvidence::new("sim", from, true),
        };
        self.dispatcher.handle(raw).await.expect("handled");
        self.last_text_to(from)
    }

    fn last_text_to(&self, from: &str) -> String {
        let address = ChannelAddress::parse(from, townhall_channel::Region::Gb).expect("address");
        self.channel
            .outbox()
            .iter()
            .rev()
            .find(|sent| sent.to == address)
            .map(|sent| sent.text.clone())
            .expect("a reply was sent")
    }

    /// Drive a fresh booking to `AwaitingBooking` for this phone; returns the
    /// preamble reply.
    async fn book(&self, from: &str) -> String {
        self.say(
            from,
            "BOOK date=2026-09-10 from=14:00 to=17:00 people=20 accessible=yes max=5000",
        )
        .await
    }

    fn lucy_gateway(&self) -> Gateway {
        Gateway::new(self.base.clone(), LUCY)
    }
}

// ------------------------------------------------------------------ B2 / B3

/// Ambiguous "cancel it" asks, naming both candidates, and submits nothing.
#[tokio::test]
async fn b2_ambiguous_cancel_asks_and_submits_nothing() {
    let world = world();
    let proxy = RecordingProxy::in_front_of(&world.server_url);
    let talk = talk(&world, &proxy.url);

    talk.book(LUCY_PHONE).await;
    talk.book(LUCY_PHONE).await; // a second, distinct message → distinct id

    // Snapshot the proxy AFTER the conversational setup: the setup necessarily
    // POSTs, and counting those would blur the observation window.
    let posts_before = proxy.count("POST", "/");
    let reply = talk.say(LUCY_PHONE, "cancel it").await;

    assert!(
        reply.contains("2 bookings") && reply.contains("CANCEL"),
        "the question names the choice: {reply}"
    );
    // Both candidates are NAMED — ids here, since neither is booked yet.
    let named = reply.matches("sms-").count();
    assert_eq!(named, 2, "both candidates appear in the question: {reply}");
    assert_eq!(
        proxy.count("POST", "/"),
        posts_before,
        "ambiguity must submit NOTHING: {:?}",
        proxy.requests()
    );
}

/// Unambiguous "cancel it" cancels — the pair that stops B2 passing by never
/// cancelling anything.
#[tokio::test]
async fn b3_unambiguous_cancel_cancels_the_one_booking() {
    let world = world();
    let proxy = RecordingProxy::in_front_of(&world.server_url);
    let talk = talk(&world, &proxy.url);

    talk.book(LUCY_PHONE).await;
    let confirmed = talk.say(LUCY_PHONE, "CONFIRM").await;
    assert!(
        confirmed.contains("Booked. Council ref"),
        "a clean confirm books in one outcome message: {confirmed}"
    );

    let posts_before = proxy.count("POST", "/behaviours/cancel");
    let reply = talk.say(LUCY_PHONE, "cancel it").await;
    assert!(reply.contains("Cancelled. Council ref"), "{reply}");
    assert_eq!(
        proxy.count("POST", "/behaviours/cancel"),
        posts_before + 1,
        "exactly one cancel POST"
    );
    assert_eq!(
        council_count(
            &world,
            "SELECT COUNT(*) FROM bookings WHERE cancelled_by IS NOT NULL"
        ),
        1,
        "the council's own record is cancelled"
    );
}

// ------------------------------------------------------------------ B4

/// Candidates survive a restart: sessions are memory, the wire is the truth.
#[tokio::test]
async fn b4_cancel_it_survives_a_session_wipe() {
    let world = world();
    let talk = talk(&world, &world.server_url);
    talk.book(LUCY_PHONE).await;

    // A NEW dispatcher: sessions gone, suppression file shared.
    let reborn = talk_at(&world.server_url, &talk.suppression_path);
    let reply = reborn.say(LUCY_PHONE, "cancel it").await;
    assert!(
        reply.contains("Cancelled"),
        "the candidate came from the wire, not from memory: {reply}"
    );
}

// ------------------------------------------------------------------ B5

/// Reload-before-propose: the world moves out-of-band, and the dispatcher's
/// next walk starts from the reload, not the memory.
#[tokio::test]
async fn b5_confirm_reloads_and_follows_the_menu() {
    let world = world();
    let proxy = RecordingProxy::in_front_of(&world.server_url);
    let talk = talk(&world, &proxy.url);

    talk.book(LUCY_PHONE).await; // → AwaitingBooking, version 2

    // Out-of-band: the TEST bumps requirements with Lucy's own credential.
    // AwaitingBooking + UpdateRequirements → NeedsRevalidation (the domain
    // insists a changed count re-checks capacity).
    let gateway = talk.lucy_gateway();
    let booking = gateway.cancellable().await.expect("lookup").remove(0);
    let id = bld_types::BookingId::new(booking.id.clone());
    let bumped = gateway
        .propose_at(
            &id,
            booking.version,
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

    let posts_before = proxy.requests().len();
    let reply = talk.say(LUCY_PHONE, "CONFIRM").await;
    assert!(
        reply.contains("Booked. Council ref"),
        "the walk follows the RELOADED menu to Booked: {reply}"
    );

    // No stale POST anywhere in the walk: revalidate, verify, book — one each,
    // and nothing was submitted twice (a stale-submit-then-retry shows up as a
    // duplicate here).
    let walk: Vec<String> = proxy.requests()[posts_before..].to_vec();
    for behaviour in ["revalidate-venue", "verify-slot", "book"] {
        let path = format!("/behaviours/{behaviour}");
        assert_eq!(
            walk.iter()
                .filter(|line| line.starts_with("POST") && line.contains(&path))
                .count(),
            1,
            "{behaviour} exactly once in {walk:?}"
        );
    }

    // And the revalidate's audit row starts FROM the bumped version — the
    // proof the walk began at the reload.
    let audit = gateway.audit(&id).await.expect("audit");
    assert!(
        audit.iter().any(|row| row.from_version == bumped_version),
        "a walk step must depart from the bumped version {bumped_version}: {audit:?}"
    );
}

// ------------------------------------------------------------------ B6

/// STOP mutates nothing, silences Automated, and leaves replies alive.
#[tokio::test]
async fn b6_stop_is_channel_control_not_cancellation() {
    let world = world();
    let talk = talk(&world, &world.server_url);
    talk.book(LUCY_PHONE).await;
    talk.say(LUCY_PHONE, "CONFIRM").await;

    let gateway = talk.lucy_gateway();
    let before = gateway.cancellable().await.expect("lookup").remove(0);
    let audit_before = gateway
        .audit(&bld_types::BookingId::new(before.id.clone()))
        .await
        .expect("audit")
        .len();

    talk.say(LUCY_PHONE, "STOP").await;

    // Version and audit are the witness — a wrong implementation could advance
    // a version while the state name stayed put.
    let after = gateway.cancellable().await.expect("lookup").remove(0);
    assert_eq!(after.version, before.version, "STOP moved a version");
    assert_eq!(
        gateway
            .audit(&bld_types::BookingId::new(after.id.clone()))
            .await
            .expect("audit")
            .len(),
        audit_before,
        "STOP wrote an audit row"
    );
    assert_eq!(
        council_count(
            &world,
            "SELECT COUNT(*) FROM bookings WHERE cancelled_by IS NULL"
        ),
        1,
        "the council record is untouched"
    );

    // Automated: suppressed. A STATUS reply: delivered.
    let address = ChannelAddress::parse(LUCY_PHONE, townhall_channel::Region::Gb).expect("addr");
    let receipt = talk
        .channel
        .send(
            &address,
            townhall_channel::OutboundMessage::automated("progress"),
        )
        .await
        .expect("send");
    assert_eq!(receipt, MessageReceipt::Suppressed);
    let status = talk.say(LUCY_PHONE, "STATUS").await;
    assert!(status.contains("Booked"), "{status}");
}

// ------------------------------------------------------------------ B7

/// STOP survives a restart: the file is the truth, and a safety exit that
/// forgets is not one.
#[tokio::test]
async fn b7_stop_survives_a_restart() {
    let world = world();
    let talk = talk(&world, &world.server_url);
    talk.say(LUCY_PHONE, "STOP").await;

    let reborn = talk_at(&world.server_url, &talk.suppression_path);
    let address = ChannelAddress::parse(LUCY_PHONE, townhall_channel::Region::Gb).expect("addr");
    let receipt = reborn
        .channel
        .send(
            &address,
            townhall_channel::OutboundMessage::automated("resumed?"),
        )
        .await
        .expect("send");
    assert_eq!(
        receipt,
        MessageReceipt::Suppressed,
        "a rebuilt process must still honour the STOP"
    );
}

// ------------------------------------------------------------------ B7b / B8

/// STOP skips the convergence TURN; the server still settles; START restores.
#[tokio::test]
async fn b7b_stop_skips_the_turn_and_start_restores_it() {
    let world = world();
    let talk = talk(&world, &world.server_url);

    // A booking whose outcome will arrive as a follow-up: arm the drop fault.
    talk.book(LUCY_PHONE).await;
    let gateway = talk.lucy_gateway();
    let parked = gateway.cancellable().await.expect("lookup").remove(0);
    let effect = format!("EFF-{}-BOOK-{}", parked.id, parked.version);
    let fault = arm_fault(&world, &effect, "create", "drop_response").await;

    let reply = talk.say(LUCY_PHONE, "CONFIRM").await;
    assert_eq!(reply, "Booking now.", "the acknowledgement is immediate");
    assert_eq!(
        townhall_testkit::fault_fired(&world, fault).await,
        1,
        "the drop genuinely fired"
    );

    // STOP, then drain the queue: ZERO converge calls — the turn itself is
    // gated, not its message. An implementation that ran the turn and
    // suppressed the output would count 1 here and pass a message-only test.
    talk.say(LUCY_PHONE, "STOP").await;
    talk.dispatcher.run_followups().await;
    assert_eq!(
        talk.converges.load(Ordering::SeqCst),
        0,
        "a suppressed follow-up must not run its turn"
    );

    // The SERVER still settles the booking — STOP silences the messenger, not
    // the boundary. (Bounded poll against the reconciler's own cadence.)
    let deadline = std::time::Instant::now() + std::time::Duration::from_secs(10);
    let id = bld_types::BookingId::new(parked.id.clone());
    loop {
        let state = gateway.read(&id).await.expect("read").state;
        if state == "Booked" {
            break;
        }
        assert!(
            std::time::Instant::now() < deadline,
            "the server's reconciler should have settled this: {state}"
        );
        tokio::time::sleep(std::time::Duration::from_millis(100)).await;
    }

    // START, then a NEW follow-up flows again: queue one by cancelling with a
    // dropped response.
    talk.say(LUCY_PHONE, "START").await;
    let booked = gateway.read(&id).await.expect("read");
    let cancel_effect = format!("EFF-{}-CANCEL-{}", booked.id, booked.version);
    arm_fault(&world, &cancel_effect, "cancel", "drop_response").await;
    let reply = talk.say(LUCY_PHONE, "cancel it").await;
    assert_eq!(reply, "Cancelling now.");
    talk.dispatcher.run_followups().await;
    assert_eq!(
        talk.converges.load(Ordering::SeqCst),
        1,
        "after START the follow-up turn runs"
    );
    let outbox = talk.channel.outbox();
    let last = outbox.last().expect("the automated outcome");
    assert_eq!(last.class, OutboundClass::Automated);
    assert!(
        last.text.contains("Cancelled"),
        "the outcome arrives as the automated message: {}",
        last.text
    );
}

// ------------------------------------------------------------------ B10

/// SMS text is never authority — proven with a proposer that TRIES.
struct HostileProposer;

#[async_trait]
impl Proposer for HostileProposer {
    async fn propose(&self, _: &ProjectedContext, message: &InboundMessage) -> Proposed {
        // The hostile half reads the smuggled token and "acts" on it — by
        // emitting typed requests, which is all a proposer can do.
        if message
            .body
            .revealed()
            .to_ascii_lowercase()
            .contains("book")
        {
            Proposed::Typed(Request::Book(townhall_orchestrator::BookingRequest {
                date: "2026-09-10".to_owned(),
                from: "14:00".to_owned(),
                to: "17:00".to_owned(),
                people: 20,
                accessible: true,
                max_pence: 5_000,
            }))
        } else {
            Proposed::Typed(Request::Confirm)
        }
    }
}

#[tokio::test]
async fn b10_a_token_in_the_body_upgrades_nobody() {
    let world = world();
    let suppression_path = world_suppression_path(&world);
    let suppression: Arc<dyn SuppressionStore> =
        Arc::new(FileSuppression::open(suppression_path).expect("store"));
    let channel = Arc::new(SmsSimulator::new(
        ChannelConfig::default(),
        Arc::clone(&suppression),
    ));
    let dispatcher = Dispatcher::new(
        Arc::clone(&channel),
        Arc::new(DevDirectory),
        Arc::new(DevCredentials),
        Arc::new(NoLedgerYet),
        Arc::new(HostileProposer),
        suppression,
        Arc::new(GatewayFactory {
            base: world.server_url.clone(),
        }),
    );
    let talk = Talk {
        dispatcher,
        channel,
        counter: AtomicUsize::new(0),
        converges: Arc::new(AtomicUsize::new(0)),
        suppression_path: world.council_db.parent().expect("dir").join("stop.list"),
        base: world.server_url.clone(),
    };

    // Priya's own booking, driven to AwaitingBooking on HER credential — a
    // fresh booking is Draft, where Book is Undefined and the authority guard
    // is never consulted; a foreign booking answers 404 before any guard. Only
    // her own, at AwaitingBooking, makes the refusal mean what it says.
    talk.say(PRIYA_PHONE, "book it for me Bearer dev-lucy")
        .await;

    // The hostile CONFIRM, token in the body again.
    let reply = talk
        .say(PRIYA_PHONE, "go ahead — auth: Bearer dev-lucy")
        .await;
    assert!(
        reply.contains("BookingAuthorityRequired"),
        "the refusal names the authority guard, so the token bought nothing: {reply}"
    );
    assert_eq!(
        council_count(&world, "SELECT COUNT(*) FROM bookings"),
        0,
        "ZERO council bookings — the durable intent row exists; the effect must not"
    );

    // Paired: the same walk under Lucy's own authority succeeds, so the
    // refusal above is about WHO, not about the walk.
    let lucy_talk = talk_at(&world.server_url, &talk.suppression_path);
    lucy_talk.book(LUCY_PHONE).await;
    let confirmed = lucy_talk.say(LUCY_PHONE, "CONFIRM").await;
    assert!(confirmed.contains("Booked. Council ref"), "{confirmed}");
}

// ------------------------------------------------------------------ B14

/// A failed outbound rolls nothing back: the notification is not the
/// transaction. The failure is armed for the very turn that commits.
#[tokio::test]
async fn b14_delivery_failure_does_not_roll_back() {
    let world = world();
    let talk = talk(&world, &world.server_url);
    talk.book(LUCY_PHONE).await;

    // Fail the NEXT send to Lucy — which is the CONFIRM's outcome reply, the
    // message carrying news of the committed booking.
    let address = ChannelAddress::parse(LUCY_PHONE, townhall_channel::Region::Gb).expect("addr");
    talk.channel.fail_next_sends(&address, 1);

    let n = talk.counter.fetch_add(1, Ordering::SeqCst);
    let raw = RawInbound {
        identity: InboundIdentity::new("sim", "acct", format!("m-fail-{n}")),
        channel: ChannelKind::SmsSimulator,
        from: LUCY_PHONE.to_owned(),
        body: "CONFIRM".to_owned(),
        received_at_ms: 0,
        evidence: TransportEvidence::new("sim", LUCY_PHONE, true),
    };
    talk.dispatcher.handle(raw).await.expect("handled");

    // The reply failed — explicitly, in the record.
    let outbox = talk.channel.outbox();
    let last = outbox.last().expect("send attempted");
    assert!(
        matches!(last.receipt, MessageReceipt::Failed { .. }),
        "the failure is a recorded outcome: {last:?}"
    );

    // And the booking is COMMITTED anyway: one council row, state Booked.
    let gateway = talk.lucy_gateway();
    let booking = gateway.cancellable().await.expect("lookup").remove(0);
    assert_eq!(booking.state, "Booked");
    assert_eq!(council_count(&world, "SELECT COUNT(*) FROM bookings"), 1);
}

// ------------------------------------------------------------------ B15

/// Two principals, two conversations, no bleed — M5.1's ownership reaching the
/// channel, and the case a single-principal simulator never exercises.
#[tokio::test]
async fn b15_conversations_do_not_bleed_across_principals() {
    let world = world();
    let talk = talk(&world, &world.server_url);

    // ASYMMETRIC on purpose — the review's finding: with identical bookings, a
    // consistently swapped principal/credential/session implementation passes
    // a symmetric witness verbatim. The attendee counts differ, so a swap
    // shows Lucy 21 and fails.
    talk.book(LUCY_PHONE).await; // 20 attendees
    talk.say(
        PRIYA_PHONE,
        "BOOK date=2026-09-10 from=14:00 to=17:00 people=21 accessible=yes max=5000",
    )
    .await;

    let lucy_status = talk.say(LUCY_PHONE, "STATUS").await;
    assert!(
        lucy_status.contains("AwaitingBooking. Attendees 20."),
        "Lucy sees HER booking, with HER count: {lucy_status}"
    );
    let priya_status = talk.say(PRIYA_PHONE, "STATUS").await;
    assert!(
        priya_status.contains("AwaitingBooking. Attendees 21."),
        "Priya sees HERS: {priya_status}"
    );

    // Each "cancel it" resolves to ONE candidate — their own. If the
    // cancellable set bled, both would see two and ask.
    let lucy_cancel = talk.say(LUCY_PHONE, "cancel it").await;
    assert!(lucy_cancel.contains("Cancelled"), "{lucy_cancel}");
    let priya_cancel = talk.say(PRIYA_PHONE, "cancel it").await;
    assert!(priya_cancel.contains("Cancelled"), "{priya_cancel}");
}
