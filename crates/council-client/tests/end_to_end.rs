//! The whole boundary, over a socket, through an unmodified coordinator.
//!
//! Slice C proved this protocol against an in-process fake. These tests run the
//! **same** coordinator against a real council over real TCP, and the point is
//! that nothing in `townhall-service` changed to make it work — the traits were
//! the seam, and swapping what sits behind them is all this took.
//!
//! Which is also why a failure here is worth more than a failure in the client's
//! own suite: if these pass, then whatever breaks in slice E under fault injection
//! is a *recovery* defect and not a networking one. That was the whole reason to
//! build D before E.

use bld_kernel::BoundaryOutcome;
use bld_types::{BookingId, BookingRequirements, Money, PrincipalId, SlotId, TimeWindow, VenueId};
use council_client::{CouncilClient, CouncilVerifier};
use council_wire::{CouncilKey, CouncilSigner};
use ed25519_dalek::SigningKey;
use mock_council::{Council, SeedSlot, clock::TestClock, pause::NeverPauses};
use sqlx::Row as _;
use std::sync::Arc;
use tempfile::TempDir;
use townhall_domain::{BookingError, BookingProposal, BookingState, VerifiedAuthority};
use townhall_service::Coordinator;
use townhall_store::{BookingRepository, NewBooking, SqliteBookingRepository};

const NOW: i64 = 1_000_000_000;
const TTL: i64 = 60_000;

/// Spec §11's four venues. One passes every guard; the other three each fail
/// exactly one, so an end-to-end denial is about the guard rather than about
/// several things being wrong at once.
const SLOTS: &[SeedSlot] = &[
    SeedSlot {
        venue_id: "TH-A",
        slot_id: "SLOT-A",
        fee_pence: 4_500,
        capacity: 30,
        accessible: true,
        available: true,
    },
    SeedSlot {
        venue_id: "TH-B",
        slot_id: "SLOT-A",
        fee_pence: 4_500,
        capacity: 30,
        accessible: false,
        available: true,
    },
    SeedSlot {
        venue_id: "TH-C",
        slot_id: "SLOT-A",
        fee_pence: 9_000,
        capacity: 30,
        accessible: true,
        available: true,
    },
    SeedSlot {
        venue_id: "TH-D",
        slot_id: "SLOT-A",
        fee_pence: 4_500,
        capacity: 10,
        accessible: true,
        available: true,
    },
];

type Sut = Coordinator<SqliteBookingRepository, CouncilClient, CouncilVerifier, CouncilClient>;

struct Harness {
    _dir: TempDir,
    council: Arc<Council>,
    repo: Arc<SqliteBookingRepository>,
    coordinator: Sut,
}

impl Harness {
    async fn new() -> Self {
        let dir = TempDir::new().expect("a temp dir");
        let clock = Arc::new(TestClock::at(NOW));
        let signer = Arc::new(CouncilSigner::new(SigningKey::from_bytes(&[7u8; 32])));
        let public = signer.verifying_key();

        let council = Arc::new(
            Council::build(
                dir.path().join("council.sqlite"),
                Arc::clone(&signer),
                clock as Arc<_>,
                Arc::new(NeverPauses),
                TTL,
            )
            .await
            .expect("open the council"),
        );
        council.seed(SLOTS).await.expect("seed");

        let listener = tokio::net::TcpListener::bind("127.0.0.1:0")
            .await
            .expect("bind an ephemeral port");
        let address = listener.local_addr().expect("a local address");
        let router = council.router();
        tokio::spawn(async move {
            let _ = axum::serve(listener, router).await;
        });

        let repo = Arc::new(
            SqliteBookingRepository::open(dir.path().join("townhall.sqlite"))
                .await
                .expect("open the repository"),
        );

        // The same client fills both the capability and the availability seats,
        // which is the honest shape: one provider, reached one way. Slice C's fake
        // needed two objects only because it modelled no catalogue.
        let client = Arc::new(CouncilClient::new(
            format!("http://{address}"),
            CouncilKey::new(public),
        ));

        let coordinator = Coordinator::new(
            Arc::clone(&repo),
            Arc::clone(&client),
            Arc::new(CouncilVerifier::new(CouncilKey::new(public))),
            client,
        );

        Self {
            _dir: dir,
            council,
            repo,
            coordinator,
        }
    }

    /// Walk a fresh booking as far as `AwaitingBooking` — the state `Book` leaves
    /// from — using only the coordinator's public entry point.
    async fn awaiting(&self, id: &BookingId, venue: &str) -> Result<(), BookingError> {
        self.repo
            .create(NewBooking {
                id: id.clone(),
                requirements: requirements(),
                owner: PrincipalId::new("lucy"),
            })
            .await
            .expect("create");

        self.propose(
            id,
            BookingProposal::SelectVenue {
                venue_id: VenueId::new(venue),
                slot_id: SlotId::new("SLOT-A"),
            },
        )
        .await?;
        self.propose(id, BookingProposal::VerifySlot).await?;
        Ok(())
    }

    /// One turn, with the denial surfaced rather than swallowed.
    async fn propose(
        &self,
        id: &BookingId,
        proposal: BookingProposal,
    ) -> Result<BookingState, BookingError> {
        match self
            .coordinator
            .propose(id, proposal, &authority(id))
            .await
            .expect("the turn should not fail at the transport level")
        {
            BoundaryOutcome::Committed(aggregate) => Ok(aggregate.state),
            BoundaryOutcome::Denied(error) => Err(error),
            other => panic!("expected a committed turn or a denial, got {other:?}"),
        }
    }

    async fn booking_count(&self) -> i64 {
        sqlx::query("SELECT COUNT(*) AS n FROM bookings")
            .fetch_one(self.council.pool())
            .await
            .expect("count the council's bookings")
            .get("n")
    }

    /// How many COUNCIL effect intents exist — `Book`/`Cancel`, the ones that
    /// reach the provider. Since M10 (ADR-030) `VerifySlot` mints a `Verify`
    /// availability intent too, but these tests are about council effects
    /// ("reaches the council", booking idempotency), so `Verify` is excluded —
    /// a double council booking still fails the count.
    async fn effect_count(&self) -> i64 {
        sqlx::query(
            "SELECT COUNT(*) AS n FROM effect_intents WHERE operation_kind IN ('Book', 'Cancel')",
        )
        .fetch_one(self.repo.pool())
        .await
        .expect("count our council effect intents")
        .get("n")
    }
}

fn requirements() -> BookingRequirements {
    BookingRequirements {
        purpose: "community meeting".to_owned(),
        requested_date: "2026-09-01".to_owned(),
        time_window: TimeWindow {
            from: "13:00".to_owned(),
            to: "17:00".to_owned(),
        },
        attendees: 20,
        wheelchair_accessible: true,
        max_fee: Money::from_pence(5_000),
    }
}

/// Lucy's grant over one booking, issued through the real approval path.
///
/// Resource-scoped because a grant names its booking (ADR-025); the fixture it
/// replaced carried capability flags that held for any id its bearer could
/// type.
fn authority(id: &BookingId) -> VerifiedAuthority {
    townhall_testkit::issuer::issue_blocking(&townhall_testkit::issuer::GrantSpec::own(
        "lucy",
        id.as_str(),
        5_000,
    ))
}

// ------------------------------------------------------------------ the happy path

/// Lucy books TH-A over HTTP. The whole way through, with nothing faked.
#[tokio::test]
async fn lucy_books_a_room_over_http() {
    let h = Harness::new().await;
    let id = BookingId::new("BKG-HTTP-1");
    h.awaiting(&id, "TH-A")
        .await
        .expect("reach AwaitingBooking");

    let state = h.propose(&id, BookingProposal::Book).await.expect("booked");

    let BookingState::Booked(booked) = state else {
        panic!("expected Booked, got {state:?}");
    };
    assert!(!booked.booking_ref.as_str().is_empty());

    // One booking at the council, one intent on our side, and they agree.
    assert_eq!(h.booking_count().await, 1);
    assert_eq!(h.effect_count().await, 1);

    let aggregate = h.repo.load(&id).await.expect("load");
    assert_eq!(
        aggregate.booking_ref.as_ref(),
        Some(&booked.booking_ref),
        "the council's reference is on the aggregate"
    );
    assert!(
        aggregate.active_effect.is_none(),
        "and the effect is finalised, not left in flight"
    );
}

/// The audit trail must attribute the confirmation to the **fact** door.
///
/// This is the property ADR-012 exists for: `Booked` was reached because the
/// council said so, not because anyone proposed it. If this row said `Proposal`,
/// the whole provenance argument would be decoration.
#[tokio::test]
async fn the_confirmation_is_attributed_to_the_fact_door() {
    let h = Harness::new().await;
    let id = BookingId::new("BKG-HTTP-AUDIT");
    h.awaiting(&id, "TH-A")
        .await
        .expect("reach AwaitingBooking");
    h.propose(&id, BookingProposal::Book).await.expect("booked");

    let (driver_kind, driver_detail): (String, String) = sqlx::query(
        r"
        SELECT driver_kind, driver_detail FROM audit_events
         WHERE booking_id = ?
         ORDER BY to_version DESC LIMIT 1
        ",
    )
    .bind(id.as_str())
    .fetch_one(h.repo.pool())
    .await
    .map(|row| (row.get("driver_kind"), row.get("driver_detail")))
    .expect("read the last audit row");

    assert_eq!(driver_kind, "Fact");
    assert_eq!(
        driver_detail, "BookingExists",
        "and it names which fact, not just its class"
    );
}

// ---------------------------------------------------- the three denials, each its own

/// TH-B is inaccessible and nothing else. Lucy needs an accessible room.
///
/// The denial must arrive with **no effect intent and no council booking** — a
/// boundary that reached outside before refusing would have caused a consequence
/// it then declined to own.
#[tokio::test]
async fn th_b_fails_accessibility_and_only_accessibility() {
    let h = Harness::new().await;
    let id = BookingId::new("BKG-HTTP-B");

    let denial = h
        .awaiting(&id, "TH-B")
        .await
        .expect_err("verification must refuse an inaccessible room");

    assert_eq!(denial, BookingError::AccessibilityRequired);
    assert_eq!(h.effect_count().await, 0);
    assert_eq!(h.booking_count().await, 0);
}

/// TH-C costs £90 against Lucy's £50 ceiling.
#[tokio::test]
async fn th_c_fails_the_fee_ceiling() {
    let h = Harness::new().await;
    let id = BookingId::new("BKG-HTTP-C");

    let denial = h
        .awaiting(&id, "TH-C")
        .await
        .expect_err("verification must refuse a room over the ceiling");

    assert_eq!(
        denial,
        BookingError::FeeExceeded {
            // TH-C's £90 exceeds Lucy's £50 authority ceiling too — authority
            // wins when both are exceeded (ADR-021).
            ceiling: townhall_domain::FeeCeiling::Authority,
        }
    );
    assert_eq!(h.effect_count().await, 0);
    assert_eq!(h.booking_count().await, 0);
}

/// TH-D holds 10 and Lucy has 20 guests.
#[tokio::test]
async fn th_d_fails_capacity() {
    let h = Harness::new().await;
    let id = BookingId::new("BKG-HTTP-D");

    let denial = h
        .awaiting(&id, "TH-D")
        .await
        .expect_err("verification must refuse a room too small");

    assert_eq!(
        denial,
        BookingError::CapacityInsufficient {
            capacity: 10,
            required: 20,
        }
    );
    assert_eq!(h.effect_count().await, 0);
    assert_eq!(h.booking_count().await, 0);
}

// ---------------------------------------------------------------- cancellation

/// Cancelling a confirmed booking, end to end, under the cancellation's own
/// effect identity.
#[tokio::test]
async fn lucy_cancels_a_confirmed_booking_over_http() {
    let h = Harness::new().await;
    let id = BookingId::new("BKG-HTTP-CANCEL");
    h.awaiting(&id, "TH-A")
        .await
        .expect("reach AwaitingBooking");
    h.propose(&id, BookingProposal::Book).await.expect("booked");

    let state = h
        .propose(
            &id,
            BookingProposal::Cancel {
                reason: "no longer needed".to_owned(),
            },
        )
        .await
        .expect("the cancellation is accepted");

    // `Cancel` on a confirmed booking reaches outside, so this turn commits the
    // in-flight state and the council's answer settles it in the same call.
    assert!(
        matches!(state, BookingState::Cancelled(_)),
        "expected Cancelled, got {state:?}"
    );

    // Two intents — the booking and the cancellation — because a cancellation is
    // an external effect with an identity of its own, not a flag on the first.
    assert_eq!(h.effect_count().await, 2);

    let cancelled_by: Option<String> = sqlx::query("SELECT cancelled_by FROM bookings LIMIT 1")
        .fetch_one(h.council.pool())
        .await
        .expect("read the council's booking")
        .get("cancelled_by");
    assert!(
        cancelled_by.is_some(),
        "the council recorded who cancelled it"
    );
}

// ------------------------------------------------------------ convergence

/// Re-proposing `Book` after it succeeded is `Undefined`, not a second booking.
///
/// `Booked` has no `Book` behaviour — the topology says so — so this never reaches
/// the council at all. The assertion that matters is the council's booking count
/// staying at one.
#[tokio::test]
async fn booking_twice_never_reaches_the_council_a_second_time() {
    let h = Harness::new().await;
    let id = BookingId::new("BKG-HTTP-TWICE");
    h.awaiting(&id, "TH-A")
        .await
        .expect("reach AwaitingBooking");
    h.propose(&id, BookingProposal::Book).await.expect("booked");

    let again = h
        .coordinator
        .propose(&id, BookingProposal::Book, &authority(&id))
        .await
        .expect("the turn should not fail");

    assert_eq!(
        again,
        BoundaryOutcome::Undefined,
        "Book does not exist in Booked"
    );
    assert_eq!(h.booking_count().await, 1);
    assert_eq!(h.effect_count().await, 1);
}

/// Two coordinators, one booking, one council. The council is asked once.
///
/// Both share the repository, so `prepare_effect` is the serialization point: the
/// loser sees a replay and stops rather than calling out. Proving it over HTTP
/// matters because a network call is where a duplicate would actually cost
/// something.
#[tokio::test]
async fn two_turns_racing_one_booking_ask_the_council_once() {
    let h = Harness::new().await;
    let id = BookingId::new("BKG-HTTP-RACE");
    h.awaiting(&id, "TH-A")
        .await
        .expect("reach AwaitingBooking");

    let authority = authority(&id);
    let first = h
        .coordinator
        .propose(&id, BookingProposal::Book, &authority);
    let second = h
        .coordinator
        .propose(&id, BookingProposal::Book, &authority);
    let (a, b) = tokio::join!(first, second);

    let outcomes = [a.expect("first turn"), b.expect("second turn")];
    assert!(
        outcomes
            .iter()
            .any(|outcome| matches!(outcome, BoundaryOutcome::Committed(_))),
        "one turn must commit: {outcomes:?}"
    );

    assert_eq!(
        h.booking_count().await,
        1,
        "the council must hold exactly one booking"
    );
    assert_eq!(h.effect_count().await, 1, "and we must hold one intent");
}
