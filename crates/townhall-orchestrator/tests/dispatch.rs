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
    ChannelAddress, ChannelConfig, ChannelKind, InboundIdentity, InboundMessage, MessageReceipt,
    RawInbound, SmsSimulator, SuppressionStore, TransportEvidence, simulator::InMemorySuppression,
};
use townhall_gateway::{GatewayError, Projection, Turn, VenueRow};
use townhall_orchestrator::{
    BookingWire, CredentialSource, Dispatcher, NoLedgerYet, PrincipalDirectory, ProjectedContext,
    Proposed, Proposer, ScriptedProposer, UsageBalance, WireFactory,
};

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
    async fn propose(&self, _: &ProjectedContext, _: &InboundMessage) -> Proposed {
        panic!("a control command reached the proposer");
    }
}

/// Answers `Unclear` and counts — for proving the proposer WAS consulted,
/// exactly once, when that is the claim.
#[derive(Default)]
struct UnclearProposer(AtomicUsize);

#[async_trait]
impl Proposer for UnclearProposer {
    async fn propose(&self, _: &ProjectedContext, _: &InboundMessage) -> Proposed {
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
    fn wire_for(&self, _token: &str) -> Arc<dyn BookingWire> {
        Arc::clone(&self.0) as Arc<dyn BookingWire>
    }
}

/// A balance port that counts and answers a nonsense sentinel a hardcoded
/// string could not reproduce.
#[derive(Default)]
struct SentinelBalance(AtomicUsize);

const BALANCE_SENTINEL: &str = "UNMETERED-SENTINEL-7f3a";

impl UsageBalance for SentinelBalance {
    fn describe(&self, _: &PrincipalId) -> String {
        self.0.fetch_add(1, Ordering::SeqCst);
        BALANCE_SENTINEL.to_owned()
    }
}

// ------------------------------------------------------------------ harness

struct Bench {
    dispatcher: Dispatcher<SmsSimulator>,
    channel: Arc<SmsSimulator>,
    wire: Arc<CountingWire>,
    balance: Arc<SentinelBalance>,
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
    let balance = Arc::new(SentinelBalance::default());
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
        Arc::clone(&balance) as Arc<dyn UsageBalance>,
        proposer_arc,
        suppression,
        Arc::new(FixedWireFactory(Arc::clone(&wire))),
    );
    Bench {
        dispatcher,
        channel,
        wire,
        balance,
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

/// Every control command answers with ZERO proposer entries and ZERO wire
/// calls — the panicking proposer makes the first half loud, the counting wire
/// makes the second half a number.
#[tokio::test]
async fn b11_control_commands_reach_nothing() {
    let bench = bench(&ProposerChoice::Panicking);
    for (text, expect) in [
        ("HELP", "BOOK date="),
        ("STOP", "Automated messages stopped"),
        ("START", "Automated messages resumed"),
        ("REVOKE", "M7"),
        ("BALANCE", BALANCE_SENTINEL),
    ] {
        bench
            .dispatcher
            .handle(bench.raw("07700 900123", text))
            .await
            .expect("handled");
        let reply = bench.last_reply();
        assert!(
            reply.contains(expect),
            "{text}: expected {expect:?} in {reply:?}"
        );
    }
    assert_eq!(bench.wire.count(), 0, "a control command touched the wire");
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
    assert_eq!(bench.balance.0.load(Ordering::SeqCst), 1);

    // HELP and STOP must not spend a balance question.
    for text in ["HELP", "STOP", "START"] {
        bench
            .dispatcher
            .handle(bench.raw("07700900123", text))
            .await
            .expect("handled");
    }
    assert_eq!(
        bench.balance.0.load(Ordering::SeqCst),
        1,
        "only BALANCE may consult the balance port"
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
        Arc::new(NoLedgerYet),
        Arc::new(PanickingProposer),
        Arc::new(InMemorySuppression::default()),
        Arc::new(FixedWireFactory(panicking)),
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

    // And the well-formed one, keys shuffled, DOES reach the wire.
    bench
        .dispatcher
        .handle(bench.raw(
            "07700900123",
            "book max=5000 people=20 to=17:00 accessible=yes from=14:00 date=2026-09-10",
        ))
        .await
        .expect("handled");
    assert_eq!(
        bench.wire.mutation_count(),
        1,
        "the complete request, in any key order, creates"
    );
}
