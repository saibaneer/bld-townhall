//! B9, B11–B13: the dispatcher's ordering contract, held with hostile ports.
//!
//! No server here. The wire is a fake that counts or panics, the proposer
//! panics on entry, and the witnesses are what NEVER got called — which is the
//! only way "the control commands reach nothing" is an assertion rather than a
//! hope.

use async_trait::async_trait;
use bld_types::{BookingId, BookingRequirements, CouncilBookingRef, PrincipalId};
use std::sync::Arc;
use std::sync::atomic::{AtomicUsize, Ordering};
use townhall_channel::{
    ChannelAddress, ChannelConfig, ChannelKind, InboundIdentity, MessageReceipt, RawInbound,
    SmsSimulator, SuppressionStore, TransportEvidence, Utterance, simulator::InMemorySuppression,
};
use townhall_gateway::{GatewayError, Projection, Turn, VenueRow};
use townhall_orchestrator::{
    BookingRequest, BookingWire, Continuation, CredentialSource, Dispatcher, InboundEvidence,
    PrincipalDirectory, ProjectedContext, Proposed, Proposer, Request, ScriptedProposer,
    UnmeteredLedger, UsageDenied, UsageLedger, WireFactory,
};

#[path = "support/mod.rs"]
mod support;
use support::{MemoryContinuation, StubApprovals, StubEvidence};

// ------------------------------------------------------------------ fakes

struct FixedDirectory(Vec<(&'static str, &'static str)>);

impl PrincipalDirectory for FixedDirectory {
    fn resolve(&self, address: &ChannelAddress) -> Option<PrincipalId> {
        self.0
            .iter()
            .find(|(bound, _)| *bound == address.revealed())
            .map(|(_, principal)| PrincipalId::new(*principal))
    }
}

struct FixedCredentials;

impl CredentialSource for FixedCredentials {
    fn token_for(&self, principal: &PrincipalId) -> Option<String> {
        Some(format!("dev-{principal}"))
    }
}

/// Panics on entry: the witness that a message NEVER reached the proposer.
struct PanickingProposer;

#[async_trait]
impl Proposer for PanickingProposer {
    async fn propose(&self, _: &ProjectedContext, _: &Utterance) -> Proposed {
        panic!("a control command reached the proposer");
    }
}

/// Answers `Unclear` and counts — for proving the proposer WAS consulted,
/// exactly once, when that is the claim.
#[derive(Default)]
struct UnclearProposer(AtomicUsize);

#[async_trait]
impl Proposer for UnclearProposer {
    async fn propose(&self, _: &ProjectedContext, _: &Utterance) -> Proposed {
        self.0.fetch_add(1, Ordering::SeqCst);
        Proposed::Unclear
    }
}

/// Counts every call; panics if constructed panicking.
#[derive(Default)]
struct CountingWire {
    calls: AtomicUsize,
    mutations: AtomicUsize,
    panicking: bool,
}

impl CountingWire {
    fn count(&self) -> usize {
        self.calls.load(Ordering::SeqCst)
    }
    fn mutation_count(&self) -> usize {
        self.mutations.load(Ordering::SeqCst)
    }
    fn touch(&self) {
        assert!(!self.panicking, "the wire was reached");
        self.calls.fetch_add(1, Ordering::SeqCst);
    }
}

#[async_trait]
impl BookingWire for CountingWire {
    async fn create(
        &self,
        _: &BookingId,
        _: &BookingRequirements,
    ) -> Result<Projection, GatewayError> {
        self.touch();
        self.mutations.fetch_add(1, Ordering::SeqCst);
        Err(GatewayError::Unavailable("counting fake".to_owned()))
    }
    async fn read(&self, _: &BookingId) -> Result<Projection, GatewayError> {
        self.touch();
        Err(GatewayError::UnknownBooking)
    }
    async fn cancellable(&self) -> Result<Vec<Projection>, GatewayError> {
        self.touch();
        Ok(Vec::new())
    }
    async fn by_reference(&self, _: &CouncilBookingRef) -> Result<Vec<Projection>, GatewayError> {
        self.touch();
        Ok(Vec::new())
    }
    async fn venues(&self) -> Result<Vec<VenueRow>, GatewayError> {
        self.touch();
        Ok(Vec::new())
    }
    async fn propose_at(
        &self,
        _: &BookingId,
        _: u64,
        _: &str,
        _: Option<serde_json::Value>,
    ) -> Result<Turn, GatewayError> {
        self.touch();
        self.mutations.fetch_add(1, Ordering::SeqCst);
        Err(GatewayError::Unavailable("counting fake".to_owned()))
    }
    async fn converge(
        &self,
        _: &BookingId,
        _: std::time::Duration,
    ) -> Result<Projection, GatewayError> {
        self.touch();
        Err(GatewayError::NotConverged { attempts: 0 })
    }
}

struct FixedWireFactory(Arc<CountingWire>);

impl WireFactory for FixedWireFactory {
    // One counting wire behind BOTH kinds, deliberately: the counts these
    // tests assert on are about how many times the dispatcher touched the wire,
    // not about which sort it asked for. Splitting the counters would silently
    // halve every existing expectation.
    fn reader_for(&self, _token: &str, _principal: &PrincipalId) -> Arc<dyn BookingWire> {
        Arc::clone(&self.0) as Arc<dyn BookingWire>
    }

    fn changer_for(
        &self,
        _token: &str,
        _principal: &PrincipalId,
        _reference: &str,
    ) -> Arc<dyn BookingWire> {
        Arc::clone(&self.0) as Arc<dyn BookingWire>
    }
}

/// A directory that panics on resolve — the witness that identity was never
/// even ASKED for. The review's point: counting wire calls alone lets a
/// dispatcher resolve identity, fetch credentials and build a wire before
/// recognizing `HELP`, and nothing fails.
struct PanickingDirectory;

impl PrincipalDirectory for PanickingDirectory {
    fn resolve(&self, _: &ChannelAddress) -> Option<PrincipalId> {
        panic!("a control command resolved identity");
    }
}

/// A factory that panics either way — no wire of EITHER kind may be built.
///
/// Both methods panic, and that matters more since M7B split them: a factory
/// that only refused to build a changer would let a control command construct
/// a reader, and "controls reach no wire" would quietly become "controls reach
/// no MUTATING wire" — a weaker claim wearing the same test name.
struct PanickingFactory;

impl WireFactory for PanickingFactory {
    fn reader_for(&self, _: &str, _principal: &PrincipalId) -> Arc<dyn BookingWire> {
        panic!("a control command built a read wire");
    }

    fn changer_for(
        &self,
        _: &str,
        _principal: &PrincipalId,
        _reference: &str,
    ) -> Arc<dyn BookingWire> {
        panic!("a control command built a change wire");
    }
}

/// A usage ledger that counts both the balance reads and the reserves, and
/// answers BALANCE with a nonsense sentinel a hardcoded string could not
/// reproduce. The reserve counter is the witness that a control command spends
/// no unit.
#[derive(Default)]
struct SentinelLedger {
    describes: AtomicUsize,
    reserves: AtomicUsize,
}

const BALANCE_SENTINEL: &str = "UNMETERED-SENTINEL-7f3a";

#[async_trait]
impl UsageLedger for SentinelLedger {
    async fn reserve(&self, _evidence: &InboundEvidence) -> Result<(), UsageDenied> {
        self.reserves.fetch_add(1, Ordering::SeqCst);
        Ok(())
    }
    async fn debit(&self, _evidence: &InboundEvidence) {}
    async fn release(&self, _evidence: &InboundEvidence) {}
    async fn describe_balance(&self, _evidence: &InboundEvidence) -> String {
        self.describes.fetch_add(1, Ordering::SeqCst);
        BALANCE_SENTINEL.to_owned()
    }
}

// ------------------------------------------------------------------ harness

struct Bench {
    dispatcher: Dispatcher<SmsSimulator>,
    channel: Arc<SmsSimulator>,
    wire: Arc<CountingWire>,
    ledger: Arc<SentinelLedger>,
    proposer_calls: Option<Arc<UnclearProposer>>,
    counter: AtomicUsize,
}

fn bench(proposer: &ProposerChoice) -> Bench {
    let suppression: Arc<InMemorySuppression> = Arc::new(InMemorySuppression::default());
    let channel = Arc::new(SmsSimulator::new(
        ChannelConfig::default(),
        Arc::clone(&suppression) as Arc<dyn SuppressionStore>,
    ));
    let wire = Arc::new(CountingWire::default());
    let ledger = Arc::new(SentinelLedger::default());
    let (proposer_arc, proposer_calls): (Arc<dyn Proposer>, Option<Arc<UnclearProposer>>) =
        match proposer {
            ProposerChoice::Panicking => (Arc::new(PanickingProposer), None),
            ProposerChoice::Unclear => {
                let counting = Arc::new(UnclearProposer::default());
                (Arc::clone(&counting) as Arc<dyn Proposer>, Some(counting))
            }
            ProposerChoice::Scripted => (Arc::new(ScriptedProposer), None),
        };
    let dispatcher = Dispatcher::new(
        Arc::clone(&channel),
        Arc::new(FixedDirectory(vec![("+447700900123", "lucy")])),
        Arc::new(FixedCredentials),
        Arc::clone(&ledger) as Arc<dyn UsageLedger>,
        proposer_arc,
        suppression,
        Arc::new(FixedWireFactory(Arc::clone(&wire))),
        Arc::new(StubApprovals::default()),
        Arc::new(StubEvidence::default()),
        Arc::new(MemoryContinuation::new()),
    );
    Bench {
        dispatcher,
        channel,
        wire,
        ledger,
        proposer_calls,
        counter: AtomicUsize::new(0),
    }
}

enum ProposerChoice {
    Panicking,
    Unclear,
    Scripted,
}

impl Bench {
    fn raw(&self, from: &str, body: &str) -> RawInbound {
        let n = self.counter.fetch_add(1, Ordering::SeqCst);
        RawInbound {
            identity: InboundIdentity::new("sim", "acct", format!("m-{n}")),
            channel: ChannelKind::SmsSimulator,
            from: from.to_owned(),
            body: body.to_owned(),
            received_at_ms: 0,
            evidence: TransportEvidence::new("sim", from, true),
        }
    }

    fn last_reply(&self) -> String {
        let outbox = self.channel.outbox();
        let last = outbox.last().expect("a reply was sent");
        assert!(
            matches!(last.receipt, MessageReceipt::Delivered { .. }),
            "the reply must actually go out: {last:?}"
        );
        last.text.clone()
    }
}

// ------------------------------------------------------------------ B11

/// Every control command answers with NOTHING else consulted: no proposer, no
/// wire, no wire CONSTRUCTION — and for the four that need no identity, no
/// directory resolution either. Everything past the classification panics, so a
/// dispatcher that "just quickly resolves who this is" before recognizing HELP
/// dies here instead of passing a call-count check it never incremented.
#[tokio::test]
async fn b11_control_commands_reach_nothing() {
    let suppression: Arc<InMemorySuppression> = Arc::new(InMemorySuppression::default());
    let channel = Arc::new(SmsSimulator::new(
        ChannelConfig::default(),
        Arc::clone(&suppression) as Arc<dyn SuppressionStore>,
    ));
    let dispatcher = Dispatcher::new(
        Arc::clone(&channel),
        Arc::new(PanickingDirectory),
        Arc::new(FixedCredentials),
        Arc::new(UnmeteredLedger),
        Arc::new(PanickingProposer),
        suppression,
        Arc::new(PanickingFactory),
        Arc::new(StubApprovals::default()),
        Arc::new(StubEvidence::default()),
        Arc::new(MemoryContinuation::new()),
    );
    let counter = AtomicUsize::new(0);
    for (text, expect) in [
        ("HELP", "BOOK date="),
        ("STOP", "Stopped."),
        ("START", "Resumed."),
        // REVOKE now reaches the approval PORT (a stub with no grants seeded for
        // this number), still BEFORE identity — so the PanickingDirectory below
        // never fires. With nothing to stop it answers plainly.
        ("REVOKE", "no active approvals to stop"),
    ] {
        let n = counter.fetch_add(1, Ordering::SeqCst);
        dispatcher
            .handle(RawInbound {
                identity: InboundIdentity::new("sim", "acct", format!("ctl-{n}")),
                channel: ChannelKind::SmsSimulator,
                from: "07700 900123".to_owned(),
                body: text.to_owned(),
                received_at_ms: 0,
                evidence: TransportEvidence::new("sim", "07700 900123", true),
            })
            .await
            .expect("handled");
        let outbox = channel.outbox();
        let reply = &outbox.last().expect("a reply").text;
        assert!(
            reply.contains(expect),
            "{text}: expected {expect:?} in {reply:?}"
        );
    }

    // BALANCE is the one control that legitimately asks WHO — so it runs on a
    // bench whose directory answers, while the wire still panics on touch (a
    // counting wire asserting zero, plus the panicking proposer).
    let bench = bench(&ProposerChoice::Panicking);
    bench
        .dispatcher
        .handle(bench.raw("07700 900123", "BALANCE"))
        .await
        .expect("handled");
    assert!(bench.last_reply().contains(BALANCE_SENTINEL));
    assert_eq!(bench.wire.count(), 0, "BALANCE touched the wire");
}

/// T7 — a texted REVOKE names the real count and builds no identity.
///
/// The stub returns a count DERIVED from grants it holds (not a hardcoded 0), so
/// the reply naming `Stopped 3` fails a dispatcher that never calls the port or
/// mis-reads the count. And it runs on a PanickingDirectory/Proposer/Factory: if
/// REVOKE resolved identity, built a token, or touched a wire before answering,
/// the test would panic instead of pass — the "no grant, no principal" property
/// STOP and REVOKE share.
#[tokio::test]
async fn a_revoke_names_the_count_and_builds_no_identity() {
    let suppression: Arc<InMemorySuppression> = Arc::new(InMemorySuppression::default());
    let channel = Arc::new(SmsSimulator::new(
        ChannelConfig::default(),
        Arc::clone(&suppression) as Arc<dyn SuppressionStore>,
    ));
    let approvals = Arc::new(StubApprovals::default());
    // Three live grants for this number — the count the sweep must report.
    approvals.seed_grants("+447700900123", 3);
    let dispatcher = Dispatcher::new(
        Arc::clone(&channel),
        Arc::new(PanickingDirectory),
        Arc::new(FixedCredentials),
        Arc::new(UnmeteredLedger),
        Arc::new(PanickingProposer),
        suppression,
        Arc::new(PanickingFactory),
        Arc::clone(&approvals) as Arc<dyn townhall_orchestrator::ApprovalPort>,
        Arc::new(StubEvidence::default()),
        Arc::new(MemoryContinuation::new()),
    );

    dispatcher
        .handle(RawInbound {
            identity: InboundIdentity::new("sim", "acct", "revoke-1"),
            channel: ChannelKind::SmsSimulator,
            from: "07700 900123".to_owned(),
            body: "REVOKE".to_owned(),
            received_at_ms: 0,
            evidence: TransportEvidence::new("sim", "07700 900123", true),
        })
        .await
        .expect("handled");

    let outbox = channel.outbox();
    let reply = &outbox.last().expect("a reply").text;
    assert!(
        reply.contains("Stopped 3 approval(s)"),
        "the reply must name the real count the port returned: {reply:?}"
    );
    assert_eq!(
        approvals.revocations.load(Ordering::SeqCst),
        1,
        "REVOKE reached the approval port exactly once"
    );
}

// ------------------------------------------------------------------ B12

/// BALANCE consults the port — exactly once, only for BALANCE — and the reply
/// carries the port's own words, so a hardcoded string cannot pass.
#[tokio::test]
async fn b12_balance_consults_the_port_and_answers_honestly() {
    let bench = bench(&ProposerChoice::Panicking);
    bench
        .dispatcher
        .handle(bench.raw("07700900123", "BALANCE"))
        .await
        .expect("handled");
    assert!(bench.last_reply().contains(BALANCE_SENTINEL));
    assert_eq!(bench.ledger.describes.load(Ordering::SeqCst), 1);

    // HELP and STOP must not spend a balance question.
    for text in ["HELP", "STOP", "START"] {
        bench
            .dispatcher
            .handle(bench.raw("07700900123", text))
            .await
            .expect("handled");
    }
    assert_eq!(
        bench.ledger.describes.load(Ordering::SeqCst),
        1,
        "only BALANCE may consult the balance read"
    );
    // And the zero-unit property, witnessed directly: not one of these control
    // commands reserved a usage unit.
    assert_eq!(
        bench.ledger.reserves.load(Ordering::SeqCst),
        0,
        "a safety exit must never reserve a unit — it cannot be paywalled"
    );
    // And no digit-only invention: the sentinel is not a number.
    assert!(!bench.last_reply().chars().all(|c| c.is_ascii_digit()));
}

// ------------------------------------------------------------------ B9

/// An unbound address is refused before the wire exists to be called.
///
/// The wire is set panicking-on-touch: a "resolve unbound to a guest" or a
/// "look them up anyway" implementation dies here rather than passing quietly.
#[tokio::test]
async fn b9_an_unbound_address_is_refused_before_any_wire_call() {
    let mut bench = bench(&ProposerChoice::Panicking);
    // Replace the wire with one that treats ANY touch as a failure.
    let panicking = Arc::new(CountingWire {
        panicking: true,
        ..CountingWire::default()
    });
    bench.dispatcher = Dispatcher::new(
        Arc::clone(&bench.channel),
        Arc::new(FixedDirectory(vec![("+447700900123", "lucy")])),
        Arc::new(FixedCredentials),
        Arc::new(UnmeteredLedger),
        Arc::new(PanickingProposer),
        Arc::new(InMemorySuppression::default()),
        Arc::new(FixedWireFactory(panicking)),
        Arc::new(StubApprovals::default()),
        Arc::new(StubEvidence::default()),
        Arc::new(MemoryContinuation::new()),
    );

    // A number the directory does not know — STATUS needs identity and a wire,
    // so both refusal layers are in play.
    bench
        .dispatcher
        .handle(bench.raw("07700111222", "STATUS"))
        .await
        .expect("handled");
    assert!(
        bench.last_reply().contains("recognize"),
        "the stranger gets a deterministic refusal: {}",
        bench.last_reply()
    );
}

// ------------------------------------------------------------------ B13

/// Unrecognized text reaches the proposer — exactly once — and mutates nothing.
///
/// Zero mutations alone is not enough: it also holds for a message the
/// dispatcher swallowed. The proposer's own call count proves the text was
/// judged rather than lost.
#[tokio::test]
async fn b13_unrecognized_text_is_judged_once_and_mutates_nothing() {
    let bench = bench(&ProposerChoice::Unclear);
    bench
        .dispatcher
        .handle(bench.raw("07700900123", "please sort out a room for tuesday"))
        .await
        .expect("handled");

    assert!(
        bench.last_reply().contains("Reply HELP"),
        "the reply points at HELP: {}",
        bench.last_reply()
    );
    assert_eq!(
        bench
            .proposer_calls
            .as_ref()
            .expect("counting proposer")
            .0
            .load(Ordering::SeqCst),
        1,
        "the text must REACH the proposer and be judged, exactly once"
    );
    assert_eq!(
        bench.wire.mutation_count(),
        0,
        "an Unclear judgment must submit nothing"
    );
}

// ------------------------------------------------------------------ the grammar

/// The scripted grammar's near-misses are Unclear, not guesses.
#[tokio::test]
async fn the_scripted_grammar_refuses_near_misses() {
    let bench = bench(&ProposerChoice::Scripted);
    for text in [
        "BOOK date=2026-09-10 from=14:00 to=17:00 people=20 accessible=yes", // missing max
        "BOOK date=2026-09-10 from=14:00 to=17:00 people=20 accessible=yes max=5000 color=red", // unknown key
        "BOOK date=2026-09-10 date=2026-09-11 from=14:00 to=17:00 people=20 accessible=yes max=5000", // duplicate
        "BOOK date=2026-09-10 from=14:00 to=17:00 people=lots accessible=yes max=5000", // bad number
        "BOOK date=2026-09-10 from=14:00 to=17:00 people=20 accessible=maybe max=5000", // bad bool
        "BOOK date=tomorrow from=14:00 to=17:00 people=20 accessible=yes max=5000", // date shape
        "BOOK date=2026-09-10 from=noon to=17:00 people=20 accessible=yes max=5000", // time shape
        "BOOK date=2026-09-10 from=14:00 to=17:00 people=0 accessible=yes max=5000", // zero people
    ] {
        bench
            .dispatcher
            .handle(bench.raw("07700900123", text))
            .await
            .expect("handled");
        assert!(
            bench.last_reply().contains("Reply HELP"),
            "{text:?} must be Unclear, got: {}",
            bench.last_reply()
        );
    }
    assert_eq!(
        bench.wire.mutation_count(),
        0,
        "a half-understood BOOK must never create anything"
    );

    // And the well-formed one, keys shuffled, raises a challenge to approve — and
    // still creates NOTHING (approve-first: the booking waits for YES).
    bench
        .dispatcher
        .handle(bench.raw(
            "07700900123",
            "book max=5000 people=20 to=17:00 accessible=yes from=14:00 date=2026-09-10",
        ))
        .await
        .expect("handled");
    assert!(
        bench.last_reply().contains("Reply YES"),
        "the complete request, in any key order, raises a challenge: {}",
        bench.last_reply()
    );
    assert_eq!(
        bench.wire.mutation_count(),
        0,
        "BOOK creates nothing — approve-first books only after a YES"
    );
}

// ------------------------------------------------------------------ the STOP lie

/// A suppression store whose disk is gone.
#[derive(Debug)]
struct BrokenSuppression;

impl SuppressionStore for BrokenSuppression {
    fn is_suppressed(&self, _: &ChannelAddress) -> bool {
        false
    }
    fn suppress(&self, _: &ChannelAddress) -> Result<(), String> {
        Err("disk full".to_owned())
    }
    fn allow(&self, _: &ChannelAddress) -> Result<(), String> {
        Err("disk full".to_owned())
    }
}

/// A failed persist must reach the human as "NOT stopped" — the review's HIGH:
/// the first implementation confirmed STOP while the write had already failed,
/// a success that lasted exactly until the next restart. This mutation-verified
/// witness is what makes that regression loud (reverting `suppress()` to
/// memory-first-ignore-errors passes every other test in this workspace).
#[tokio::test]
async fn a_failed_stop_is_reported_as_not_stopped() {
    let channel = Arc::new(SmsSimulator::new(
        ChannelConfig::default(),
        Arc::new(BrokenSuppression) as Arc<dyn SuppressionStore>,
    ));
    let dispatcher = Dispatcher::new(
        Arc::clone(&channel),
        Arc::new(PanickingDirectory),
        Arc::new(FixedCredentials),
        Arc::new(UnmeteredLedger),
        Arc::new(PanickingProposer),
        Arc::new(BrokenSuppression),
        Arc::new(PanickingFactory),
        Arc::new(StubApprovals::default()),
        Arc::new(StubEvidence::default()),
        Arc::new(MemoryContinuation::new()),
    );
    dispatcher
        .handle(RawInbound {
            identity: InboundIdentity::new("sim", "acct", "stop-fail"),
            channel: ChannelKind::SmsSimulator,
            from: "07700 900123".to_owned(),
            body: "STOP".to_owned(),
            received_at_ms: 0,
            evidence: TransportEvidence::new("sim", "07700 900123", true),
        })
        .await
        .expect("handled");
    let outbox = channel.outbox();
    let reply = &outbox.last().expect("a reply").text;
    assert!(
        reply.contains("NOT stopped"),
        "a failed persist must not be confirmed as stopped: {reply}"
    );
    assert!(
        !reply.contains("I won't act on your behalf"),
        "the success text must be absent: {reply}"
    );
}

/// And the store itself: persist-first means a failed write leaves memory
/// unchanged, so `is_suppressed` never claims a state the disk does not hold.
#[test]
fn file_suppression_does_not_commit_what_it_could_not_persist() {
    let dir = tempfile::tempdir().expect("tempdir");
    let store =
        townhall_orchestrator::FileSuppression::open(dir.path().join("stop.list")).expect("open");
    let lucy =
        ChannelAddress::parse("+447700900123", townhall_channel::Region::Gb).expect("address");

    // Make the parent unwritable, so the staged write fails.
    let mut permissions = std::fs::metadata(dir.path()).expect("meta").permissions();
    std::os::unix::fs::PermissionsExt::set_mode(&mut permissions, 0o555);
    std::fs::set_permissions(dir.path(), permissions.clone()).expect("chmod");

    let outcome = store.suppress(&lucy);
    assert!(
        outcome.is_err(),
        "an unwritable disk must surface: {outcome:?}"
    );
    assert!(
        !store.is_suppressed(&lucy),
        "memory must not hold a state the disk refused — that state evaporates \
         at restart, which is the whole failure"
    );

    // Restore writability so the tempdir can clean up (and prove recovery).
    std::os::unix::fs::PermissionsExt::set_mode(&mut permissions, 0o755);
    std::fs::set_permissions(dir.path(), permissions).expect("chmod back");
    store.suppress(&lucy).expect("a healthy disk suppresses");
    assert!(store.is_suppressed(&lucy));
}

// ------------------------------------------------------------------ binding drift

/// A directory whose binding can be changed mid-test.
#[derive(Debug, Default)]
struct SwappableDirectory(std::sync::Mutex<Option<&'static str>>);

impl PrincipalDirectory for SwappableDirectory {
    fn resolve(&self, _: &ChannelAddress) -> Option<PrincipalId> {
        self.0
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .map(PrincipalId::new)
    }
}

/// A wire that accepts a cancellation and PANICS if convergence runs — the
/// witness that a drifted follow-up's turn never started.
struct AcceptingWire;

#[async_trait]
impl BookingWire for AcceptingWire {
    async fn create(
        &self,
        _: &BookingId,
        _: &BookingRequirements,
    ) -> Result<Projection, GatewayError> {
        unreachable!()
    }
    async fn read(&self, _: &BookingId) -> Result<Projection, GatewayError> {
        unreachable!()
    }
    async fn cancellable(&self) -> Result<Vec<Projection>, GatewayError> {
        Ok(vec![Projection {
            id: "sms-test".to_owned(),
            version: 4,
            state: "Booked".to_owned(),
            requirements: townhall_gateway::dto::Requirements {
                purpose: "p".to_owned(),
                requested_date: "2026-09-10".to_owned(),
                from: "14:00".to_owned(),
                to: "17:00".to_owned(),
                attendees: 20,
                wheelchair_accessible: true,
                max_fee_pence: 5_000,
            },
            selected_venue: None,
            booking_ref: Some("TH-1".to_owned()),
            available_behaviours: vec!["Cancel".to_owned()],
            checkout_url: None,
        }])
    }
    async fn by_reference(&self, _: &CouncilBookingRef) -> Result<Vec<Projection>, GatewayError> {
        unreachable!()
    }
    async fn venues(&self) -> Result<Vec<VenueRow>, GatewayError> {
        unreachable!()
    }
    async fn propose_at(
        &self,
        _: &BookingId,
        _: u64,
        _: &str,
        _: Option<serde_json::Value>,
    ) -> Result<Turn, GatewayError> {
        Ok(Turn::Accepted {
            retry_after: std::time::Duration::from_millis(1),
        })
    }
    async fn converge(
        &self,
        _: &BookingId,
        _: std::time::Duration,
    ) -> Result<Projection, GatewayError> {
        panic!("a drifted follow-up ran its turn");
    }
}

struct AcceptingFactory;

impl WireFactory for AcceptingFactory {
    fn reader_for(&self, _: &str, _principal: &PrincipalId) -> Arc<dyn BookingWire> {
        Arc::new(AcceptingWire)
    }

    fn changer_for(
        &self,
        _: &str,
        _principal: &PrincipalId,
        _reference: &str,
    ) -> Arc<dyn BookingWire> {
        Arc::new(AcceptingWire)
    }
}

/// A follow-up whose binding drifted between queueing and draining is dropped
/// BEFORE any wire exists — otherwise the dispatcher authenticates as the OLD
/// principal and sends their booking reference to whoever holds the number now.
/// (The review's sharpest scenario; converge panics, so a wrong implementation
/// dies rather than passes.)
#[tokio::test]
async fn a_drifted_binding_drops_the_followup_before_any_wire() {
    let suppression: Arc<InMemorySuppression> = Arc::new(InMemorySuppression::default());
    let channel = Arc::new(SmsSimulator::new(
        ChannelConfig::default(),
        Arc::clone(&suppression) as Arc<dyn SuppressionStore>,
    ));
    let directory = Arc::new(SwappableDirectory::default());
    *directory
        .0
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner) = Some("lucy");

    // The booking "cancel it" resolves to was approved through this dispatcher —
    // seed the grant that lets it be cancelled, since cancellation now presents a
    // reference an approval issued (§23.1).
    let continuations = Arc::new(MemoryContinuation::new());
    continuations.seed(Continuation {
        principal: PrincipalId::new("lucy"),
        challenge_id: "ch-drift".to_owned(),
        booking_id: BookingId::new("sms-test"),
        request: Request::Book(BookingRequest {
            date: "2026-09-10".to_owned(),
            from: "14:00".to_owned(),
            to: "17:00".to_owned(),
            people: 20,
            accessible: true,
            max_pence: 5_000,
        }),
        address_revealed: "+447700900123".to_owned(),
        region: "Gb".to_owned(),
        reference: Some("ref-drift".to_owned()),
        booked: true,
    });

    let dispatcher = Dispatcher::new(
        Arc::clone(&channel),
        Arc::clone(&directory) as Arc<dyn PrincipalDirectory>,
        Arc::new(FixedCredentials),
        Arc::new(UnmeteredLedger),
        Arc::new(ScriptedProposer),
        suppression,
        Arc::new(AcceptingFactory),
        Arc::new(StubApprovals::default()),
        Arc::new(StubEvidence::default()),
        continuations,
    );

    // "cancel it" → one candidate → Accepted → a follow-up is queued.
    dispatcher
        .handle(RawInbound {
            identity: InboundIdentity::new("sim", "acct", "drift-1"),
            channel: ChannelKind::SmsSimulator,
            from: "07700 900123".to_owned(),
            body: "cancel it".to_owned(),
            received_at_ms: 0,
            evidence: TransportEvidence::new("sim", "07700 900123", true),
        })
        .await
        .expect("handled");
    let sent_before = channel.outbox().len();

    // The number changes hands before the queue is drained.
    *directory
        .0
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner) = Some("priya");

    dispatcher.run_followups().await;

    // Nothing ran (converge panics if it did) and nothing was sent.
    assert_eq!(
        channel.outbox().len(),
        sent_before,
        "a drifted follow-up must send nothing: {:?}",
        channel.outbox().last()
    );
}

// ------------------------------------------------------------------ W7

/// A stateful mock council: `create` makes a Draft→Created booking (idempotent
/// on re-create), and `propose_at` walks it Created→VenueSelected→
/// AwaitingBooking→Booked. `creates` counts only NEW creates, so a resumed walk
/// that re-creates
/// an existing booking is caught as a double-book.
#[derive(Default)]
struct MockCouncil {
    bookings: std::sync::Mutex<std::collections::HashMap<String, (u64, String)>>,
    creates: AtomicUsize,
}

fn mock_requirements() -> townhall_gateway::dto::Requirements {
    townhall_gateway::dto::Requirements {
        purpose: "p".to_owned(),
        requested_date: "2026-09-10".to_owned(),
        from: "14:00".to_owned(),
        to: "17:00".to_owned(),
        attendees: 20,
        wheelchair_accessible: true,
        max_fee_pence: 5_000,
    }
}

fn mock_projection(id: &str, version: u64, state: &str) -> Projection {
    Projection {
        id: id.to_owned(),
        version,
        state: state.to_owned(),
        requirements: mock_requirements(),
        selected_venue: None,
        booking_ref: (state == "Booked").then(|| "TH-W7".to_owned()),
        available_behaviours: Vec::new(),
        checkout_url: None,
    }
}

#[async_trait]
impl BookingWire for MockCouncil {
    async fn create(
        &self,
        id: &BookingId,
        _: &BookingRequirements,
    ) -> Result<Projection, GatewayError> {
        let mut bookings = self.bookings.lock().unwrap();
        if let Some((version, _)) = bookings.get(id.as_str()) {
            // Idempotent: the message-derived id already exists.
            return Err(GatewayError::Existing { current: *version });
        }
        self.creates.fetch_add(1, Ordering::SeqCst);
        bookings.insert(id.as_str().to_owned(), (1, "Created".to_owned()));
        Ok(mock_projection(id.as_str(), 1, "Created"))
    }
    async fn read(&self, id: &BookingId) -> Result<Projection, GatewayError> {
        let bookings = self.bookings.lock().unwrap();
        let (version, state) = bookings
            .get(id.as_str())
            .ok_or(GatewayError::UnknownBooking)?;
        Ok(mock_projection(id.as_str(), *version, state))
    }
    async fn cancellable(&self) -> Result<Vec<Projection>, GatewayError> {
        Ok(Vec::new())
    }
    async fn by_reference(&self, _: &CouncilBookingRef) -> Result<Vec<Projection>, GatewayError> {
        Ok(Vec::new())
    }
    async fn venues(&self) -> Result<Vec<VenueRow>, GatewayError> {
        Ok(vec![VenueRow {
            venue_id: "TH-A".to_owned(),
            slot_id: "SLOT-A".to_owned(),
            capacity: 30,
            accessible: true,
            fee_pence: 4_500,
            available: true,
        }])
    }
    async fn propose_at(
        &self,
        id: &BookingId,
        expected_version: u64,
        behaviour: &str,
        _: Option<serde_json::Value>,
    ) -> Result<Turn, GatewayError> {
        let mut bookings = self.bookings.lock().unwrap();
        let (version, state) = bookings
            .get_mut(id.as_str())
            .ok_or(GatewayError::UnknownBooking)?;
        if *version != expected_version {
            return Err(GatewayError::Contended);
        }
        let next = match behaviour {
            "select-venue" => "VenueSelected",
            "verify-slot" => "AwaitingBooking",
            "book" => "Booked",
            other => panic!("unexpected behaviour {other}"),
        };
        *version += 1;
        next.clone_into(state);
        Ok(Turn::Committed {
            state: next.to_owned(),
            version: *version,
        })
    }
    async fn converge(
        &self,
        _: &BookingId,
        _: std::time::Duration,
    ) -> Result<Projection, GatewayError> {
        Err(GatewayError::NotConverged { attempts: 0 })
    }
}

struct MockFactory(Arc<MockCouncil>);

impl WireFactory for MockFactory {
    fn reader_for(&self, _: &str, _: &PrincipalId) -> Arc<dyn BookingWire> {
        Arc::clone(&self.0) as Arc<dyn BookingWire>
    }
    fn changer_for(&self, _: &str, _: &PrincipalId, _: &str) -> Arc<dyn BookingWire> {
        Arc::clone(&self.0) as Arc<dyn BookingWire>
    }
}

/// W7 — a crash between YES and Booked resumes and books exactly once.
///
/// A booking Lucy approved was created at the council, then the process died
/// before the walk reached Booked: the durable continuation holds it
/// `reference: Some`, `booked: false`. A reborn dispatcher's `resume()` finishes
/// it — and creates NO second booking, because the message-derived id is
/// idempotent (`create` lands on `Existing`). A second `resume()` is a no-op.
#[tokio::test]
async fn w7_a_crash_between_yes_and_booked_resumes_and_books_once() {
    let council = Arc::new(MockCouncil::default());
    // The crashed process had created the booking (Created) before dying — count
    // that create as the process's, not resume's.
    council
        .bookings
        .lock()
        .unwrap()
        .insert("sms-w7".to_owned(), (1, "Created".to_owned()));

    let continuations = Arc::new(MemoryContinuation::new());
    continuations.seed(Continuation {
        principal: PrincipalId::new("lucy"),
        challenge_id: "ch-w7".to_owned(),
        booking_id: BookingId::new("sms-w7"),
        request: Request::Book(BookingRequest {
            date: "2026-09-10".to_owned(),
            from: "14:00".to_owned(),
            to: "17:00".to_owned(),
            people: 20,
            accessible: true,
            max_pence: 5_000,
        }),
        address_revealed: "+447700900123".to_owned(),
        region: "Gb".to_owned(),
        reference: Some("ref-w7".to_owned()),
        booked: false,
    });

    let suppression: Arc<InMemorySuppression> = Arc::new(InMemorySuppression::default());
    let channel = Arc::new(SmsSimulator::new(
        ChannelConfig::default(),
        Arc::clone(&suppression) as Arc<dyn SuppressionStore>,
    ));
    let dispatcher = Dispatcher::new(
        channel,
        Arc::new(FixedDirectory(vec![("+447700900123", "lucy")])),
        Arc::new(FixedCredentials),
        Arc::new(UnmeteredLedger),
        Arc::new(PanickingProposer),
        suppression,
        Arc::new(MockFactory(Arc::clone(&council))),
        Arc::new(StubApprovals::default()),
        Arc::new(StubEvidence::default()),
        Arc::clone(&continuations) as Arc<dyn townhall_orchestrator::ContinuationStore>,
    );

    dispatcher.resume().await;

    assert_eq!(
        council.bookings.lock().unwrap().get("sms-w7").unwrap().1,
        "Booked",
        "resume walks the owed booking to Booked"
    );
    assert_eq!(
        council.creates.load(Ordering::SeqCst),
        0,
        "resume must not create a SECOND booking — the id is idempotent"
    );

    // A second resume is a no-op: the continuation is now booked.
    dispatcher.resume().await;
    assert_eq!(council.creates.load(Ordering::SeqCst), 0);
    assert!(
        continuations.all().iter().all(|c| c.booked),
        "the completed booking is marked booked, off the resume list"
    );
}
