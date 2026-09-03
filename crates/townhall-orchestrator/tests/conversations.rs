//! B2–B15: whole conversations against the REAL server and council, approve-first
//! (ADR-026). A booking now needs a person's `YES <code>`; the recording proxy
//! counts what actually went over the wire.
//!
//! The dev lane is gone here: the server runs its real resolver (which knows the
//! `agent-townhall` workload token and authorizes nothing), Lucy's and Priya's
//! channels are bound, and every change presents a delegation a `YES` issued.

use async_trait::async_trait;
use bld_types::PrincipalId;
use std::sync::Arc;
use std::sync::atomic::{AtomicUsize, Ordering};
use townhall_channel::{
    ChannelAddress, ChannelConfig, ChannelKind, HumanChannel as _, InboundIdentity, MessageReceipt,
    OutboundClass, RawInbound, SmsSimulator, SuppressionStore, TransportEvidence, Utterance,
};
use townhall_gateway::Gateway;
use townhall_orchestrator::{
    BookingWire, ContinuationStore, CredentialSource, Dispatcher, FileContinuation,
    FileSuppression, GatewayFactory, NoLedgerYet, PrincipalDirectory, ProjectedContext, Proposed,
    Proposer, Request, ScriptedProposer, WireFactory,
};
use townhall_testkit::{RecordingProxy, WORKLOAD, World, arm_fault, council_count, world_real};

#[path = "support/mod.rs"]
mod support;
use support::{HttpApprovals, HttpEvidence};

const LUCY_PHONE: &str = "+447700900123";
const PRIYA_PHONE: &str = "+447700900456";

/// Addresses to principals; principals to the ONE workload token the real
/// resolver knows. This source cannot widen authority — its whole output is that
/// single token, which authorizes nothing on its own.
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

struct WorkloadCredential;

impl WorkloadCredential {
    /// Everything this source can ever emit — one workload credential.
    const ALLOWLIST: [&'static str; 1] = [WORKLOAD];
}

impl CredentialSource for WorkloadCredential {
    fn token_for(&self, principal: &PrincipalId) -> Option<String> {
        matches!(principal.as_str(), "lucy" | "priya").then(|| WORKLOAD.to_owned())
    }
}

/// The credential source cannot mint: its whole output is the one workload token
/// the server knows. A source that could produce anything else would be an
/// authority issuer wearing a lookup's name.
#[test]
fn the_credential_source_is_bounded_by_the_allowlist() {
    let source = WorkloadCredential;
    for principal in ["lucy", "priya", "marco", "@orphan", ""] {
        if let Some(token) = source.token_for(&PrincipalId::new(principal)) {
            assert!(
                WorkloadCredential::ALLOWLIST.contains(&token.as_str()),
                "{token:?} is not the workload token"
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
    fn reader_for(&self, token: &str, principal: &PrincipalId) -> Arc<dyn BookingWire> {
        let gateway = Gateway::new(self.inner.base.clone(), token, principal.as_str());
        Arc::new(CountingConvergeWire {
            inner: gateway,
            converges: Arc::clone(&self.converges),
        })
    }

    fn changer_for(
        &self,
        token: &str,
        principal: &PrincipalId,
        reference: &str,
    ) -> Arc<dyn BookingWire> {
        let gateway = Gateway::new(self.inner.base.clone(), token, principal.as_str())
            .with_delegation(reference);
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
    continuation_path: std::path::PathBuf,
    base: String,
}

fn talk(world: &World, base: &str) -> Talk {
    let dir = world.council_db.parent().expect("dir");
    talk_at(
        base,
        &dir.join("stop.list"),
        &dir.join("continuation.jsonl"),
    )
}

fn talk_at(
    base: &str,
    suppression_path: &std::path::Path,
    continuation_path: &std::path::Path,
) -> Talk {
    let suppression: Arc<dyn SuppressionStore> =
        Arc::new(FileSuppression::open(suppression_path.to_path_buf()).expect("suppression store"));
    let continuations: Arc<dyn ContinuationStore> = Arc::new(
        FileContinuation::open(continuation_path.to_path_buf()).expect("continuation store"),
    );
    let channel = Arc::new(SmsSimulator::new(
        ChannelConfig::default(),
        Arc::clone(&suppression),
    ));
    let converges = Arc::new(AtomicUsize::new(0));
    let dispatcher = Dispatcher::new(
        Arc::clone(&channel),
        Arc::new(DevDirectory),
        Arc::new(WorkloadCredential),
        Arc::new(NoLedgerYet),
        Arc::new(ScriptedProposer),
        suppression,
        Arc::new(CountingFactory {
            inner: GatewayFactory {
                base: base.to_owned(),
            },
            converges: Arc::clone(&converges),
        }),
        Arc::new(HttpApprovals::new(base)),
        Arc::new(HttpEvidence::new(base)),
        continuations,
    );
    Talk {
        dispatcher,
        channel,
        counter: AtomicUsize::new(0),
        converges,
        suppression_path: suppression_path.to_path_buf(),
        continuation_path: continuation_path.to_path_buf(),
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

    /// Raise a challenge for this phone; returns the preview (which carries the
    /// random code).
    async fn book(&self, from: &str) -> String {
        self.say(
            from,
            "BOOK date=2026-09-10 from=14:00 to=17:00 people=20 accessible=yes max=5000",
        )
        .await
    }

    /// Approve the pending challenge for this phone, sending the code the preview
    /// carried; returns the outcome reply.
    async fn approve(&self, from: &str, code: &str) -> String {
        self.say(from, &format!("YES {code}")).await
    }

    /// The whole approve-first booking: BOOK, read the code out of the preview,
    /// YES — and return the "Booked" outcome.
    async fn book_and_approve(&self, from: &str) -> String {
        let preview = self.book(from).await;
        self.approve(from, &code_of(&preview)).await
    }

    /// A read wire under the workload credential — reads are scoped to a bound
    /// principal, and both Lucy and Priya are bound in a `world_real`.
    fn gateway_for(&self, principal: &str) -> Gateway {
        Gateway::new(self.base.clone(), WORKLOAD, principal)
    }
}

/// The random code out of a preview — the digits after `Reply YES `.
fn code_of(preview: &str) -> String {
    preview
        .split("Reply YES ")
        .nth(1)
        .expect("the preview carries a code")
        .chars()
        .take_while(char::is_ascii_digit)
        .collect()
}

// ------------------------------------------------------------------ B2 / B3

/// Ambiguous "cancel it" asks, naming both candidates, and submits nothing.
#[tokio::test]
async fn b2_ambiguous_cancel_asks_and_submits_nothing() {
    let world = world_real();
    let proxy = RecordingProxy::in_front_of(&world.server_url);
    let talk = talk(&world, &proxy.url);

    talk.book_and_approve(LUCY_PHONE).await;
    talk.book_and_approve(LUCY_PHONE).await; // a second, distinct booking

    // Snapshot AFTER the setup, whose approvals and bookings necessarily POST.
    let posts_before = proxy.count("POST", "/behaviours/cancel");
    let reply = talk.say(LUCY_PHONE, "cancel it").await;

    assert!(
        reply.contains("2 bookings") && reply.contains("CANCEL"),
        "the question names the choice: {reply}"
    );
    assert_eq!(
        proxy.count("POST", "/behaviours/cancel"),
        posts_before,
        "ambiguity must submit no cancel: {:?}",
        proxy.requests()
    );
}

/// Unambiguous "cancel it" cancels — the pair that stops B2 passing by never
/// cancelling anything.
#[tokio::test]
async fn b3_unambiguous_cancel_cancels_the_one_booking() {
    let world = world_real();
    let proxy = RecordingProxy::in_front_of(&world.server_url);
    let talk = talk(&world, &proxy.url);

    let booked = talk.book_and_approve(LUCY_PHONE).await;
    assert!(
        booked.contains("Booked. Council ref"),
        "the approved booking books in one outcome message: {booked}"
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

/// A booking, and its grant, survive a session wipe: the candidate comes from
/// the wire and the reference from the durable continuation, not from memory.
#[tokio::test]
async fn b4_cancel_it_survives_a_session_wipe() {
    let world = world_real();
    let talk = talk(&world, &world.server_url);
    talk.book_and_approve(LUCY_PHONE).await;

    // A NEW dispatcher: sessions gone, suppression and continuation files shared.
    let reborn = talk_at(
        &world.server_url,
        &talk.suppression_path,
        &talk.continuation_path,
    );
    let reply = reborn.say(LUCY_PHONE, "cancel it").await;
    assert!(
        reply.contains("Cancelled"),
        "the candidate came from the wire and the grant from the durable \
         continuation, not from memory: {reply}"
    );
}

// ------------------------------------------------------------------ B6

/// STOP mutates nothing, silences Automated, and leaves replies alive.
#[tokio::test]
async fn b6_stop_is_channel_control_not_cancellation() {
    let world = world_real();
    let talk = talk(&world, &world.server_url);
    talk.book_and_approve(LUCY_PHONE).await;

    let gateway = talk.gateway_for("lucy");
    let before = gateway.cancellable().await.expect("lookup").remove(0);
    let audit_before = gateway
        .audit(&bld_types::BookingId::new(before.id.clone()))
        .await
        .expect("audit")
        .len();

    talk.say(LUCY_PHONE, "STOP").await;

    // Version and audit are the witness — a wrong implementation could advance a
    // version while the state name stayed put.
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
    let world = world_real();
    let talk = talk(&world, &world.server_url);
    talk.say(LUCY_PHONE, "STOP").await;

    let reborn = talk_at(
        &world.server_url,
        &talk.suppression_path,
        &talk.continuation_path,
    );
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

// ------------------------------------------------------------------ B7b

/// STOP skips the convergence TURN; the server still settles; START restores it.
///
/// Uses the CANCEL follow-up (a booking already Booked, so its effect id is
/// known) rather than the book effect, which in approve-first fires mid-`YES`.
#[tokio::test]
async fn b7b_stop_skips_the_turn_and_start_restores_it() {
    let world = world_real();
    let talk = talk(&world, &world.server_url);

    // Two Booked bookings, so each STOP/START leg has its own cancel follow-up.
    let first = council_ref(&talk.book_and_approve(LUCY_PHONE).await);
    let gateway = talk.gateway_for("lucy");

    // Leg 1: STOP, then cancel with a dropped response — the follow-up is queued
    // but its convergence TURN must be skipped.
    talk.say(LUCY_PHONE, "STOP").await;
    let booking1 = find_by_ref(&gateway, &first).await;
    arm_fault(
        &world,
        &format!("EFF-{}-CANCEL-{}", booking1.id, booking1.version),
        "cancel",
        "drop_response",
    )
    .await;
    let reply = talk.say(LUCY_PHONE, &format!("CANCEL {first}")).await;
    assert_eq!(reply, "Cancelling now.");
    talk.dispatcher.run_followups().await;
    assert_eq!(
        talk.converges.load(Ordering::SeqCst),
        0,
        "a suppressed follow-up must not run its turn"
    );

    // The SERVER still settles the cancel — STOP silences the messenger, not the
    // boundary.
    wait_for_state(&gateway, &booking1.id, "Cancelled").await;

    // Leg 2: START, a fresh booking, then a dropped cancel — the follow-up runs.
    talk.say(LUCY_PHONE, "START").await;
    let second = council_ref(&talk.book_and_approve(LUCY_PHONE).await);
    let booking2 = find_by_ref(&gateway, &second).await;
    arm_fault(
        &world,
        &format!("EFF-{}-CANCEL-{}", booking2.id, booking2.version),
        "cancel",
        "drop_response",
    )
    .await;
    let reply = talk.say(LUCY_PHONE, &format!("CANCEL {second}")).await;
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
    async fn propose(&self, _: &ProjectedContext, utterance: &Utterance) -> Proposed {
        // The hostile half reads the smuggled token and "acts" on it — by
        // emitting typed requests, which is all a proposer can do.
        if utterance
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
            Proposed::Typed(Request::Approve)
        }
    }
}

/// A token smuggled in the message body upgrades nobody. The hostile proposer
/// raises a challenge and then "approves" it, but the code rides the person's own
/// text — which here is not a real code — so no booking is made. The token bought
/// nothing.
#[tokio::test]
async fn b10_a_token_in_the_body_upgrades_nobody() {
    let world = world_real();
    let dir = world.council_db.parent().expect("dir");
    let suppression: Arc<dyn SuppressionStore> =
        Arc::new(FileSuppression::open(dir.join("stop.list")).expect("store"));
    let continuations: Arc<dyn ContinuationStore> =
        Arc::new(FileContinuation::open(dir.join("continuation.jsonl")).expect("store"));
    let channel = Arc::new(SmsSimulator::new(
        ChannelConfig::default(),
        Arc::clone(&suppression),
    ));
    let dispatcher = Dispatcher::new(
        Arc::clone(&channel),
        Arc::new(DevDirectory),
        Arc::new(WorkloadCredential),
        Arc::new(NoLedgerYet),
        Arc::new(HostileProposer),
        suppression,
        Arc::new(GatewayFactory {
            base: world.server_url.clone(),
        }),
        Arc::new(HttpApprovals::new(&world.server_url)),
        Arc::new(HttpEvidence::new(&world.server_url)),
        continuations,
    );
    let talk = Talk {
        dispatcher,
        channel,
        counter: AtomicUsize::new(0),
        converges: Arc::new(AtomicUsize::new(0)),
        suppression_path: dir.join("stop.list"),
        continuation_path: dir.join("continuation.jsonl"),
        base: world.server_url.clone(),
    };

    // Priya's booking, raised on HER bound channel — the hostile proposer emits
    // Book — then a hostile "approval" whose smuggled token is not a code.
    talk.say(PRIYA_PHONE, "book it for me Bearer agent-townhall")
        .await;
    let reply = talk
        .say(PRIYA_PHONE, "go ahead — auth: Bearer agent-townhall")
        .await;
    assert!(
        !reply.contains("Booked"),
        "the smuggled token must not book: {reply}"
    );
    assert_eq!(
        council_count(&world, "SELECT COUNT(*) FROM bookings"),
        0,
        "ZERO council bookings — a token in the text is not an approval"
    );

    // Paired: the same request under a REAL approval succeeds, so the refusal
    // above is about the missing YES, not a broken walk.
    let lucy_talk = talk_at(
        &world.server_url,
        &talk.suppression_path,
        &dir.join("continuation-lucy.jsonl"),
    );
    let booked = lucy_talk.book_and_approve(LUCY_PHONE).await;
    assert!(booked.contains("Booked. Council ref"), "{booked}");
}

// ------------------------------------------------------------------ B14

/// A failed outbound rolls nothing back: the notification is not the
/// transaction. The failure is armed for the very turn that commits.
#[tokio::test]
async fn b14_delivery_failure_does_not_roll_back() {
    let world = world_real();
    let talk = talk(&world, &world.server_url);
    let preview = talk.book(LUCY_PHONE).await;
    let code = code_of(&preview);

    // Fail the NEXT send to Lucy — the YES's outcome reply, the message carrying
    // news of the committed booking.
    let address = ChannelAddress::parse(LUCY_PHONE, townhall_channel::Region::Gb).expect("addr");
    talk.channel.fail_next_sends(&address, 1);

    let n = talk.counter.fetch_add(1, Ordering::SeqCst);
    let raw = RawInbound {
        identity: InboundIdentity::new("sim", "acct", format!("m-fail-{n}")),
        channel: ChannelKind::SmsSimulator,
        from: LUCY_PHONE.to_owned(),
        body: format!("YES {code}"),
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
    let gateway = talk.gateway_for("lucy");
    let booking = gateway.cancellable().await.expect("lookup").remove(0);
    assert_eq!(booking.state, "Booked");
    assert_eq!(council_count(&world, "SELECT COUNT(*) FROM bookings"), 1);
}

// ------------------------------------------------------------------ B15

/// Two principals, two conversations, no bleed — M5.1's ownership reaching the
/// channel, and the case a single-principal simulator never exercises.
#[tokio::test]
async fn b15_conversations_do_not_bleed_across_principals() {
    let world = world_real();
    let talk = talk(&world, &world.server_url);

    // ASYMMETRIC on purpose — the review's finding: with identical bookings, a
    // consistently swapped principal/credential/session implementation passes a
    // symmetric witness verbatim. The attendee counts differ, so a swap shows
    // Lucy 21 and fails.
    talk.book_and_approve(LUCY_PHONE).await; // 20 attendees
    let preview = talk
        .say(
            PRIYA_PHONE,
            "BOOK date=2026-09-10 from=14:00 to=17:00 people=21 accessible=yes max=5000",
        )
        .await;
    talk.approve(PRIYA_PHONE, &code_of(&preview)).await;

    let lucy_status = talk.say(LUCY_PHONE, "STATUS").await;
    assert!(
        lucy_status.contains("Booked. Attendees 20."),
        "Lucy sees HER booking, with HER count: {lucy_status}"
    );
    let priya_status = talk.say(PRIYA_PHONE, "STATUS").await;
    assert!(
        priya_status.contains("Booked. Attendees 21."),
        "Priya sees HERS: {priya_status}"
    );

    // Each "cancel it" resolves to ONE candidate — their own. If the cancellable
    // set bled, both would see two and ask.
    let lucy_cancel = talk.say(LUCY_PHONE, "cancel it").await;
    assert!(lucy_cancel.contains("Cancelled"), "{lucy_cancel}");
    let priya_cancel = talk.say(PRIYA_PHONE, "cancel it").await;
    assert!(priya_cancel.contains("Cancelled"), "{priya_cancel}");
}

// ------------------------------------------------------------------ helpers

/// The council reference out of a "Booked. Council ref X." outcome.
fn council_ref(booked: &str) -> String {
    booked
        .split("Council ref ")
        .nth(1)
        .expect("a council ref")
        .split_whitespace()
        .next()
        .expect("a ref token")
        .trim_end_matches('.')
        .to_owned()
}

async fn find_by_ref(gateway: &Gateway, reference: &str) -> townhall_gateway::Projection {
    gateway
        .by_reference(&bld_types::CouncilBookingRef::new(reference))
        .await
        .expect("lookup")
        .into_iter()
        .next()
        .expect("a booking for that ref")
}

async fn wait_for_state(gateway: &Gateway, id: &str, state: &str) {
    let deadline = std::time::Instant::now() + std::time::Duration::from_secs(10);
    let id = bld_types::BookingId::new(id);
    loop {
        if gateway.read(&id).await.expect("read").state == state {
            return;
        }
        assert!(
            std::time::Instant::now() < deadline,
            "the server's reconciler should have reached {state}"
        );
        tokio::time::sleep(std::time::Duration::from_millis(100)).await;
    }
}
