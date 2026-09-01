//! Slice C's gate: the effect protocol, against an in-process council.
//!
//! Every failure here means the *protocol* is wrong. Slice D puts the same
//! protocol over HTTP, where a failure could also mean the client is
//! misconfigured — which is exactly why these run first.

use bld_kernel::{BoundaryOutcome, Capability, Verified};
use bld_types::{
    ActorId, AvailabilityGrant, BookingId, BookingRequirements, CouncilBookingRef, EffectAttempt,
    EffectIntentId, Money, PrincipalId, Provenance, SlotId, TimeWindow, VenueId,
};
use std::{path::PathBuf, sync::Arc};
use tempfile::TempDir;
use townhall_domain::{
    BookingEffect, BookingError, BookingProposal, EffectStatus, OperationKind, VenueFacts,
    VerifiedAuthority, VerifiedProviderFact,
};
use townhall_service::{
    Attended, Coordinator, Reconciliation, ServiceError,
    fake::{
        CouncilVerifier, ExecuteGate, FAKE_GRANT, FakeCouncil, FixedAvailability, ObservedCouncil,
        Script, WireOp,
    },
};
use townhall_store::{
    AuditEvent, BookingRepository, ClaimedEffect, EscalationWrite, FinalizeEffect, FinalizedEffect,
    HandedOffEffect, HandoffEffect, NewBooking, PrepareEffect, PreparedEffect,
    SqliteBookingRepository, StoreError, TransitionAudit, derive_effect_intent_id,
};

// --------------------------------------------------------------- fixtures

/// The version `Book` is proposed from: create (0), `SelectVenue` (1), `VerifySlot` (2).
const AT_BOOK: u64 = 2;

fn requirements() -> BookingRequirements {
    BookingRequirements {
        purpose: "community meeting".to_owned(),
        requested_date: "2026-08-20".to_owned(),
        time_window: TimeWindow {
            from: "13:00".to_owned(),
            to: "17:00".to_owned(),
        },
        attendees: 20,
        wheelchair_accessible: true,
        max_fee: Money::from_pence(5_000),
    }
}

fn facts() -> VenueFacts {
    VenueFacts {
        venue_id: VenueId::new("TH-A"),
        slot_id: SlotId::new("SLOT-A"),
        capacity: 30,
        wheelchair_accessible: true,
        fee: Money::from_pence(4_500),
        available: true,
    }
}

fn authority() -> VerifiedAuthority {
    VerifiedAuthority {
        principal: PrincipalId::new("lucy"),
        actor: ActorId::new("townhall-agent"),
        max_fee: Money::from_pence(5_000),
        may_book: true,
        may_cancel: true,
    }
}

fn select() -> BookingProposal {
    BookingProposal::SelectVenue {
        venue_id: VenueId::new("TH-A"),
        slot_id: SlotId::new("SLOT-A"),
    }
}

fn book_plan() -> BookingEffect {
    BookingEffect::Book {
        principal: PrincipalId::new("lucy"),
        attendees: 20,
        facts: facts(),
        // The grant the availability source issued, not one this test invented.
        // The plan the coordinator derives carries whatever the observation
        // carried, so naming a different token here would assert a plan the
        // boundary never builds.
        grant: AvailabilityGrant::new(FAKE_GRANT),
    }
}

type Sut = Coordinator<SqliteBookingRepository, FakeCouncil, CouncilVerifier, FixedAvailability>;

struct Harness {
    temp: TempDir,
    path: PathBuf,
    repo: Arc<SqliteBookingRepository>,
    council: Arc<FakeCouncil>,
    coordinator: Sut,
    reconciliation: Reconciliation<
        SqliteBookingRepository,
        FakeCouncil,
        CouncilVerifier,
        FixedAvailability,
        FakeCouncil,
    >,
    /// The store's clock, movable — reconciliation cadences are real times, and
    /// this suite's no-sleep rule means the clock moves instead of the test
    /// waiting.
    clock: Arc<ProtocolClock>,
}

/// A movable store clock for this suite.
#[derive(Debug)]
struct ProtocolClock(std::sync::atomic::AtomicI64);

impl ProtocolClock {
    fn advance(&self, by_ms: i64) {
        self.0.fetch_add(by_ms, std::sync::atomic::Ordering::SeqCst);
    }
}

impl townhall_store::StoreClock for ProtocolClock {
    fn now_ms(&self) -> i64 {
        self.0.load(std::sync::atomic::Ordering::SeqCst)
    }
}

async fn harness() -> Harness {
    let temp = TempDir::new().expect("temp dir");
    let path = temp.path().join("townhall.sqlite");
    let clock = Arc::new(ProtocolClock(std::sync::atomic::AtomicI64::new(
        1_000_000_000,
    )));
    let repo = Arc::new(
        SqliteBookingRepository::open_with(
            &path,
            townhall_store::DEFAULT_EFFECT_TTL_MS,
            Arc::clone(&clock) as Arc<dyn townhall_store::StoreClock>,
        )
        .await
        .expect("repository should open"),
    );
    let council = Arc::new(FakeCouncil::new());
    let coordinator = Coordinator::new(
        Arc::clone(&repo),
        Arc::clone(&council),
        Arc::new(CouncilVerifier),
        Arc::new(FixedAvailability::new(facts())),
    );
    let reconciliation = Reconciliation::new(
        Arc::new(Coordinator::new(
            Arc::clone(&repo),
            Arc::clone(&council),
            Arc::new(CouncilVerifier),
            Arc::new(FixedAvailability::new(facts())),
        )),
        Arc::clone(&council),
    );
    Harness {
        temp,
        path,
        repo,
        council,
        coordinator,
        reconciliation,
        clock,
    }
}

/// Walk a fresh booking to `AwaitingBooking` — the state `Book` departs from.
async fn awaiting(h: &Harness, id: &BookingId, requirements: BookingRequirements) {
    h.repo
        .create(NewBooking {
            id: id.clone(),
            requirements,
        })
        .await
        .expect("create");
    for proposal in [select(), BookingProposal::VerifySlot] {
        let name = proposal.name();
        let outcome = h
            .coordinator
            .propose(id, proposal, &authority())
            .await
            .expect("no service error");
        assert!(
            matches!(outcome, BoundaryOutcome::Committed(_)),
            "setup step {name} must commit, got {outcome:?}"
        );
    }
}

async fn in_flight_effect(h: &Harness, id: &BookingId) -> EffectIntentId {
    h.repo
        .load(id)
        .await
        .expect("load")
        .active_effect
        .expect("an effect must be in flight")
}

// ------------------------------------------------------------ happy path

/// The acceptance scenario end to end — and the audit trail attributing each step
/// to the door that drove it.
#[tokio::test]
async fn lucy_books_a_room() {
    let h = harness().await;
    let id = BookingId::new("BKG-HAPPY");
    awaiting(&h, &id, requirements()).await;

    let outcome = h
        .coordinator
        .propose(&id, BookingProposal::Book, &authority())
        .await
        .expect("no service error");

    let BoundaryOutcome::Committed(aggregate) = outcome else {
        panic!("Book must commit through to Booked, got {outcome:?}");
    };
    assert_eq!(aggregate.state.name(), "Booked");
    assert!(
        aggregate.booking_ref.is_some(),
        "the council's reference must be recorded"
    );
    assert_eq!(
        aggregate.active_effect, None,
        "a settled booking waits on nothing"
    );

    assert_eq!(h.council.call_count(), 1);
    assert_eq!(h.council.booking_count(), 1);

    let intent = h
        .repo
        .load_effect(&derive_effect_intent_id(&id, OperationKind::Book, AT_BOOK))
        .await
        .expect("the intent");
    assert_eq!(intent.status, EffectStatus::Confirmed);
    assert_eq!(intent.provider_reference, aggregate.booking_ref);

    // The trail says who drove each step. Before ADR-017 the confirmation had to
    // claim a proposal caused it.
    let trail: Vec<_> = h
        .repo
        .audit_events(&id)
        .await
        .expect("audit")
        .into_iter()
        .map(|row| (row.driver_kind, row.driver_detail, row.to_state))
        .collect();
    assert_eq!(
        trail,
        vec![
            (
                Provenance::Proposal,
                "SelectVenue".to_owned(),
                "VenueSelected".to_owned()
            ),
            (
                Provenance::Proposal,
                "VerifySlot".to_owned(),
                "AwaitingBooking".to_owned()
            ),
            (
                Provenance::Proposal,
                "Book".to_owned(),
                "BookingInProgress".to_owned()
            ),
            (
                Provenance::Fact,
                "BookingExists".to_owned(),
                "Booked".to_owned()
            ),
        ],
        "the fact door, not Lucy, is what confirmed the booking"
    );
}

/// Both external routes execute, not just booking.
#[tokio::test]
async fn lucy_cancels_a_confirmed_booking() {
    let h = harness().await;
    let id = BookingId::new("BKG-CANCEL");
    awaiting(&h, &id, requirements()).await;
    h.coordinator
        .propose(&id, BookingProposal::Book, &authority())
        .await
        .expect("book");

    let outcome = h
        .coordinator
        .propose(
            &id,
            BookingProposal::Cancel {
                reason: "changed mind".to_owned(),
            },
            &authority(),
        )
        .await
        .expect("no service error");

    let BoundaryOutcome::Committed(aggregate) = outcome else {
        panic!("Cancel must commit through to Cancelled, got {outcome:?}");
    };
    assert_eq!(aggregate.state.name(), "Cancelled");
    assert_eq!(aggregate.active_effect, None);
    // The reference of what was cancelled is kept — the convergence reading of
    // this state depends on it.
    assert!(aggregate.booking_ref.is_some());
    assert_eq!(
        h.council.call_count(),
        2,
        "one booking call, one cancellation"
    );
}

// ----------------------------------------------------- persist before effect

/// Crash between Phase A's commit and the call. Restart finds the intent durable
/// and the council **never contacted**.
#[tokio::test]
async fn a_crash_before_the_call_leaves_no_external_consequence() {
    let h = harness().await;
    let id = BookingId::new("BKG-CRASH-BEFORE");
    awaiting(&h, &id, requirements()).await;

    // Phase A commits, then the process dies before the request leaves. A council
    // that answers nothing stands in for that: either way Phase A is durable and
    // Phase C never ran, and the assertion that separates this from the
    // crash-after case is the call log.
    h.council
        .script([Script::GoQuiet("process died before the request left")]);

    let outcome = h
        .coordinator
        .propose(&id, BookingProposal::Book, &authority())
        .await
        .expect("no service error");
    assert!(
        outcome.is_unresolved(),
        "an unanswered call is neither success nor failure, got {outcome:?}"
    );

    // Reopen the database, as a restart would.
    let reopened = SqliteBookingRepository::open(&h.path)
        .await
        .expect("reopen after restart");
    let aggregate = reopened.load(&id).await.expect("load after restart");
    assert_eq!(
        aggregate.state.name(),
        "BookingInProgress",
        "recovery must find the booking exactly where it was left"
    );
    let effect = aggregate
        .active_effect
        .clone()
        .expect("and must find what it is waiting on");
    let intent = reopened.load_effect(&effect).await.expect("the intent");
    // `Unknown`, not `Prepared` — and the change is the point (ADR-019 §4 /
    // decision 5). This fixture scripts a call that got no answer, and Phase B
    // now records the attempt BEFORE the wire, so "a call began" is durable and
    // `Prepared` finally means what it says: never attempted. The true
    // crash-BEFORE-the-call case — where `Prepared` is the assertion — needs a
    // killable BLD process, and gets one in E's subprocess harness.
    assert_eq!(
        intent.status,
        EffectStatus::Unknown,
        "a call began and nothing came back; the row must say so"
    );
    assert!(
        h.repo
            .escalated_unresolved(10)
            .await
            .expect("queue")
            .is_empty(),
        "one lost answer is nowhere near a human's problem yet"
    );
    assert_eq!(intent.canonical_plan.operation_kind(), OperationKind::Book);
    assert_eq!(
        intent.canonical_plan,
        book_plan(),
        "the plan is durable too"
    );
}

/// Crash after the call, before the local commit. The council *was* asked, so the
/// booking may exist — and the boundary knows that it might. That is the whole of
/// ADR-014, and the call count is what distinguishes it from the case above.
#[tokio::test]
async fn a_crash_after_the_call_leaves_a_recoverable_record() {
    let h = harness().await;
    let id = BookingId::new("BKG-CRASH-AFTER");
    awaiting(&h, &id, requirements()).await;

    // The council commits and the answer is lost coming home. Indistinguishable,
    // from here, from a crash in the same window.
    h.council.script([Script::GoQuiet(
        "committed at the council, response dropped",
    )]);

    let outcome = h
        .coordinator
        .propose(&id, BookingProposal::Book, &authority())
        .await
        .expect("no service error");
    assert!(outcome.is_unresolved());

    assert_eq!(
        h.council.call_count(),
        1,
        "the request went out; we simply never learned the answer"
    );

    let reopened = SqliteBookingRepository::open(&h.path)
        .await
        .expect("reopen after restart");
    let aggregate = reopened.load(&id).await.expect("load");
    assert_eq!(aggregate.state.name(), "BookingInProgress");
    let intent = reopened
        .load_effect(&aggregate.active_effect.clone().expect("in flight"))
        .await
        .expect("the intent");
    assert!(
        !intent.status.is_terminal(),
        "nothing may be concluded from an answer we never received"
    );
}

/// The intent is durable **before** the call, observed from inside the call on a
/// separate connection. If Phase A had not committed, or a transaction were still
/// held open across the call, this would find nothing or block.
///
/// The claim is narrower than "no transaction is open": under WAL a second
/// *reader* proves no competing writer holds the lock, not that no read
/// transaction exists. It is an integration regression test, not a proof.
#[tokio::test]
async fn the_intent_is_durable_before_the_council_is_asked() {
    let temp = TempDir::new().expect("temp dir");
    let path = temp.path().join("townhall.sqlite");
    let repo = Arc::new(SqliteBookingRepository::open(&path).await.expect("open"));
    let council = Arc::new(FakeCouncil::new());

    let observed_path = path.clone();
    let seen: Arc<std::sync::Mutex<Option<(String, EffectStatus)>>> =
        Arc::new(std::sync::Mutex::new(None));
    let seen_in_hook = Arc::clone(&seen);

    let observer = Arc::new(ObservedCouncil::new(
        Arc::clone(&council),
        Arc::new(move |effect_id: &EffectIntentId| {
            let path = observed_path.clone();
            let effect_id = effect_id.clone();
            let slot = Arc::clone(&seen_in_hook);
            std::thread::spawn(move || {
                let rt = tokio::runtime::Builder::new_current_thread()
                    .enable_all()
                    .build()
                    .expect("runtime");
                rt.block_on(async move {
                    let other = SqliteBookingRepository::open(&path)
                        .await
                        .expect("a second connection must open");
                    let intent = other
                        .load_effect(&effect_id)
                        .await
                        .expect("the intent must already be durable");
                    *slot.lock().expect("lock") =
                        Some((intent.booking_id.to_string(), intent.status));
                });
            })
            .join()
            .expect("the observation must not panic");
        }),
    ));

    let coordinator = Coordinator::new(
        Arc::clone(&repo),
        observer,
        Arc::new(CouncilVerifier),
        Arc::new(FixedAvailability::new(facts())),
    );

    let id = BookingId::new("BKG-DURABLE");
    repo.create(NewBooking {
        id: id.clone(),
        requirements: requirements(),
    })
    .await
    .expect("create");
    for proposal in [select(), BookingProposal::VerifySlot] {
        coordinator
            .propose(&id, proposal, &authority())
            .await
            .expect("setup");
    }
    coordinator
        .propose(&id, BookingProposal::Book, &authority())
        .await
        .expect("book");

    let observation = seen.lock().expect("lock").clone();
    // `Unknown` mid-call, observed from a SECOND connection while the capability
    // is being invoked — which is gate M10's property, and it must be `Unknown`
    // rather than `Prepared` because Phase B records the attempt before the wire
    // (ADR-014 one level in). An implementation that wrote `Unknown` after the
    // timeout would read `Prepared` here and fail.
    assert_eq!(
        observation,
        Some((id.to_string(), EffectStatus::Unknown)),
        "the attempt must be durable, as Unknown, before the council is asked"
    );
}

// ---------------------------------------------------- ambiguity and refusal

/// A permanent refusal returns the booking to a re-proposable state, keeps *why*,
/// and a re-proposal mints a **fresh** identity — a tombstoned one can never
/// succeed.
#[tokio::test]
async fn a_permanent_refusal_returns_to_awaiting_with_a_fresh_identity() {
    let h = harness().await;
    let id = BookingId::new("BKG-REFUSED");
    awaiting(&h, &id, requirements()).await;
    h.council
        .script([Script::RefusePermanently("hall closed for maintenance")]);

    let outcome = h
        .coordinator
        .propose(&id, BookingProposal::Book, &authority())
        .await
        .expect("no service error");
    let BoundaryOutcome::Committed(aggregate) = outcome else {
        panic!("a refusal is an outcome and must commit, got {outcome:?}");
    };
    assert_eq!(aggregate.state.name(), "AwaitingBooking");
    assert_eq!(aggregate.booking_ref, None, "nothing was booked");

    let first = derive_effect_intent_id(&id, OperationKind::Book, AT_BOOK);
    let rejected = h.repo.load_effect(&first).await.expect("the intent");
    assert_eq!(rejected.status, EffectStatus::Rejected);
    assert_eq!(
        rejected
            .outcome_detail
            .as_ref()
            .map(bld_types::BoundedString::as_str),
        Some("hall closed for maintenance"),
        "why it was refused must survive"
    );

    // Try again. The council behaves this time.
    let outcome = h
        .coordinator
        .propose(&id, BookingProposal::Book, &authority())
        .await
        .expect("no service error");
    assert!(matches!(outcome, BoundaryOutcome::Committed(_)));
    let second = derive_effect_intent_id(&id, OperationKind::Book, aggregate.version);
    assert_ne!(first, second, "a re-proposal must mint a fresh identity");
    assert_eq!(h.council.booking_count(), 1);
}

/// The provenance test. A refusal that says nothing about whether the effect
/// happened must never become a durable rejection — that is ADR-016's much
/// stronger claim, and an ordinary rate limit is not entitled to it.
#[tokio::test]
async fn a_temporary_refusal_cannot_become_a_rejection() {
    let h = harness().await;
    let id = BookingId::new("BKG-TEMPORARY");
    awaiting(&h, &id, requirements()).await;
    h.council
        .script([Script::RefuseTemporarily("rate limited; try again")]);

    let outcome = h
        .coordinator
        .propose(&id, BookingProposal::Book, &authority())
        .await
        .expect("no service error");
    assert!(
        outcome.is_unresolved(),
        "a temporary refusal establishes nothing, got {outcome:?}"
    );

    let aggregate = h.repo.load(&id).await.expect("load");
    assert_eq!(
        aggregate.state.name(),
        "BookingInProgress",
        "the effect stays in flight for reconciliation"
    );
    let intent = h
        .repo
        .load_effect(&in_flight_effect(&h, &id).await)
        .await
        .expect("the intent");
    assert!(
        !intent.status.is_terminal(),
        "no conclusion may be drawn from a 'try again'"
    );
    let _ = aggregate;
}

/// An unattributable response is not evidence. A boundary that cannot read an
/// answer has not received one.
#[tokio::test]
async fn a_forged_response_concludes_nothing() {
    let h = harness().await;
    let id = BookingId::new("BKG-FORGED");
    awaiting(&h, &id, requirements()).await;
    h.council.script([Script::Forge]);

    let outcome = h
        .coordinator
        .propose(&id, BookingProposal::Book, &authority())
        .await
        .expect("no service error");
    assert!(outcome.is_unresolved());

    let aggregate = h.repo.load(&id).await.expect("load");
    assert_eq!(aggregate.state.name(), "BookingInProgress");
    assert_eq!(
        aggregate.booking_ref, None,
        "a forged confirmation must not become a booking reference"
    );
}

/// `Unknown` survives a restart with the *same* identity, and there is no way to
/// mint a second one by asking again — the proposal door has no `Book` at
/// `BookingInProgress`.
#[tokio::test]
async fn an_unknown_outcome_survives_restart_without_a_second_identity() {
    let h = harness().await;
    let id = BookingId::new("BKG-UNKNOWN");
    awaiting(&h, &id, requirements()).await;
    h.council.script([Script::GoQuiet("timed out")]);

    h.coordinator
        .propose(&id, BookingProposal::Book, &authority())
        .await
        .expect("no service error");
    let effect = in_flight_effect(&h, &id).await;

    let reopened = SqliteBookingRepository::open(&h.path)
        .await
        .expect("reopen");
    let after = reopened.load(&id).await.expect("load after restart");
    assert_eq!(after.active_effect, Some(effect));
    assert_eq!(after.state.name(), "BookingInProgress");

    let outcome = h
        .coordinator
        .propose(&id, BookingProposal::Book, &authority())
        .await
        .expect("no service error");
    assert!(
        matches!(outcome, BoundaryOutcome::Undefined),
        "an in-flight booking has no book behaviour, got {outcome:?}"
    );
    assert_eq!(
        h.council.call_count(),
        1,
        "and the council was not asked twice"
    );
}

// ----------------------------------------------- idempotency and convergence

/// One identity, one booking, however many times the council is asked. The
/// property slice D's real council must also have, proven here first.
#[tokio::test]
async fn one_identity_yields_one_booking() {
    let h = harness().await;
    let id = BookingId::new("BKG-IDEMPOTENT");
    // Both calls present the same attempt — same identity *and* same deadline.
    // A retry that re-derived its deadline would present a different one, which
    // is what the envelope exists to make impossible on the coordinator path.
    let attempt = EffectAttempt {
        id: derive_effect_intent_id(&id, OperationKind::Book, AT_BOOK),
        expires_at_ms: 1_000_030_000,
    };

    let first = h
        .council
        .execute(&book_plan(), &attempt)
        .await
        .expect("first call");
    let again = h
        .council
        .execute(&book_plan(), &attempt)
        .await
        .expect("second call");

    assert_eq!(first.body, again.body, "a retry returns the original");
    assert_eq!(h.council.booking_count(), 1, "and creates nothing new");
    assert_eq!(h.council.call_count(), 2, "though it was asked twice");
}

/// Re-applying a fact the state already reflects is `Converged` — success,
/// because a reconciler re-applies facts by design.
#[tokio::test]
async fn re_observing_a_settled_booking_converges() {
    let h = harness().await;
    let id = BookingId::new("BKG-CONVERGE");
    awaiting(&h, &id, requirements()).await;
    let BoundaryOutcome::Committed(booked) = h
        .coordinator
        .propose(&id, BookingProposal::Book, &authority())
        .await
        .expect("book")
    else {
        panic!("expected a commit");
    };
    let reference = booked.booking_ref.clone().expect("a reference");

    let replayed = h
        .coordinator
        .observe(
            &id,
            Verified::assert_verified(VerifiedProviderFact::BookingExists {
                effect_intent_id: derive_effect_intent_id(&id, OperationKind::Book, AT_BOOK),
                booking_ref: reference,
                venue_id: VenueId::new("TH-A"),
                slot_id: SlotId::new("SLOT-A"),
                attendees: 20,
                fee: Money::from_pence(4_500),
                principal: PrincipalId::new("lucy"),
            }),
        )
        .await
        .expect("no service error");

    assert!(
        matches!(replayed, BoundaryOutcome::Converged),
        "already-applied is success, got {replayed:?}"
    );
    assert_eq!(
        h.repo.load(&id).await.expect("load").version,
        booked.version,
        "convergence must write nothing"
    );
}

// -------------------------------------------------------------- giving up

/// Giving up is a pursuit decision (ADR-019): the booking does not move, the
/// pointer stays, and the intent's status stays `Unknown` — because the council
/// may well hold the room and any other claim would assert what nobody
/// established. Then the part the old design made impossible: the council
/// finally answers, and the booking is adopted through the ordinary fact arms.
#[tokio::test]
async fn exhaustion_marks_the_intent_and_a_late_fact_still_lands() {
    let h = harness().await;
    let id = BookingId::new("BKG-EXHAUSTED");
    awaiting(&h, &id, requirements()).await;

    // The initial call is the dropped-response scenario: the council CREATES the
    // booking and the answer never arrives. One attempt spent, outcome unknown —
    // and unknown is now recorded as such before the wire, not left as Prepared.
    h.council
        .script([Script::SucceedThenGoQuiet("response eaten")]);
    h.coordinator
        .propose(&id, BookingProposal::Book, &authority())
        .await
        .expect("book");
    let effect = in_flight_effect(&h, &id).await;
    let before = h.repo.load(&id).await.expect("load");
    let audit_before = h.repo.audit_events(&id).await.expect("audit").len();

    // Spend the rest of the budget through real reconciliation turns — scripted
    // no-answers, the store clock advanced past the cadence between turns, never
    // a sleep. The escalating turn consumes no script: it asks the domain, not
    // the council, which is the point.
    h.council.script([
        Script::GoQuiet("still nothing"),
        Script::GoQuiet("still nothing"),
        Script::GoQuiet("still nothing"),
        Script::GoQuiet("still nothing"),
    ]);
    for turn in 0..5 {
        h.clock.advance(10_000);
        let attended = h.reconciliation.attend(&effect).await.expect("attend");
        if turn < 4 {
            assert!(
                matches!(attended, Attended::StillUnknown { .. }),
                "turn {turn}: expected StillUnknown, got {attended:?}"
            );
        } else {
            assert_eq!(attended, Attended::Escalated, "the budget is spent");
        }
    }

    // Escalated: flagged for a human, and NOTHING else changed.
    let after = h.repo.load(&id).await.expect("load");
    assert_eq!(
        after.state.name(),
        "BookingInProgress",
        "the state is the story"
    );
    assert_eq!(
        after.active_effect,
        Some(effect.clone()),
        "the pointer survives — a late fact must still bind"
    );
    assert_eq!(
        after.version, before.version,
        "escalation touches only the intent row, never the aggregate"
    );
    let intent = h.repo.load_effect(&effect).await.expect("intent");
    assert_eq!(
        intent.status,
        EffectStatus::Unknown,
        "the outcome IS unknown, and the status says so"
    );
    assert_eq!(
        h.repo.escalated_unresolved(10).await.expect("queue"),
        vec![effect.clone()],
        "the human queue holds exactly this question"
    );
    assert_eq!(
        h.repo.audit_events(&id).await.expect("audit").len(),
        audit_before,
        "escalation is a marker on the intent, never an audit event — the \
         audit trail records the aggregate's story, and the aggregate did not move"
    );

    // Straight back for another turn: the escalated cadence holds. A second
    // escalation cannot happen through this door (the once-only write is the
    // store's own gate), and neither can an early re-ask.
    assert_eq!(
        h.reconciliation.attend(&effect).await.expect("attend"),
        Attended::NotDue,
        "escalated means hourly, and the hour is not up"
    );

    // The council finally answers — the ADR-019 payoff. The booking it created
    // under this identity all along is adopted, through the unmodified arms.
    h.clock.advance(townhall_store::MAX_CADENCE_MS + 1);
    h.council.script([Script::Succeed]);
    // Make the fake actually hold a booking for this identity: the original
    // create landed even though its response was eaten.
    assert_eq!(
        h.council.booking_count(),
        1,
        "the council held it all along"
    );
    let settled = h.reconciliation.attend(&effect).await.expect("attend");
    assert_eq!(settled, Attended::Settled);

    let adopted = h.repo.load(&id).await.expect("load");
    assert_eq!(adopted.state.name(), "Booked");
    assert!(
        h.repo
            .escalated_unresolved(10)
            .await
            .expect("queue")
            .is_empty(),
        "the question was answered, so it leaves the queue"
    );
}

// ---------------------------------------------------------------- refusals

/// A denial costs nothing and touches nothing — no version bump, and nothing
/// external reached.
#[tokio::test]
async fn a_denied_proposal_touches_nothing() {
    let h = harness().await;
    let id = BookingId::new("BKG-DENIED");
    awaiting(&h, &id, requirements()).await;
    let before = h.repo.load(&id).await.expect("load");

    let broke = VerifiedAuthority {
        max_fee: Money::from_pence(4_000),
        ..authority()
    };
    let outcome = h
        .coordinator
        .propose(&id, BookingProposal::Book, &broke)
        .await
        .expect("no service error");

    assert!(
        matches!(outcome, BoundaryOutcome::Denied(_)),
        "a £45 room over a £40 ceiling must be refused, got {outcome:?}"
    );
    assert_eq!(
        h.repo.load(&id).await.expect("load").version,
        before.version
    );
    assert_eq!(
        h.council.call_count(),
        0,
        "and nothing external was reached"
    );
}

/// The two kinds of no stay distinct all the way out to a caller. A lift with no
/// button for floor 13, versus a button that wants a keycard.
#[tokio::test]
async fn undefined_and_denied_reach_the_caller_as_different_answers() {
    let h = harness().await;

    // `Draft` has no book at all — nothing was consulted.
    let absent = BookingId::new("BKG-UNDEFINED");
    h.repo
        .create(NewBooking {
            id: absent.clone(),
            requirements: requirements(),
        })
        .await
        .expect("create");
    let undefined = h
        .coordinator
        .propose(&absent, BookingProposal::Book, &authority())
        .await
        .expect("no service error");
    assert!(
        matches!(undefined, BoundaryOutcome::Undefined),
        "Draft has no book behaviour, got {undefined:?}"
    );

    // Whereas `VerifySlot` exists at `VenueSelected` and is refused: a 30-seat
    // room cannot hold 999 people.
    let crowded = BookingId::new("BKG-DENIED-VERIFY");
    h.repo
        .create(NewBooking {
            id: crowded.clone(),
            requirements: BookingRequirements {
                attendees: 999,
                ..requirements()
            },
        })
        .await
        .expect("create");
    h.coordinator
        .propose(&crowded, select(), &authority())
        .await
        .expect("select");
    let denied = h
        .coordinator
        .propose(&crowded, BookingProposal::VerifySlot, &authority())
        .await
        .expect("no service error");
    assert!(
        matches!(denied, BoundaryOutcome::Denied(_)),
        "999 people into 30 seats exists as a behaviour and is refused, got {denied:?}"
    );
}

/// Contention is reported, never fabricated into success.
#[tokio::test]
async fn exhausting_the_attempt_budget_is_reported() {
    let h = harness().await;
    let id = BookingId::new("BKG-CONTENDED");
    awaiting(&h, &id, requirements()).await;
    h.council.script([Script::GoQuiet("timed out")]);
    h.coordinator
        .propose(&id, BookingProposal::Book, &authority())
        .await
        .expect("book");

    // A coordinator with no attempts at all: the first commit it needs is one it
    // never gets to make.
    let starved = Coordinator::new(
        Arc::clone(&h.repo),
        Arc::clone(&h.council),
        Arc::new(CouncilVerifier),
        Arc::new(FixedAvailability::new(facts())),
    )
    .with_attempts(0);

    let error = starved
        .observe(
            &id,
            Verified::assert_verified(VerifiedProviderFact::EffectAbsent {
                effect_intent_id: derive_effect_intent_id(&id, OperationKind::Book, AT_BOOK),
            }),
        )
        .await
        .expect_err("a starved budget must report, not invent");
    assert!(
        matches!(error, ServiceError::Contended { attempts: 0 }),
        "expected a contention report, got {error:?}"
    );
    assert_eq!(
        h.repo.load(&id).await.expect("load").state.name(),
        "BookingInProgress",
        "and nothing was committed on the way out"
    );
}

// ------------------------------------------------------------ concurrency

/// Two coordinators propose the same booking at the same version. Exactly one
/// asks the council.
///
/// `prepare_effect` serialises them under `BEGIN IMMEDIATE`, so the loser finds
/// the intent already committed and is told `replayed`. It must then **stop**: the
/// winner may already have executed, and re-running Phase B would ask the council
/// twice for one identity.
///
/// The fake council is idempotent, so a second call would produce no second
/// booking — which is exactly why the assertion is on the **call count**, not the
/// booking count. Relying on provider idempotency to cover our own double-send is
/// what ADR-014 exists to refuse; a provider that lacked it would double-book.
#[tokio::test]
async fn two_coordinators_racing_one_booking_ask_the_council_once() {
    let h = harness().await;
    let id = BookingId::new("BKG-RACE");
    awaiting(&h, &id, requirements()).await;

    let second = Coordinator::new(
        Arc::clone(&h.repo),
        Arc::clone(&h.council),
        Arc::new(CouncilVerifier),
        Arc::new(FixedAvailability::new(facts())),
    );

    let gate = Arc::new(tokio::sync::Barrier::new(2));
    let (left_gate, right_gate) = (Arc::clone(&gate), Arc::clone(&gate));
    let left_id = id.clone();
    let right_id = id.clone();

    let left = async move {
        left_gate.wait().await;
        h.coordinator
            .propose(&left_id, BookingProposal::Book, &authority())
            .await
    };
    let right = async move {
        right_gate.wait().await;
        second
            .propose(&right_id, BookingProposal::Book, &authority())
            .await
    };

    let (a, b) = tokio::join!(left, right);
    let a = a.expect("no service error");
    let b = b.expect("no service error");

    // Exactly one carried the booking through; the other stopped short.
    let committed = [&a, &b]
        .iter()
        .filter(|outcome| matches!(outcome, BoundaryOutcome::Committed(_)))
        .count();
    assert_eq!(
        committed, 1,
        "exactly one turn may commit the booking, got {a:?} and {b:?}"
    );

    assert_eq!(
        h.council.call_count(),
        1,
        "the council must be asked once per identity by US, not deduped by the provider"
    );
    assert_eq!(h.council.booking_count(), 1);

    let final_state = h.repo.load(&id).await.expect("load");
    assert_eq!(final_state.state.name(), "Booked");
    assert_eq!(final_state.active_effect, None);
}

/// A fact whose commit loses the compare-and-set is re-classified against the new
/// state, not discarded — and produces no second effect and no second call.
///
/// This is ADR-012's rule made executable: evidence identity is stable across
/// races, and transition meaning is derived from the evidence plus whatever state
/// now holds. The council really did book the room; losing a CAS does not make
/// that untrue.
#[tokio::test]
async fn a_fact_that_loses_a_race_is_re_classified_not_dropped() {
    let h = harness().await;
    let id = BookingId::new("BKG-LOSTRACE");
    awaiting(&h, &id, requirements()).await;
    // Leave the booking in flight with nothing concluded.
    h.council.script([Script::GoQuiet("no answer")]);
    h.coordinator
        .propose(&id, BookingProposal::Book, &authority())
        .await
        .expect("book");
    let effect = in_flight_effect(&h, &id).await;
    let calls_before = h.council.call_count();

    // Two observers carrying the *same* verified confirmation, concurrently.
    let confirmation = || {
        Verified::assert_verified(VerifiedProviderFact::BookingExists {
            effect_intent_id: effect.clone(),
            booking_ref: bld_types::CouncilBookingRef::new("TH-90001"),
            venue_id: VenueId::new("TH-A"),
            slot_id: SlotId::new("SLOT-A"),
            attendees: 20,
            fee: Money::from_pence(4_500),
            principal: PrincipalId::new("lucy"),
        })
    };
    let second = Coordinator::new(
        Arc::clone(&h.repo),
        Arc::clone(&h.council),
        Arc::new(CouncilVerifier),
        Arc::new(FixedAvailability::new(facts())),
    );

    let gate = Arc::new(tokio::sync::Barrier::new(2));
    let (left_gate, right_gate) = (Arc::clone(&gate), Arc::clone(&gate));
    let left_id = id.clone();
    let right_id = id.clone();
    let left_fact = confirmation();
    let right_fact = confirmation();

    let (a, b) = tokio::join!(
        async move {
            left_gate.wait().await;
            h.coordinator.observe(&left_id, left_fact).await
        },
        async move {
            right_gate.wait().await;
            second.observe(&right_id, right_fact).await
        }
    );
    let a = a.expect("no service error");
    let b = b.expect("no service error");

    // One committed. The other, re-classifying against the state the winner
    // produced, converged — its evidence was already applied.
    let outcomes = [&a, &b];
    assert_eq!(
        outcomes
            .iter()
            .filter(|o| matches!(o, BoundaryOutcome::Committed(_)))
            .count(),
        1,
        "exactly one may commit, got {a:?} and {b:?}"
    );
    assert_eq!(
        outcomes
            .iter()
            .filter(|o| matches!(o, BoundaryOutcome::Converged))
            .count(),
        1,
        "the loser must converge, not fail, got {a:?} and {b:?}"
    );

    let after = h.repo.load(&id).await.expect("load");
    assert_eq!(after.state.name(), "Booked");
    assert_eq!(
        h.council.call_count(),
        calls_before,
        "Phase C must never re-enter Phase B"
    );
    let intent = h.repo.load_effect(&effect).await.expect("the intent");
    assert_eq!(intent.status, EffectStatus::Confirmed);
    assert_eq!(intent.supersedes, None, "no second effect was minted");
}

// -------------------------------------------------------- the denial logbook

/// A harness with the logbook wired.
async fn harness_with_denials() -> (Harness, Arc<townhall_store::denials::DenialLog>) {
    let mut h = harness().await;
    let log = Arc::new(
        townhall_store::denials::DenialLog::open(
            h.temp.path().join("denials.sqlite"),
            Arc::clone(&h.clock) as Arc<dyn townhall_store::StoreClock>,
        )
        .await
        .expect("open the denial log"),
    );
    h.coordinator = Coordinator::new(
        Arc::clone(&h.repo),
        Arc::clone(&h.council),
        Arc::new(CouncilVerifier),
        Arc::new(FixedAvailability::new(facts())),
    )
    .with_denial_log(Arc::clone(&log));
    // The reconciler's door refuses too (the system-event door), so its own
    // coordinator carries the same logbook.
    h.reconciliation = Reconciliation::new(
        Arc::new(
            Coordinator::new(
                Arc::clone(&h.repo),
                Arc::clone(&h.council),
                Arc::new(CouncilVerifier),
                Arc::new(FixedAvailability::new(facts())),
            )
            .with_denial_log(Arc::clone(&log)),
        ),
        Arc::clone(&h.council),
    );
    (h, log)
}

/// A refusal is provable from the database afterwards: who, what, why, when —
/// and the reason is the stable name, never display text with numbers in it.
#[tokio::test]
async fn a_refusal_leaves_a_durable_row() {
    let (h, log) = harness_with_denials().await;
    let id = BookingId::new("BKG-DENY");
    awaiting(&h, &id, requirements()).await;

    // Lucy tries to book with no booking authority: the door exists, the guard
    // says no.
    let mut no_authority = authority();
    no_authority.may_book = false;
    let outcome = h
        .coordinator
        .propose(&id, BookingProposal::Book, &no_authority)
        .await
        .expect("turn");
    assert!(matches!(outcome, BoundaryOutcome::Denied(_)));

    let rows = log.rows().await.expect("rows");
    assert_eq!(rows.len(), 1);
    assert_eq!(rows[0].booking_id, id.to_string());
    assert_eq!(rows[0].driver_kind, "Proposal");
    assert_eq!(rows[0].driver_detail, "Book");
    assert_eq!(rows[0].reason, "BookingAuthorityRequired");
    assert_eq!(rows[0].principal, "lucy");
    assert_eq!(rows[0].occurrences, 1);
}

/// Identical refusals compress; different people do not. Two principals refused
/// identically are TWO rows — collapsing them would attribute one person's
/// refusals to the other, which is a false record, not a compression.
#[tokio::test]
async fn identical_refusals_compress_but_principals_never_merge() {
    let (h, log) = harness_with_denials().await;
    let id = BookingId::new("BKG-FLOOD");
    awaiting(&h, &id, requirements()).await;

    let mut lucy = authority();
    lucy.may_book = false;
    let mut marco = authority();
    marco.principal = PrincipalId::new("marco");
    marco.may_book = false;

    // Lucy is refused 40 times; Marco once. Every answer is the same typed
    // refusal — the 40th identical denial costs the caller nothing different.
    for _ in 0..40 {
        let outcome = h
            .coordinator
            .propose(&id, BookingProposal::Book, &lucy)
            .await
            .expect("turn");
        assert!(matches!(
            outcome,
            BoundaryOutcome::Denied(BookingError::BookingAuthorityRequired)
        ));
    }
    h.coordinator
        .propose(&id, BookingProposal::Book, &marco)
        .await
        .expect("turn");

    let rows = log.rows().await.expect("rows");
    assert_eq!(rows.len(), 2, "one row per principal, not one total");
    let lucy_row = rows.iter().find(|r| r.principal == "lucy").expect("lucy");
    let marco_row = rows.iter().find(|r| r.principal == "marco").expect("marco");
    assert_eq!(
        lucy_row.occurrences, 40,
        "the flood is a counter, not 40 rows"
    );
    assert_eq!(marco_row.occurrences, 1);
}

/// The same refusal in a different hour is a different row: "4,000 times
/// between 02:00 and 03:00, and twice in August" must stay answerable.
#[tokio::test]
async fn the_same_refusal_next_hour_is_a_new_row() {
    let (h, log) = harness_with_denials().await;
    let id = BookingId::new("BKG-WINDOWS");
    awaiting(&h, &id, requirements()).await;
    let mut lucy = authority();
    lucy.may_book = false;

    h.coordinator
        .propose(&id, BookingProposal::Book, &lucy)
        .await
        .expect("turn");
    h.clock.advance(60 * 60 * 1000 + 1);
    h.coordinator
        .propose(&id, BookingProposal::Book, &lucy)
        .await
        .expect("turn");

    let rows = log.rows().await.expect("rows");
    assert_eq!(rows.len(), 2, "one row per hour");
    assert_ne!(rows[0].window_start_ms, rows[1].window_start_ms);
}

/// Asking for a button that does not exist is counted, never rowed — it is
/// forgeable from pure garbage, and a durable row per garbage request is a
/// disk-filling attack (ADR-017).
#[tokio::test]
async fn asking_for_a_nonexistent_behaviour_is_counted_not_rowed() {
    let (h, log) = harness_with_denials().await;
    let id = BookingId::new("BKG-UNDEFINED");
    h.repo
        .create(NewBooking {
            id: id.clone(),
            requirements: requirements(),
        })
        .await
        .expect("create");

    // Book from Draft: no such edge. Three times.
    for _ in 0..3 {
        let outcome = h
            .coordinator
            .propose(&id, BookingProposal::Book, &authority())
            .await
            .expect("turn");
        assert_eq!(outcome, BoundaryOutcome::Undefined);
    }

    assert_eq!(log.undefined_count("Draft", "Book"), 3);
    assert!(
        log.rows().await.expect("rows").is_empty(),
        "no durable row for a behaviour that does not exist"
    );
}

/// The refusal at the FACT door records too — this is where the refusals that
/// matter most live, and the design review found the original ADR wiring only
/// the proposal door. An unattributable one is recorded as exactly that.
#[tokio::test]
async fn a_fact_door_refusal_records_with_a_derived_principal() {
    let (h, log) = harness_with_denials().await;
    let id = BookingId::new("BKG-FACTDENY");
    awaiting(&h, &id, requirements()).await;
    h.council.script([Script::SucceedThenGoQuiet("eaten")]);
    h.coordinator
        .propose(&id, BookingProposal::Book, &authority())
        .await
        .expect("book");
    let effect = in_flight_effect(&h, &id).await;

    // A signed cancellation fact arrives for a BOOKING intent: wrong kind, and
    // the domain refuses it. Nobody proposed anything — the principal comes
    // from the persisted plan.
    let wrong_kind = Verified::assert_verified(VerifiedProviderFact::CancellationExists {
        effect_intent_id: effect.clone(),
        booking_ref: CouncilBookingRef::new("TH-99999"),
    });
    let outcome = h.coordinator.observe(&id, wrong_kind).await.expect("turn");
    assert!(matches!(
        outcome,
        BoundaryOutcome::Denied(BookingError::EffectKindMismatch)
    ));

    let rows = log.rows().await.expect("rows");
    assert_eq!(rows.len(), 1);
    assert_eq!(rows[0].driver_kind, "Fact");
    assert_eq!(rows[0].driver_detail, "CancellationExists");
    assert_eq!(rows[0].reason, "EffectKindMismatch");
    assert_eq!(
        rows[0].principal, "lucy",
        "derived from the persisted plan, since no one proposed anything"
    );
}

/// One identity, two provider references — duplication, corruption or broken
/// idempotency, and never silent convergence (gate M6's named refusal). The
/// booking is genuinely `Booked` when the second reference arrives, so an
/// implementation that shrugs "already booked, close enough" fails here.
#[tokio::test]
async fn a_second_provider_reference_for_one_identity_is_refused_and_rowed() {
    let (h, log) = harness_with_denials().await;
    let id = BookingId::new("BKG-TWOREFS");
    awaiting(&h, &id, requirements()).await;
    h.coordinator
        .propose(&id, BookingProposal::Book, &authority())
        .await
        .expect("book");
    let booked = h.repo.load(&id).await.expect("load");
    assert_eq!(booked.state.name(), "Booked");
    let effect = derive_effect_intent_id(&id, OperationKind::Book, AT_BOOK);

    // A signed fact arrives claiming the SAME intent produced a DIFFERENT
    // booking. Field-perfect otherwise — the reference is the lie.
    let second_reference = Verified::assert_verified(VerifiedProviderFact::BookingExists {
        effect_intent_id: effect,
        booking_ref: CouncilBookingRef::new("TH-00001"),
        venue_id: VenueId::new("TH-A"),
        slot_id: SlotId::new("SLOT-A"),
        attendees: 20,
        fee: Money::from_pence(4_500),
        principal: PrincipalId::new("lucy"),
    });
    let outcome = h
        .coordinator
        .observe(&id, second_reference)
        .await
        .expect("turn");
    assert!(
        matches!(
            outcome,
            BoundaryOutcome::Denied(BookingError::DuplicateProviderEffect)
        ),
        "one identity cannot have booked two rooms: {outcome:?}"
    );

    let rows = log.rows().await.expect("rows");
    assert_eq!(rows.len(), 1);
    assert_eq!(rows[0].driver_kind, "Fact");
    assert_eq!(rows[0].driver_detail, "BookingExists");
    assert_eq!(rows[0].reason, "DuplicateProviderEffect");
    assert_eq!(rows[0].principal, "lucy");
}

/// The THIRD door records its refusals too. An exhausted chase for an effect
/// the booking is not waiting on is `Denied(EffectMismatch)` by the domain —
/// and that refusal lands in the logbook like any other, attributed from the
/// stale intent's own persisted plan.
#[tokio::test]
async fn an_exhausted_chase_for_the_wrong_effect_is_denied_and_rowed() {
    let (h, log) = harness_with_denials().await;
    let id = BookingId::new("BKG-STALECHASE");
    awaiting(&h, &id, requirements()).await;

    // Lucy's booking is honestly in flight on its OWN effect...
    h.council.script([Script::SucceedThenGoQuiet("eaten")]);
    h.coordinator
        .propose(&id, BookingProposal::Book, &authority())
        .await
        .expect("book");
    let live = in_flight_effect(&h, &id).await;

    // ...and a STALE intent for the same booking sits in the store with its
    // budget long spent — the leftover a crashed, older deployment could leave.
    // Planted raw because no current door can mint it, which is the point: the
    // reconciler will meet rows history wrote, not only rows this build writes.
    let plan_json: String = sqlx::query_scalar(
        "SELECT canonical_plan_json FROM effect_intents WHERE effect_intent_id = ?",
    )
    .bind(live.as_str())
    .fetch_one(h.repo.pool())
    .await
    .expect("the live plan");
    sqlx::query(
        r"
        INSERT INTO effect_intents (effect_intent_id, booking_id, operation_kind,
                                    source_version, canonical_plan_json, status,
                                    expires_at_ms, created_at_ms, updated_at_ms,
                                    attempts_started, attempts_finished)
        VALUES (?, ?, 'Book', 99, ?, 'Unknown', ?, 0, 0, 1000, 1000)
        ",
    )
    .bind("EFF-STALE-CHASE")
    .bind(id.to_string())
    .bind(&plan_json)
    .bind(i64::MAX / 2)
    .execute(h.repo.pool())
    .await
    .expect("plant the stale intent");

    let stale = EffectIntentId::new("EFF-STALE-CHASE");
    let attended = h.reconciliation.attend(&stale).await.expect("attend");
    assert_eq!(
        attended,
        Attended::NotDue,
        "nothing to escalate: the booking is not waiting on this effect"
    );
    assert!(
        h.repo
            .escalated_unresolved(10)
            .await
            .expect("queue")
            .is_empty(),
        "no marker was written — the domain said no, so the store wrote nothing"
    );
    let untouched = h.repo.load(&id).await.expect("load");
    assert_eq!(untouched.state.name(), "BookingInProgress");
    assert_eq!(untouched.active_effect, Some(live));

    let rows = log.rows().await.expect("rows");
    assert_eq!(rows.len(), 1);
    assert_eq!(rows[0].driver_kind, "SystemEvent");
    assert_eq!(rows[0].driver_detail, "ReconciliationExhausted");
    assert_eq!(rows[0].reason, "EffectMismatch");
    assert_eq!(
        rows[0].principal, "lucy",
        "attributed from the stale intent's own persisted plan"
    );
}

// ------------------------------------------------------------- lease visibility

/// Gate M15: Phase B PARTICIPATES in leasing — observed, not narrated. While
/// the coordinator's call is on the wire, a second connection must see the
/// lease held, and a reconciler asked to attend the same intent must answer
/// `NotDue` rather than racing the call. When the turn ends, the lease is gone.
#[tokio::test]
async fn phase_b_holds_the_lease_and_a_mid_call_reconciler_defers() {
    let temp = TempDir::new().expect("temp dir");
    let path = temp.path().join("townhall.sqlite");
    let repo = Arc::new(SqliteBookingRepository::open(&path).await.expect("open"));
    let council = Arc::new(FakeCouncil::new());

    let observed_path = path.clone();
    let seen: Arc<std::sync::Mutex<Option<(bool, Attended)>>> =
        Arc::new(std::sync::Mutex::new(None));
    let seen_in_hook = Arc::clone(&seen);

    let observer = Arc::new(ObservedCouncil::new(
        Arc::clone(&council),
        Arc::new(move |effect_id: &EffectIntentId| {
            let path = observed_path.clone();
            let effect_id = effect_id.clone();
            let slot = Arc::clone(&seen_in_hook);
            std::thread::spawn(move || {
                let rt = tokio::runtime::Builder::new_current_thread()
                    .enable_all()
                    .build()
                    .expect("runtime");
                rt.block_on(async move {
                    let other = Arc::new(
                        SqliteBookingRepository::open(&path)
                            .await
                            .expect("a second connection must open"),
                    );
                    // The lease, read from the row itself — the row is the
                    // only witness the reconciler gets.
                    let held: Option<i64> = sqlx::query_scalar(
                        "SELECT lease_until_ms FROM effect_intents \
                         WHERE effect_intent_id = ?",
                    )
                    .bind(effect_id.as_str())
                    .fetch_one(other.pool())
                    .await
                    .expect("the intent row");
                    // And the reconciler's own answer while the call is live.
                    let bystander = Arc::new(FakeCouncil::new());
                    let reconciliation = Reconciliation::new(
                        Arc::new(Coordinator::new(
                            Arc::clone(&other),
                            Arc::clone(&bystander),
                            Arc::new(CouncilVerifier),
                            Arc::new(FixedAvailability::new(facts())),
                        )),
                        Arc::clone(&bystander),
                    );
                    let attended = reconciliation
                        .attend(&effect_id)
                        .await
                        .expect("attend must not error");
                    *slot.lock().expect("lock") = Some((held.is_some(), attended));
                });
            })
            .join()
            .expect("the observation must not panic");
        }),
    ));

    let coordinator = Coordinator::new(
        Arc::clone(&repo),
        observer,
        Arc::new(CouncilVerifier),
        Arc::new(FixedAvailability::new(facts())),
    );

    let id = BookingId::new("BKG-LEASED");
    repo.create(NewBooking {
        id: id.clone(),
        requirements: requirements(),
    })
    .await
    .expect("create");
    for proposal in [select(), BookingProposal::VerifySlot] {
        coordinator
            .propose(&id, proposal, &authority())
            .await
            .expect("setup");
    }
    // The call itself goes quiet, so the turn ends unresolved — the lease's
    // release must not depend on an answer arriving.
    council.script([Script::GoQuiet("no answer")]);
    coordinator
        .propose(&id, BookingProposal::Book, &authority())
        .await
        .expect("book");

    let observation = seen.lock().expect("lock").clone();
    assert_eq!(
        observation,
        Some((true, Attended::NotDue)),
        "mid-call: the lease is held on the row, and the reconciler defers to it"
    );

    // The turn is over: the lease is given back, answer or no answer, so the
    // reconciler owns the chase at its ordinary cadence rather than waiting
    // out a dead owner.
    let effect = derive_effect_intent_id(&id, OperationKind::Book, AT_BOOK);
    let released: Option<i64> =
        sqlx::query_scalar("SELECT lease_until_ms FROM effect_intents WHERE effect_intent_id = ?")
            .bind(effect.as_str())
            .fetch_one(repo.pool())
            .await
            .expect("the intent row");
    assert_eq!(released, None, "the lease does not outlive the turn");
}

// ------------------------------------ slice F: in-flight cancellation (ADR-020)

/// Test 9's shape: Lucy says "stop" while her booking's outcome is unknown.
/// The move is LOCAL — the state records who asked and nothing touches the
/// wire, because you cannot cancel what may not exist. An implementation that
/// refuses Cancel mid-flight fails the propose; one that sends anything fails
/// the wire log.
#[tokio::test]
async fn a_cancel_mid_flight_commits_locally_and_touches_no_wire() {
    let h = harness().await;
    let id = BookingId::new("BKG-MIDFLIGHT");
    awaiting(&h, &id, requirements()).await;
    h.council.script([Script::SucceedThenGoQuiet("eaten")]);
    h.coordinator
        .propose(&id, BookingProposal::Book, &authority())
        .await
        .expect("book");
    let effect = in_flight_effect(&h, &id).await;
    let wire_before = h.council.wire_log().len();
    let before = h.repo.load(&id).await.expect("load");

    let outcome = h
        .coordinator
        .propose(
            &id,
            BookingProposal::Cancel {
                reason: "changed my mind".to_owned(),
            },
            &authority(),
        )
        .await
        .expect("the turn runs");
    let BoundaryOutcome::Committed(aggregate) = outcome else {
        panic!("Cancel mid-flight must commit locally, got {outcome:?}");
    };

    assert_eq!(aggregate.state.name(), "CancellationRequested");
    let townhall_domain::BookingState::CancellationRequested(requested) = &aggregate.state else {
        panic!("wrong state shape");
    };
    assert_eq!(
        requested.effect_intent_id, effect,
        "still waiting on the SAME booking intent"
    );
    assert_eq!(
        requested.cancelled_by,
        PrincipalId::new("lucy"),
        "the state remembers who asked — the handoff will need it"
    );
    assert_eq!(aggregate.active_effect, Some(effect));
    assert_eq!(aggregate.version, before.version + 1);
    assert_eq!(
        h.council.wire_log().len(),
        wire_before,
        "nothing was sent and nothing was asked: the move is local"
    );
}

/// The guard on the new edge, and its logbook row: cancelling mid-flight
/// without cancellation authority is refused and recorded.
#[tokio::test]
async fn a_mid_flight_cancel_without_authority_is_denied_and_rowed() {
    let (h, log) = harness_with_denials().await;
    let id = BookingId::new("BKG-MIDDENY");
    awaiting(&h, &id, requirements()).await;
    h.council.script([Script::SucceedThenGoQuiet("eaten")]);
    h.coordinator
        .propose(&id, BookingProposal::Book, &authority())
        .await
        .expect("book");

    let mut no_authority = authority();
    no_authority.may_cancel = false;
    let outcome = h
        .coordinator
        .propose(
            &id,
            BookingProposal::Cancel {
                reason: "not allowed to".to_owned(),
            },
            &no_authority,
        )
        .await
        .expect("turn");
    assert!(matches!(
        outcome,
        BoundaryOutcome::Denied(BookingError::CancellationAuthorityRequired)
    ));

    let rows = log.rows().await.expect("rows");
    assert_eq!(rows.len(), 1);
    assert_eq!(rows[0].driver_detail, "Cancel");
    assert_eq!(rows[0].reason, "CancellationAuthorityRequired");
    assert_eq!(rows[0].principal, "lucy");
}

/// ADR-019's inheritance, as a test: escalation changes no menu, so Cancel is
/// proposable on an ESCALATED booking — and the escalation marker survives the
/// move untouched.
#[tokio::test]
async fn cancel_is_proposable_on_an_escalated_booking() {
    let h = harness().await;
    let id = BookingId::new("BKG-ESCCANCEL");
    awaiting(&h, &id, requirements()).await;
    h.council
        .script([Script::SucceedThenGoQuiet("response eaten")]);
    h.coordinator
        .propose(&id, BookingProposal::Book, &authority())
        .await
        .expect("book");
    let effect = in_flight_effect(&h, &id).await;

    h.council.script([
        Script::GoQuiet("still nothing"),
        Script::GoQuiet("still nothing"),
        Script::GoQuiet("still nothing"),
        Script::GoQuiet("still nothing"),
    ]);
    for turn in 0..5 {
        h.clock.advance(10_000);
        let attended = h.reconciliation.attend(&effect).await.expect("attend");
        if turn == 4 {
            assert_eq!(attended, Attended::Escalated);
        }
    }

    let outcome = h
        .coordinator
        .propose(
            &id,
            BookingProposal::Cancel {
                reason: "took too long".to_owned(),
            },
            &authority(),
        )
        .await
        .expect("turn");
    let BoundaryOutcome::Committed(aggregate) = outcome else {
        panic!("Cancel must exist on an escalated booking, got {outcome:?}");
    };
    assert_eq!(aggregate.state.name(), "CancellationRequested");
    assert_eq!(
        h.repo.escalated_unresolved(10).await.expect("queue"),
        vec![effect],
        "the question is still open — the marker is the intent's, not the state's"
    );
}

/// Test 14: pre-deadline "not found" is `Unknown`, never absence — and under
/// `CancellationRequested` the reconciler may ONLY ask (the pursuit table's
/// resolve-only row). Kills mapping not-yet to absence, and kills a resend
/// rule that ignores the wanted table — either sends the create here, booking
/// the room Lucy is cancelling.
#[tokio::test]
async fn a_requested_cancellation_only_asks_and_never_sends() {
    let h = harness().await;
    let id = BookingId::new("BKG-ASKONLY");
    awaiting(&h, &id, requirements()).await;
    // The create is CALLED and answers nothing — the council did no work, so
    // the honest lookup answer below is "not yet visible".
    h.council.script([Script::GoQuiet("nothing")]);
    h.coordinator
        .propose(&id, BookingProposal::Book, &authority())
        .await
        .expect("book");
    let effect = in_flight_effect(&h, &id).await;
    h.coordinator
        .propose(
            &id,
            BookingProposal::Cancel {
                reason: "stop".to_owned(),
            },
            &authority(),
        )
        .await
        .expect("cancel");

    for expected_attempts in [2, 3] {
        h.clock.advance(10_000);
        let attended = h.reconciliation.attend(&effect).await.expect("attend");
        assert_eq!(
            attended,
            Attended::StillUnknown {
                attempts_started: expected_attempts
            },
            "the intent could still be committed, so nothing may conclude"
        );
    }

    let executes = h
        .council
        .wire_log()
        .iter()
        .filter(|(op, _)| *op == WireOp::Execute)
        .count();
    assert_eq!(
        executes, 1,
        "exactly the original send — recovery asked twice and sent NOTHING"
    );
    let still = h.repo.load(&id).await.expect("load");
    assert_eq!(still.state.name(), "CancellationRequested");
    assert_eq!(
        h.repo.load_effect(&effect).await.expect("intent").status,
        EffectStatus::Unknown
    );
}

/// The pursuit table's other clause for resolve-only: an intent that was never
/// even ATTEMPTED (`Prepared` — the crash window between Phase A's commit and
/// Phase B's mark) is still only asked about once the desire is withdrawn.
/// Kills a first-send leg that dispatches on status alone.
#[tokio::test]
async fn a_never_attempted_create_is_not_sent_after_cancellation_is_requested() {
    let h = harness().await;
    let id = BookingId::new("BKG-NEVERSENT");
    awaiting(&h, &id, requirements()).await;

    // The crash state, built by the same committed operation a crash would
    // leave behind: Phase A done (intent durable, in-flight state committed),
    // Phase B never reached — `Prepared`, 0/0.
    let effect = derive_effect_intent_id(&id, OperationKind::Book, AT_BOOK);
    let loaded = h.repo.load(&id).await.expect("load");
    let booking = townhall_domain::Booking::from(&loaded);
    h.repo
        .prepare_effect(PrepareEffect {
            booking_id: id.clone(),
            source_version: loaded.version,
            canonical_plan: book_plan(),
            next: townhall_domain::Booking {
                state: townhall_domain::BookingState::BookingInProgress(
                    townhall_domain::BookingInProgress {
                        effect_intent_id: effect.clone(),
                    },
                ),
                active_effect: Some(effect.clone()),
                ..booking
            },
            audit: TransitionAudit::driven_by(&BookingProposal::Book),
        })
        .await
        .expect("phase A");

    h.coordinator
        .propose(
            &id,
            BookingProposal::Cancel {
                reason: "never mind".to_owned(),
            },
            &authority(),
        )
        .await
        .expect("cancel");

    h.clock.advance(10_000);
    let attended = h.reconciliation.attend(&effect).await.expect("attend");
    assert_eq!(
        attended,
        Attended::StillUnknown {
            attempts_started: 1
        }
    );
    assert_eq!(
        h.council.call_count(),
        0,
        "the create was NEVER sent: withdrawn means withdrawn, even for Prepared"
    );
    assert_eq!(
        h.council
            .wire_log()
            .iter()
            .filter(|(op, _)| *op == WireOp::Resolve)
            .count(),
        1,
        "recovery asked — asking is all it may do here"
    );
}

/// Per-call accounting, pinned directly (plan review round 3): one uncontended
/// query-then-resend turn moves the pursuit row by exactly two on BOTH
/// columns, and the wire shows the ask strictly before the send.
///
/// (The plan sketched this from a crashed 1/0 start; a real crash start
/// belongs to test 12's process-level fixture. The honest protocol-level
/// start is 1/1 — the property pinned is identical: +2 started, +2 finished,
/// with DISTINCT finishes per call, killing an implementation that folds two
/// returns into one finish or two departures into one start.)
#[tokio::test]
async fn a_query_and_resend_turn_counts_both_attempts_and_asks_first() {
    let h = harness().await;
    let id = BookingId::new("BKG-TWOCALLS");
    awaiting(&h, &id, requirements()).await;
    h.council.script([Script::GoQuiet("nothing happened")]);
    h.coordinator
        .propose(&id, BookingProposal::Book, &authority())
        .await
        .expect("book");
    let effect = in_flight_effect(&h, &id).await;
    let (started, finished) = pursuit_counts(&h, &effect).await;
    assert_eq!((started, finished), (1, 1), "the failed first send");

    // One turn: ask (the council's authenticated "nothing yet"), then — the
    // booking is still wanted — send the same identity. The fake books it.
    h.clock.advance(10_000);
    let attended = h.reconciliation.attend(&effect).await.expect("attend");
    assert_eq!(attended, Attended::Settled);
    assert_eq!(h.repo.load(&id).await.expect("load").state.name(), "Booked");

    let (started, finished) = pursuit_counts(&h, &effect).await;
    assert_eq!(
        (started, finished),
        (3, 3),
        "two wire calls, two marks each side — ADR-019's contract verbatim"
    );
    let log = h.council.wire_log();
    let ops: Vec<WireOp> = log
        .iter()
        .filter(|(_, logged)| *logged == effect.as_str())
        .map(|(op, _)| *op)
        .collect();
    assert_eq!(
        ops,
        vec![WireOp::Execute, WireOp::Resolve, WireOp::Execute],
        "the resend ASKED first — a blind resend is what idempotency would hide"
    );
    assert_eq!(h.council.booking_count(), 1);
}

async fn pursuit_counts(h: &Harness, effect: &EffectIntentId) -> (i64, i64) {
    sqlx::query_as::<_, (i64, i64)>(
        "SELECT attempts_started, attempts_finished FROM effect_intents \
         WHERE effect_intent_id = ?",
    )
    .bind(effect.as_str())
    .fetch_one(h.repo.pool())
    .await
    .expect("the intent row")
}

/// Gate M16, end to end: the distinction `NeedsHuman` destroyed, driven through
/// the doors. Exhaustion at `CancellationRequested`, then the late answer
/// arrives — and lands as "now cancel it", never as "Booked": the handoff
/// fires, the cancel effect is minted and SENT, and the story ends `Cancelled`.
#[tokio::test]
async fn an_escalated_cancellation_is_finished_by_the_late_fact() {
    let h = harness().await;
    let id = BookingId::new("BKG-M16");
    awaiting(&h, &id, requirements()).await;
    // The council BOOKS the room and the answer is eaten: the late fact will
    // be real.
    h.council.script([Script::SucceedThenGoQuiet("eaten")]);
    h.coordinator
        .propose(&id, BookingProposal::Book, &authority())
        .await
        .expect("book");
    let effect = in_flight_effect(&h, &id).await;
    h.coordinator
        .propose(
            &id,
            BookingProposal::Cancel {
                reason: "waited too long".to_owned(),
            },
            &authority(),
        )
        .await
        .expect("cancel");

    // Spend the budget asking (resolve-only: no send can happen here), then
    // the sixth claim escalates.
    h.council.script([
        Script::GoQuiet("nothing"),
        Script::GoQuiet("nothing"),
        Script::GoQuiet("nothing"),
        Script::GoQuiet("nothing"),
    ]);
    for turn in 0..5 {
        h.clock.advance(10_000);
        let attended = h.reconciliation.attend(&effect).await.expect("attend");
        if turn == 4 {
            assert_eq!(attended, Attended::Escalated, "the budget is spent");
        }
    }
    assert_eq!(
        h.repo.escalated_unresolved(10).await.expect("queue"),
        vec![effect.clone()]
    );

    // The late answer: the council held the booking all along. The handoff —
    // finalise the book intent, mint the cancel successor, move to
    // CancellingBooking — is exactly what NeedsHuman could not express.
    h.clock.advance(townhall_store::MAX_CADENCE_MS + 1);
    let attended = h.reconciliation.attend(&effect).await.expect("attend");
    assert_eq!(attended, Attended::Settled);
    let handed = h.repo.load(&id).await.expect("load");
    assert_eq!(handed.state.name(), "CancellingBooking");
    assert!(
        h.repo
            .escalated_unresolved(10)
            .await
            .expect("queue")
            .is_empty(),
        "the question was answered — the queue is a predicate, and it is false now"
    );

    // The minted cancel intent is due, wanted, never attempted: recovery's
    // first-send leg executes it, carrying the canceller the STATE remembered.
    let due = h.reconciliation.due(10).await.expect("due");
    assert_eq!(due.len(), 1, "the cancel successor is recovery's next job");
    let cancel_effect = due[0].clone();
    let attended = h
        .reconciliation
        .attend(&cancel_effect)
        .await
        .expect("attend");
    assert_eq!(attended, Attended::Settled);
    assert_eq!(
        h.repo.load(&id).await.expect("load").state.name(),
        "Cancelled"
    );
    let sent = h.council.calls();
    let last = sent.last().expect("the cancel was sent");
    assert!(
        matches!(
            &last.plan,
            BookingEffect::CancelBooking { principal, .. }
                if *principal == PrincipalId::new("lucy")
        ),
        "the plan carries WHO asked to cancel, across the whole ambiguous window"
    );
    assert_eq!(
        h.repo
            .load_effect(&cancel_effect)
            .await
            .expect("intent")
            .status,
        EffectStatus::Confirmed
    );
}

/// Gate M3, the composed race: a cancel send held past its lease's expiry, a
/// second worker escalating the intent meanwhile, and the held call's answer
/// landing LATE — the stale owner's pursuit writes are fenced to nothing, and
/// the verified fact still settles through the version-fenced fact door.
#[tokio::test]
async fn an_escalation_during_a_held_call_is_fenced_and_the_fact_still_lands() {
    let h = harness().await;
    let id = BookingId::new("BKG-M3");
    awaiting(&h, &id, requirements()).await;
    h.coordinator
        .propose(&id, BookingProposal::Book, &authority())
        .await
        .expect("book to Booked");
    // The ordinary cancellation's send answers nothing: intent Unknown, 1/1.
    h.council.script([Script::GoQuiet("answer lost")]);
    h.coordinator
        .propose(
            &id,
            BookingProposal::Cancel {
                reason: "plans changed".to_owned(),
            },
            &authority(),
        )
        .await
        .expect("cancel");
    let cancel_effect = in_flight_effect(&h, &id).await;

    // Three quiet queries: marks 2-4.
    h.council.script([
        Script::GoQuiet("nothing"),
        Script::GoQuiet("nothing"),
        Script::GoQuiet("nothing"),
    ]);
    for _ in 0..3 {
        h.clock.advance(10_000);
        let attended = h
            .reconciliation
            .attend(&cancel_effect)
            .await
            .expect("attend");
        assert!(matches!(attended, Attended::StillUnknown { .. }));
    }

    // Turn five: claims at 4 < 5, queries (mark 5, the honest not-yet),
    // resends (mark 6) — and the send is HELD at the gate.
    let gate = Arc::new(ExecuteGate::default());
    h.council.gate_executes(Arc::clone(&gate));
    h.clock.advance(10_000);
    let worker_one = {
        let reconciliation = Reconciliation::new(
            Arc::new(Coordinator::new(
                Arc::clone(&h.repo),
                Arc::clone(&h.council),
                Arc::new(CouncilVerifier),
                Arc::new(FixedAvailability::new(facts())),
            )),
            Arc::clone(&h.council),
        );
        let effect = cancel_effect.clone();
        tokio::spawn(async move { reconciliation.attend(&effect).await })
    };
    // The call is genuinely on the wire — witnessed, never slept for.
    gate.arrived
        .acquire()
        .await
        .expect("the held call arrives")
        .forget();

    // The lease expires under the held call; worker two takes over and finds
    // the budget spent: escalation, under ITS token.
    h.clock.advance(30_001);
    let attended = h
        .reconciliation
        .attend(&cancel_effect)
        .await
        .expect("attend");
    assert_eq!(attended, Attended::Escalated, "worker two gave up honestly");

    // The council finally answers worker one's held send. The stale owner's
    // pursuit writes match nothing — but the FACT is version-fenced, not
    // lease-fenced, and it lands.
    gate.release.add_permits(1);
    let attended = worker_one
        .await
        .expect("the task ran")
        .expect("the turn ran");
    assert_eq!(
        attended,
        Attended::Settled,
        "the answer outlives the lease: the cancellation is real"
    );

    assert_eq!(
        h.repo.load(&id).await.expect("load").state.name(),
        "Cancelled"
    );
    assert!(
        h.repo
            .escalated_unresolved(10)
            .await
            .expect("queue")
            .is_empty(),
        "settled means answered: the escalated question leaves the queue"
    );
    let (started, finished, escalated_at, escalation_attempts) =
        sqlx::query_as::<_, (i64, i64, Option<i64>, Option<i64>)>(
            "SELECT attempts_started, attempts_finished, escalated_at_ms, \
             escalation_attempts FROM effect_intents WHERE effect_intent_id = ?",
        )
        .bind(cancel_effect.as_str())
        .fetch_one(h.repo.pool())
        .await
        .expect("row");
    assert!(escalated_at.is_some(), "exactly one marker was written");
    assert_eq!(escalation_attempts, Some(6), "derived IN the write");
    assert_eq!(
        (started, finished),
        (6, 5),
        "the fence ate exactly the stale owner's finish — nothing else"
    );
}

// --------------------------------------------- test 17: the re-apply path

/// A repository that loses the FIRST finalize on purpose: before delegating
/// it, a competing Cancel commits through the inner handle — so the settle
/// under test genuinely receives `StaleVersion` and must re-apply the same
/// verified fact against the state that beat it. Everything else delegates.
struct RacingRepo {
    inner: Arc<SqliteBookingRepository>,
    finalize_attempts: std::sync::atomic::AtomicUsize,
    inject: std::sync::Mutex<Option<InjectedCancel>>,
    /// A competing commit performed immediately before THIS wrapper's first
    /// `commit` delegation — the "winner lands between the load and the CAS"
    /// schedule for a LOCAL transition.
    commit_inject: std::sync::Mutex<Option<InjectedCancel>>,
    /// A rival's whole Phase A performed immediately before this wrapper's
    /// first `prepare_effect` delegation — the schedule where the caller's
    /// prepare finds the rival's intent and comes back `replayed`.
    prepare_inject: std::sync::Mutex<Option<PrepareEffect>>,
}

impl RacingRepo {
    fn passthrough(inner: Arc<SqliteBookingRepository>) -> Self {
        Self {
            inner,
            finalize_attempts: std::sync::atomic::AtomicUsize::new(0),
            inject: std::sync::Mutex::new(None),
            commit_inject: std::sync::Mutex::new(None),
            prepare_inject: std::sync::Mutex::new(None),
        }
    }
}

struct InjectedCancel {
    id: BookingId,
    version: u64,
    next: townhall_domain::Booking,
    audit: TransitionAudit,
}

#[async_trait::async_trait]
impl BookingRepository for RacingRepo {
    async fn create(
        &self,
        booking: NewBooking,
    ) -> Result<townhall_domain::BookingAggregate, StoreError> {
        self.inner.create(booking).await
    }
    async fn load(&self, id: &BookingId) -> Result<townhall_domain::BookingAggregate, StoreError> {
        self.inner.load(id).await
    }
    async fn commit(
        &self,
        id: &BookingId,
        expected_version: u64,
        next: townhall_domain::Booking,
        audit: TransitionAudit,
    ) -> Result<townhall_domain::BookingAggregate, StoreError> {
        let rival = self
            .commit_inject
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .take();
        if let Some(rival) = rival {
            self.inner
                .commit(&rival.id, rival.version, rival.next, rival.audit)
                .await
                .expect("the competing commit lands first");
        }
        self.inner.commit(id, expected_version, next, audit).await
    }
    async fn audit_events(&self, id: &BookingId) -> Result<Vec<AuditEvent>, StoreError> {
        self.inner.audit_events(id).await
    }
    async fn prepare_effect(&self, request: PrepareEffect) -> Result<PreparedEffect, StoreError> {
        let rival = self
            .prepare_inject
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .take();
        if let Some(rival) = rival {
            self.inner
                .prepare_effect(rival)
                .await
                .expect("the rival's Phase A commits first");
        }
        self.inner.prepare_effect(request).await
    }
    async fn finalize_effect(
        &self,
        request: FinalizeEffect,
    ) -> Result<FinalizedEffect, StoreError> {
        let first = self
            .finalize_attempts
            .fetch_add(1, std::sync::atomic::Ordering::SeqCst)
            == 0;
        if first {
            let injected = self
                .inject
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner)
                .take();
            if let Some(cancel) = injected {
                // The competing writer wins the CAS between this settle's
                // classification and its commit.
                self.inner
                    .commit(&cancel.id, cancel.version, cancel.next, cancel.audit)
                    .await
                    .expect("the competing Cancel commits first");
            }
        }
        self.inner.finalize_effect(request).await
    }
    async fn handoff_effect(&self, request: HandoffEffect) -> Result<HandedOffEffect, StoreError> {
        self.inner.handoff_effect(request).await
    }
    async fn load_effect(
        &self,
        id: &EffectIntentId,
    ) -> Result<townhall_domain::EffectIntent, StoreError> {
        self.inner.load_effect(id).await
    }
    async fn due_effects(&self, limit: u32) -> Result<Vec<EffectIntentId>, StoreError> {
        self.inner.due_effects(limit).await
    }
    async fn claim_effect(
        &self,
        id: &EffectIntentId,
        lease_ms: i64,
    ) -> Result<Option<ClaimedEffect>, StoreError> {
        self.inner.claim_effect(id, lease_ms).await
    }
    async fn note_attempt_started(
        &self,
        id: &EffectIntentId,
        token: i64,
    ) -> Result<bool, StoreError> {
        self.inner.note_attempt_started(id, token).await
    }
    async fn note_attempt_finished(
        &self,
        id: &EffectIntentId,
        token: i64,
        next_attempt_after_ms: i64,
    ) -> Result<bool, StoreError> {
        self.inner
            .note_attempt_finished(id, token, next_attempt_after_ms)
            .await
    }
    async fn release_lease(&self, id: &EffectIntentId, token: i64) -> Result<(), StoreError> {
        self.inner.release_lease(id, token).await
    }
    async fn mark_escalated(
        &self,
        id: &EffectIntentId,
        token: i64,
        long_cadence_ms: i64,
    ) -> Result<EscalationWrite, StoreError> {
        self.inner.mark_escalated(id, token, long_cadence_ms).await
    }
    async fn escalated_unresolved(&self, limit: u32) -> Result<Vec<EffectIntentId>, StoreError> {
        self.inner.escalated_unresolved(limit).await
    }
    async fn retry_hint_ms(&self, id: &EffectIntentId) -> Result<Option<i64>, StoreError> {
        self.inner.retry_hint_ms(id).await
    }
}

/// Test 17: a post-expiry `EffectAbsent` loses its CAS to a competing Cancel
/// and is RE-APPLIED against the state that won — the original ADR-016 race,
/// through the full re-apply path, with the loss forced deterministically.
/// The witnesses are the audit trail's ORDER and the finalize count; the final
/// state alone would prove nothing.
#[tokio::test]
async fn a_lost_cas_reapplies_the_same_verified_absence() {
    let h = harness().await;
    let id = BookingId::new("BKG-REAPPLY");
    awaiting(&h, &id, requirements()).await;
    h.council.script([Script::GoQuiet("never arrived")]);
    h.coordinator
        .propose(&id, BookingProposal::Book, &authority())
        .await
        .expect("book");
    let effect = in_flight_effect(&h, &id).await;
    let in_flight = h.repo.load(&id).await.expect("load");
    assert_eq!(in_flight.state.name(), "BookingInProgress");

    // The competing writer's commit, primed: BookingInProgress →
    // CancellationRequested at the version the settle will load.
    let booking = townhall_domain::Booking::from(&in_flight);
    let competing = InjectedCancel {
        id: id.clone(),
        version: in_flight.version,
        next: townhall_domain::Booking {
            state: townhall_domain::BookingState::CancellationRequested(
                townhall_domain::CancellationRequested {
                    effect_intent_id: effect.clone(),
                    cancelled_by: PrincipalId::new("lucy"),
                },
            ),
            active_effect: Some(effect.clone()),
            ..booking
        },
        audit: TransitionAudit::driven_by(&BookingProposal::Cancel {
            reason: "race".to_owned(),
        }),
    };
    let racing = Arc::new(RacingRepo {
        inject: std::sync::Mutex::new(Some(competing)),
        ..RacingRepo::passthrough(Arc::clone(&h.repo))
    });
    let coordinator = Coordinator::new(
        Arc::clone(&racing),
        Arc::clone(&h.council),
        Arc::new(CouncilVerifier),
        Arc::new(FixedAvailability::new(facts())),
    );

    // The verified post-expiry absence — monotonic by ADR-016's construction,
    // which is exactly what makes re-applying it safe.
    let fact = Verified::assert_verified(VerifiedProviderFact::EffectAbsent {
        effect_intent_id: effect.clone(),
    });
    let outcome = coordinator.observe(&id, fact).await.expect("the turn runs");
    let BoundaryOutcome::Committed(aggregate) = outcome else {
        panic!("the re-applied fact must commit, got {outcome:?}");
    };
    assert_eq!(
        aggregate.state.name(),
        "Cancelled",
        "absence under CancellationRequested means: nothing to cancel — done"
    );

    assert_eq!(
        racing
            .finalize_attempts
            .load(std::sync::atomic::Ordering::SeqCst),
        2,
        "the first finalize was refused; the second carried the re-application"
    );
    let audit = h.repo.audit_events(&id).await.expect("audit");
    let tail: Vec<(String, String, u64, u64)> = audit
        .iter()
        .rev()
        .take(2)
        .rev()
        .map(|row| {
            (
                row.driver_kind.name().to_owned(),
                row.driver_detail.clone(),
                row.from_version,
                row.to_version,
            )
        })
        .collect();
    assert_eq!(
        tail,
        vec![
            (
                "Proposal".to_owned(),
                "Cancel".to_owned(),
                in_flight.version,
                in_flight.version + 1
            ),
            (
                "Fact".to_owned(),
                "EffectAbsent".to_owned(),
                in_flight.version + 1,
                in_flight.version + 2
            ),
        ],
        "the Cancel won first, and the SAME fact then moved the winner's state"
    );
    assert_eq!(
        h.repo.load_effect(&effect).await.expect("intent").status,
        EffectStatus::Absent
    );
}

/// The attribution payoff of the `CancelBooking` storage break, at BOTH remaining
/// doors (PR #16 review, HIGH): a refusal on a cancellation intent is
/// attributed to the CANCELLER from the persisted plan — asserted with a
/// canceller who is not the booker, so a fallback to the booking's principal
/// (or to the pre-F empty string) fails by name.
#[tokio::test]
async fn a_refusal_on_a_cancellation_intent_is_attributed_to_the_canceller() {
    let (h, log) = harness_with_denials().await;
    let id = BookingId::new("BKG-CXLBLAME");
    awaiting(&h, &id, requirements()).await;
    h.coordinator
        .propose(&id, BookingProposal::Book, &authority())
        .await
        .expect("book to Booked");

    // Marco — not Lucy — asks to cancel, and the cancel's answer goes quiet:
    // the persisted CancelBooking plan carries HIS name.
    let mut marco = authority();
    marco.principal = PrincipalId::new("marco");
    h.council.script([Script::GoQuiet("answer lost")]);
    h.coordinator
        .propose(
            &id,
            BookingProposal::Cancel {
                reason: "marco's call".to_owned(),
            },
            &marco,
        )
        .await
        .expect("cancel");
    let cancel_effect = in_flight_effect(&h, &id).await;

    // Fact door: a CancellationExists naming the WRONG reference is refused by
    // the plan binding — and the row names marco, from the plan, because the
    // fact itself carries no principal.
    let wrong_ref = Verified::assert_verified(VerifiedProviderFact::CancellationExists {
        effect_intent_id: cancel_effect.clone(),
        booking_ref: CouncilBookingRef::new("TH-00000"),
    });
    let outcome = h.coordinator.observe(&id, wrong_ref).await.expect("turn");
    assert!(
        matches!(
            outcome,
            BoundaryOutcome::Denied(BookingError::EffectPlanMismatch {
                field: "booking_ref"
            })
        ),
        "a reference the plan never named: {outcome:?}"
    );

    // System-event door: a stale CANCEL-plan intent (planted raw — no current
    // door can mint it, and the reconciler must survive rows history wrote)
    // whose exhausted chase names an effect the booking is not waiting on. The
    // plan is the live cancel's own persisted JSON — CancelBooking, carrying
    // marco — copied rather than re-serialized, so the fixture cannot drift
    // from what the store actually writes.
    let stale_plan: String = sqlx::query_scalar(
        "SELECT canonical_plan_json FROM effect_intents WHERE effect_intent_id = ?",
    )
    .bind(cancel_effect.as_str())
    .fetch_one(h.repo.pool())
    .await
    .expect("the live cancel plan");
    sqlx::query(
        r"
        INSERT INTO effect_intents (effect_intent_id, booking_id, operation_kind,
                                    source_version, canonical_plan_json, status,
                                    expires_at_ms, created_at_ms, updated_at_ms,
                                    attempts_started, attempts_finished)
        VALUES (?, ?, 'Cancel', 99, ?, 'Unknown', ?, 0, 0, 1000, 1000)
        ",
    )
    .bind("EFF-STALE-CANCEL")
    .bind(id.to_string())
    .bind(&stale_plan)
    .bind(i64::MAX / 2)
    .execute(h.repo.pool())
    .await
    .expect("plant");
    let attended = h
        .reconciliation
        .attend(&EffectIntentId::new("EFF-STALE-CANCEL"))
        .await
        .expect("attend");
    assert_eq!(attended, Attended::NotDue);

    let rows = log.rows().await.expect("rows");
    assert_eq!(rows.len(), 2, "one refusal per door: {rows:?}");
    let fact_row = rows
        .iter()
        .find(|row| row.driver_kind == "Fact")
        .expect("the fact-door row");
    assert_eq!(fact_row.driver_detail, "CancellationExists");
    assert_eq!(fact_row.reason, "EffectPlanMismatch");
    assert_eq!(
        fact_row.principal, "marco",
        "attributed to the CANCELLER, from the persisted plan"
    );
    let event_row = rows
        .iter()
        .find(|row| row.driver_kind == "SystemEvent")
        .expect("the system-event row");
    assert_eq!(event_row.driver_detail, "ReconciliationExhausted");
    assert_eq!(event_row.reason, "EffectMismatch");
    assert_eq!(event_row.principal, "marco");
}

// ---------------------------------------- M5 groundwork: the versioned turn

/// Schedule (a) of the 412 contract (ADR-021): the winner commits BEFORE the
/// stale request enters the turn — the pre-classification comparison refuses,
/// carrying the fresh version, and the loser changed nothing.
#[tokio::test]
async fn a_stale_expectation_is_refused_before_classification() {
    let h = harness().await;
    let id = BookingId::new("BKG-STALE-A");
    awaiting(&h, &id, requirements()).await;
    let seen = h.repo.load(&id).await.expect("load").version;

    // The winner: a real committed turn at the version the loser also saw.
    let won = h
        .coordinator
        .propose_at(
            &id,
            seen,
            BookingProposal::UpdateRequirements { attendees: None },
            &authority(),
        )
        .await
        .expect("the winner's turn runs");
    assert!(matches!(won, BoundaryOutcome::Committed(_)));
    let after_winner = h.repo.load(&id).await.expect("load");
    assert_eq!(after_winner.version, seen + 1);
    let audit_after_winner = h.repo.audit_events(&id).await.expect("audit").len();

    // The loser, still holding the old tag.
    let refused = h
        .coordinator
        .propose_at(&id, seen, BookingProposal::Book, &authority())
        .await;
    let Err(ServiceError::PreconditionFailed { current }) = refused else {
        panic!("a stale expectation must be refused, got {refused:?}");
    };
    assert_eq!(current, seen + 1, "the refusal carries the fresh ETag");
    assert_eq!(
        h.repo.load(&id).await.expect("load").version,
        seen + 1,
        "the loser changed nothing"
    );
    assert_eq!(
        h.repo.audit_events(&id).await.expect("audit").len(),
        audit_after_winner,
        "no audit row for a refused precondition"
    );
    assert_eq!(h.council.call_count(), 0, "and nothing touched the wire");
}

/// Schedule (b): the winner commits BETWEEN the turn's load and its CAS — the
/// comparison passed, so only the version-bound CAS can refuse, and it does,
/// surfacing as the same `PreconditionFailed` with the fresh version.
#[tokio::test]
async fn a_cas_loss_mid_turn_is_refused_as_stale() {
    let h = harness().await;
    let id = BookingId::new("BKG-STALE-B");
    awaiting(&h, &id, requirements()).await;
    let loaded = h.repo.load(&id).await.expect("load");

    let competing = InjectedCancel {
        id: id.clone(),
        version: loaded.version,
        next: townhall_domain::Booking {
            state: townhall_domain::BookingState::NeedsRevalidation(
                townhall_domain::NeedsRevalidation {
                    selected: townhall_domain::Booking::from(&loaded).selected_venue,
                },
            ),
            availability: None,
            ..townhall_domain::Booking::from(&loaded)
        },
        audit: TransitionAudit::driven_by(&BookingProposal::UpdateRequirements { attendees: None }),
    };
    let racing = Arc::new(RacingRepo {
        commit_inject: std::sync::Mutex::new(Some(competing)),
        ..RacingRepo::passthrough(Arc::clone(&h.repo))
    });
    let coordinator = Coordinator::new(
        Arc::clone(&racing),
        Arc::clone(&h.council),
        Arc::new(CouncilVerifier),
        Arc::new(FixedAvailability::new(facts())),
    );

    // A local turn (UpdateRequirements) whose CAS the injected winner beats.
    let refused = coordinator
        .propose_at(
            &id,
            loaded.version,
            BookingProposal::UpdateRequirements {
                attendees: Some(25),
            },
            &authority(),
        )
        .await;
    let Err(ServiceError::PreconditionFailed { current }) = refused else {
        panic!("a mid-turn CAS loss must surface as stale, got {refused:?}");
    };
    assert_eq!(
        current,
        loaded.version + 1,
        "the winner's version, freshly read"
    );
}

/// The replay schedule, through BOTH surfaces against the same race (ADR-021):
/// a rival's whole Phase A lands between the load and this turn's prepare.
/// The versionless surface keeps M4's contract — `Unresolved`, recovery owns
/// it; the versioned surface refuses it as stale, because THIS caller
/// performed no mutation and `Unresolved` would claim work it did not do.
#[tokio::test]
async fn a_replayed_prepare_is_unresolved_in_process_and_stale_over_a_version() {
    for versioned in [false, true] {
        let h = harness().await;
        let id = BookingId::new("BKG-REPLAY");
        awaiting(&h, &id, requirements()).await;
        let loaded = h.repo.load(&id).await.expect("load");
        let effect = derive_effect_intent_id(&id, OperationKind::Book, loaded.version);

        // The rival's Phase A, verbatim: same key, same version, same plan.
        let rival = PrepareEffect {
            booking_id: id.clone(),
            source_version: loaded.version,
            canonical_plan: book_plan(),
            next: townhall_domain::Booking {
                state: townhall_domain::BookingState::BookingInProgress(
                    townhall_domain::BookingInProgress {
                        effect_intent_id: effect.clone(),
                    },
                ),
                active_effect: Some(effect.clone()),
                ..townhall_domain::Booking::from(&loaded)
            },
            audit: TransitionAudit::driven_by(&BookingProposal::Book),
        };
        let racing = Arc::new(RacingRepo {
            prepare_inject: std::sync::Mutex::new(Some(rival)),
            ..RacingRepo::passthrough(Arc::clone(&h.repo))
        });
        let coordinator = Coordinator::new(
            Arc::clone(&racing),
            Arc::clone(&h.council),
            Arc::new(CouncilVerifier),
            Arc::new(FixedAvailability::new(facts())),
        );

        if versioned {
            let refused = coordinator
                .propose_at(&id, loaded.version, BookingProposal::Book, &authority())
                .await;
            let Err(ServiceError::PreconditionFailed { current }) = refused else {
                panic!("a replayed prepare under a version must be stale: {refused:?}");
            };
            assert_eq!(current, loaded.version + 1);
        } else {
            let turn = coordinator
                .propose(&id, BookingProposal::Book, &authority())
                .await
                .expect("the turn runs");
            assert!(
                matches!(turn, BoundaryOutcome::Unresolved),
                "M4's contract verbatim: recovery owns a replay, got {turn:?}"
            );
        }
        assert_eq!(
            h.council.call_count(),
            0,
            "neither surface re-ran Phase B for a replay (versioned={versioned})"
        );
    }
}

/// The facade end to end at the protocol level: create (and its duplicate),
/// the projection's exported menu, the derived retry hint, the audit
/// projection, and the reconcile trigger following the chase to done.
#[tokio::test]
async fn the_facade_carries_the_whole_surface() {
    let h = harness().await;
    let coordinator = Arc::new(Coordinator::new(
        Arc::clone(&h.repo),
        Arc::clone(&h.council),
        Arc::new(CouncilVerifier),
        Arc::new(FixedAvailability::new(facts())),
    ));
    let reconciliation = Arc::new(Reconciliation::new(
        Arc::clone(&coordinator),
        Arc::clone(&h.council),
    ));
    let api = townhall_service::BookingApi::new(
        coordinator,
        reconciliation,
        Arc::new(townhall_service::fake::FixedCatalogue::of(Vec::new())),
        Arc::new(FixedAvailability::new(facts())),
    );

    let id = BookingId::new("BKG-FACADE");
    let created = api
        .create(id.clone(), requirements())
        .await
        .expect("created");
    assert_eq!(created.version, 0);
    assert_eq!(created.available_behaviours, &["SelectVenue", "Cancel"]);

    let duplicate = api.create(id.clone(), requirements()).await;
    let Err(townhall_service::ApiError::AlreadyExists { current }) = duplicate else {
        panic!("a duplicate create carries the existing version: {duplicate:?}");
    };
    assert_eq!(current, 0);

    // Walk to AwaitingBooking through the versioned surface only.
    let mut version = created.version;
    for proposal in [select(), BookingProposal::VerifySlot] {
        let mutated = api
            .propose_at(&id, version, proposal, &authority())
            .await
            .expect("committed");
        version = mutated.current_version;
    }
    let projection = api.read(&id).await.expect("read");
    assert_eq!(projection.state, "AwaitingBooking");
    assert_eq!(
        projection.available_behaviours,
        projection_menu_of(&h, &id).await,
        "the projection's menu IS the domain's export"
    );

    // Book with the answer eaten: 202 semantics — Unresolved, and the hint is
    // the STORE's schedule (the full retry cadence, freshly written).
    h.council.script([Script::SucceedThenGoQuiet("eaten")]);
    let mutated = api
        .propose_at(&id, version, BookingProposal::Book, &authority())
        .await
        .expect("the turn runs");
    assert!(matches!(mutated.outcome, BoundaryOutcome::Unresolved));
    assert_eq!(
        mutated.retry_after_ms,
        Some(5_000),
        "the hint projects the durable schedule, not a constant"
    );

    // The reconcile trigger drives the chase to done — attend, never propose.
    h.clock.advance(10_000);
    let outcomes = api.attend_booking(&id).await.expect("attend");
    assert_eq!(outcomes, vec![Attended::Settled]);
    assert_eq!(api.read(&id).await.expect("read").state, "Booked");
    assert!(
        api.attend_booking(&id).await.expect("attend").is_empty(),
        "nothing in flight, nothing to attend"
    );

    let audit = api.audit(&id).await.expect("audit");
    let last = audit.last().expect("rows");
    assert_eq!(last.driver_kind, "Fact");
    assert_eq!(last.driver_detail, "BookingExists");
    let missing = api.audit(&BookingId::new("BKG-NOBODY")).await;
    assert!(matches!(
        missing,
        Err(townhall_service::ApiError::UnknownBooking)
    ));
}

async fn projection_menu_of(h: &Harness, id: &BookingId) -> &'static [&'static str] {
    h.repo.load(id).await.expect("load").state.proposal_menu()
}

/// ADR-021's 503/local pair at the protocol level: with the availability
/// provider unreachable, asking about a slot is refused as `FactsUnavailable`
/// (the wire's 503 — nothing durable exists yet), while CANCELLING an
/// in-flight booking still commits — its cell never binds facts, and a dead
/// provider must not hold Lucy's withdrawal hostage.
#[tokio::test]
async fn an_unreachable_provider_denies_asking_but_not_cancelling() {
    let h = harness().await;
    let id = BookingId::new("BKG-DEADPROV");
    awaiting(&h, &id, requirements()).await;
    h.council.script([Script::GoQuiet("eaten")]);
    h.coordinator
        .propose(&id, BookingProposal::Book, &authority())
        .await
        .expect("book");

    let unreachable = Coordinator::new(
        Arc::clone(&h.repo),
        Arc::clone(&h.council),
        Arc::new(CouncilVerifier),
        Arc::new(FixedAvailability::unreachable(facts())),
    );

    // A fresh booking that needs facts: refused as could-not-ask, not as
    // answered-nothing.
    let other = BookingId::new("BKG-DEADPROV-2");
    h.repo
        .create(NewBooking {
            id: other.clone(),
            requirements: requirements(),
        })
        .await
        .expect("create");
    unreachable
        .propose(&other, select(), &authority())
        .await
        .expect("select commits — selection binds no facts");
    let refused = unreachable
        .propose(&other, BookingProposal::VerifySlot, &authority())
        .await
        .expect("the turn runs");
    assert!(
        matches!(
            refused,
            BoundaryOutcome::Denied(BookingError::FactsUnavailable)
        ),
        "could-not-ask has its own name: {refused:?}"
    );

    // Lucy's mid-flight withdrawal needs nothing from the provider.
    let cancelled = unreachable
        .propose(
            &id,
            BookingProposal::Cancel {
                reason: "provider down, mind changed".to_owned(),
            },
            &authority(),
        )
        .await
        .expect("the turn runs");
    let BoundaryOutcome::Committed(aggregate) = cancelled else {
        panic!("a dead provider must not hold the withdrawal hostage: {cancelled:?}");
    };
    assert_eq!(aggregate.state.name(), "CancellationRequested");
}
