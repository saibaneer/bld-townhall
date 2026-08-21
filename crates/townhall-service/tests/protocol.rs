//! Slice C's gate: the effect protocol, against an in-process council.
//!
//! Every failure here means the *protocol* is wrong. Slice D puts the same
//! protocol over HTTP, where a failure could also mean the client is
//! misconfigured — which is exactly why these run first.

use bld_kernel::{BoundaryOutcome, Capability, Verified};
use bld_types::{
    ActorId, AvailabilityGrant, BookingId, BookingRequirements, EffectAttempt, EffectIntentId,
    Money, PrincipalId, Provenance, SlotId, TimeWindow, VenueId,
};
use std::{path::PathBuf, sync::Arc};
use tempfile::TempDir;
use townhall_domain::{
    BookingEffect, BookingProposal, EffectStatus, OperationKind, SystemEvent, VenueFacts,
    VerifiedAuthority, VerifiedProviderFact,
};
use townhall_service::{
    Coordinator, ServiceError,
    fake::{CouncilVerifier, FAKE_GRANT, FakeCouncil, FixedAvailability, ObservedCouncil, Script},
};
use townhall_store::{
    BookingRepository, NewBooking, SqliteBookingRepository, derive_effect_intent_id,
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
    _temp: TempDir,
    path: PathBuf,
    repo: Arc<SqliteBookingRepository>,
    council: Arc<FakeCouncil>,
    coordinator: Sut,
}

async fn harness() -> Harness {
    let temp = TempDir::new().expect("temp dir");
    let path = temp.path().join("townhall.sqlite");
    let repo = Arc::new(
        SqliteBookingRepository::open(&path)
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
    Harness {
        _temp: temp,
        path,
        repo,
        council,
        coordinator,
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
    assert_eq!(
        intent.status,
        EffectStatus::Prepared,
        "nothing was established about this effect"
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
    assert_eq!(
        observation,
        Some((id.to_string(), EffectStatus::Prepared)),
        "the intent must be readable, and Prepared, from another connection during the call"
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

/// `NeedsHuman` is reachable only through the system-event door, and giving up
/// finalises the effect it gave up on — a `NeedsHuman` still pointing at a live
/// intent would invite a reconciler to keep chasing what was just abandoned.
#[tokio::test]
async fn exhausted_reconciliation_escalates_and_stops_pointing_at_the_effect() {
    let h = harness().await;
    let id = BookingId::new("BKG-EXHAUSTED");
    awaiting(&h, &id, requirements()).await;
    h.council.script([Script::GoQuiet("timed out")]);
    h.coordinator
        .propose(&id, BookingProposal::Book, &authority())
        .await
        .expect("book");
    let effect = in_flight_effect(&h, &id).await;

    let outcome = h
        .coordinator
        .record(
            &id,
            SystemEvent::ReconciliationExhausted {
                effect_intent_id: effect.clone(),
            },
        )
        .await
        .expect("no service error");

    let BoundaryOutcome::Committed(aggregate) = outcome else {
        panic!("exhaustion is an outcome and must commit, got {outcome:?}");
    };
    assert_eq!(aggregate.state.name(), "NeedsHuman");
    assert_eq!(
        aggregate.active_effect, None,
        "nothing may keep chasing an abandoned effect"
    );
    let intent = h.repo.load_effect(&effect).await.expect("the intent");
    // `Abandoned`, and the exact status is the assertion — `is_terminal()` alone
    // would have let `Absent` through, which asserts the council tombstoned the
    // intent. Nobody established that; we stopped asking.
    assert_eq!(
        intent.status,
        EffectStatus::Abandoned,
        "exhaustion must claim nothing about the provider"
    );
    assert_eq!(
        intent.provider_reference, None,
        "and must not invent a reference"
    );

    let last = h
        .repo
        .audit_events(&id)
        .await
        .expect("audit")
        .pop()
        .expect("a row");
    assert_eq!(
        last.driver_kind,
        Provenance::SystemEvent,
        "the runtime, not the council and not Lucy, concluded this"
    );
}

/// Exhaustion of a *different* effect says nothing about this booking.
#[tokio::test]
async fn exhaustion_of_another_effect_changes_nothing() {
    let h = harness().await;
    let id = BookingId::new("BKG-OTHEREFFECT");
    awaiting(&h, &id, requirements()).await;
    h.council.script([Script::GoQuiet("timed out")]);
    h.coordinator
        .propose(&id, BookingProposal::Book, &authority())
        .await
        .expect("book");
    let before = h.repo.load(&id).await.expect("load");

    let outcome = h
        .coordinator
        .record(
            &id,
            SystemEvent::ReconciliationExhausted {
                effect_intent_id: EffectIntentId::new("EFF-SOMEBODY-ELSE"),
            },
        )
        .await
        .expect("no service error");

    assert!(
        matches!(outcome, BoundaryOutcome::Denied(_)),
        "a refusal with a reason, not a silent gap, got {outcome:?}"
    );
    assert_eq!(
        h.repo.load(&id).await.expect("load").version,
        before.version,
        "nothing moved"
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
