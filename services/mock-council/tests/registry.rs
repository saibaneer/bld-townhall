//! The council's own gates: the four-state machine, the catalogue, expiry, and
//! the two halves of commit-before-response.
//!
//! These exercise the registry directly rather than over HTTP. That is on purpose
//! — a failure here is a protocol failure, and routing it through a socket first
//! only adds a second thing that could be wrong. The HTTP surface has its own
//! suite.

use council_wire::{CouncilSigner, EffectOutcome, GrantClaims};
use ed25519_dalek::SigningKey;
use mock_council::{
    Council, SeedSlot,
    clock::TestClock,
    pause::{NeverPauses, PausePoint, Pauses},
    registry::{ApplyCancellation, CreateBooking, OperationKind, Registry, ResolveEffect},
};
use sqlx::Row;
use std::sync::{
    Arc,
    atomic::{AtomicUsize, Ordering},
};
use tempfile::TempDir;

const NOW: i64 = 1_000_000_000;
const DEADLINE: i64 = 1_000_030_000;
const TTL: i64 = 60_000;

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
        venue_id: "TH-SHUT",
        slot_id: "SLOT-A",
        fee_pence: 4_500,
        capacity: 30,
        accessible: true,
        available: false,
    },
];

struct Harness {
    _dir: TempDir,
    council: Council,
    clock: Arc<TestClock>,
}

impl Harness {
    async fn new() -> Self {
        Self::with_pauses(Arc::new(NeverPauses)).await
    }

    async fn with_pauses(pauses: Arc<dyn Pauses>) -> Self {
        let dir = TempDir::new().expect("a temp dir");
        let clock = Arc::new(TestClock::at(NOW));
        let council = Council::build(
            dir.path().join("council.sqlite"),
            Arc::new(CouncilSigner::new(SigningKey::from_bytes(&[7u8; 32]))),
            Arc::clone(&clock) as Arc<_>,
            pauses,
            TTL,
        )
        .await
        .expect("open the council");
        council.seed(SLOTS).await.expect("seed");
        Self {
            _dir: dir,
            council,
            clock,
        }
    }

    fn registry(&self) -> &Arc<Registry> {
        self.council.registry()
    }

    /// A live grant for a slot, as the availability endpoint would issue it.
    async fn grant_for(&self, venue: &str, slot: &str) -> String {
        self.registry()
            .availability(venue, slot)
            .await
            .expect("availability")
            .expect("a known slot")
            .grant
    }

    async fn state_of(&self, effect_intent_id: &str) -> Option<String> {
        sqlx::query("SELECT state FROM effects WHERE effect_intent_id = ?")
            .bind(effect_intent_id)
            .fetch_optional(self.council.pool())
            .await
            .expect("read the registry")
            .map(|row| row.get::<String, _>("state"))
    }

    async fn row_version(&self, venue: &str, slot: &str) -> i64 {
        sqlx::query("SELECT row_version FROM venue_slots WHERE venue_id = ? AND slot_id = ?")
            .bind(venue)
            .bind(slot)
            .fetch_one(self.council.pool())
            .await
            .expect("read the row version")
            .get("row_version")
    }

    async fn booking_count(&self) -> i64 {
        sqlx::query("SELECT COUNT(*) AS n FROM bookings")
            .fetch_one(self.council.pool())
            .await
            .expect("count")
            .get::<i64, _>("n")
    }

    async fn create(&self, id: &str) -> EffectOutcome {
        let grant = self.grant_for("TH-A", "SLOT-A").await;
        self.create_with(id, "TH-A", "SLOT-A", 20, 4_500, grant)
            .await
    }

    async fn create_with(
        &self,
        id: &str,
        venue: &str,
        slot: &str,
        attendees: u16,
        fee: u64,
        grant: String,
    ) -> EffectOutcome {
        self.registry()
            .create_booking(&CreateBooking {
                effect_intent_id: id.to_owned(),
                expires_at_ms: DEADLINE,
                venue_id: venue.to_owned(),
                slot_id: slot.to_owned(),
                attendees,
                asserted_fee_pence: fee,
                principal: "lucy".to_owned(),
                grant,
            })
            .await
            .expect("create")
    }

    async fn resolve(&self, id: &str, kind: OperationKind) -> EffectOutcome {
        self.registry()
            .resolve(&ResolveEffect {
                effect_intent_id: id.to_owned(),
                expires_at_ms: DEADLINE,
                kind,
            })
            .await
            .expect("resolve")
    }

    async fn cancel(&self, id: &str, reference: &str) -> EffectOutcome {
        self.registry()
            .apply_cancellation(&ApplyCancellation {
                effect_intent_id: id.to_owned(),
                expires_at_ms: DEADLINE,
                booking_reference: reference.to_owned(),
            })
            .await
            .expect("cancel")
    }
}

fn reference(outcome: &EffectOutcome) -> &str {
    match outcome {
        EffectOutcome::BookingCreated(facts) => &facts.booking_reference,
        EffectOutcome::CancellationApplied { booking_reference } => booking_reference,
        other => panic!("expected a created effect, got {other:?}"),
    }
}

// ------------------------------------------------ the registry's state machine

/// One identity, one booking, however many times it is asked.
#[tokio::test]
async fn a_retried_create_returns_the_original() {
    let h = Harness::new().await;

    let first = h.create("EFF-1").await;
    let again = h.create("EFF-1").await;

    assert_eq!(first, again, "a retry returns the original answer");
    assert_eq!(h.booking_count().await, 1, "and creates nothing new");
    assert_eq!(h.state_of("EFF-1").await.as_deref(), Some("Created"));
}

/// A create for an identity bound as a cancellation is our bug, not a provider
/// fact. It must not be `ProviderRejected`, which the coordinator acts on
/// irreversibly.
#[tokio::test]
async fn a_create_for_a_cancel_identity_is_a_protocol_conflict() {
    let h = Harness::new().await;
    let created = h.create("EFF-BOOK").await;
    let booking = reference(&created).to_owned();

    h.cancel("EFF-CANCEL", &booking).await;
    let confused = h.create("EFF-CANCEL").await;

    assert!(
        matches!(confused, EffectOutcome::ProtocolConflict { .. }),
        "expected a protocol conflict, got {confused:?}"
    );
}

/// A caller who could shorten a deadline could force premature absence and cancel
/// a booking about to succeed. So the binding is immutable — and nothing about the
/// row moves when one is refused.
#[tokio::test]
async fn a_different_deadline_conflicts_and_changes_nothing() {
    let h = Harness::new().await;
    h.create("EFF-1").await;

    let conflicted = h
        .registry()
        .resolve(&ResolveEffect {
            effect_intent_id: "EFF-1".to_owned(),
            expires_at_ms: DEADLINE - 5_000,
            kind: OperationKind::Book,
        })
        .await
        .expect("resolve");

    assert!(matches!(conflicted, EffectOutcome::ProtocolConflict { .. }));
    assert_eq!(
        h.state_of("EFF-1").await.as_deref(),
        Some("Created"),
        "the stored state is untouched"
    );
    assert_eq!(h.booking_count().await, 1);
}

/// A resolve before the deadline binds the row and says "not yet" — an answer the
/// three-state design could not represent, which is why `Open` exists. Without it
/// the council either lies that the effect is settled or never binds the deadline.
#[tokio::test]
async fn a_resolve_before_expiry_binds_the_row_and_answers_not_yet() {
    let h = Harness::new().await;

    let answer = h.resolve("EFF-UNSEEN", OperationKind::Book).await;

    assert_eq!(answer, EffectOutcome::NotYetVisible);
    assert_eq!(h.state_of("EFF-UNSEEN").await.as_deref(), Some("Open"));

    let bound: i64 = sqlx::query("SELECT expires_at_ms FROM effects WHERE effect_intent_id = ?")
        .bind("EFF-UNSEEN")
        .fetch_one(h.council.pool())
        .await
        .expect("read")
        .get("expires_at_ms");
    assert_eq!(bound, DEADLINE, "the deadline is bound on first sight");
}

/// Past the deadline, absence becomes sayable — and it is written down. The row is
/// the assertion, not the clock reading that produced it.
#[tokio::test]
async fn a_resolve_after_expiry_tombstones_the_identity() {
    let h = Harness::new().await;
    h.clock.set(DEADLINE + 1);

    let answer = h.resolve("EFF-GONE", OperationKind::Book).await;

    assert_eq!(answer, EffectOutcome::DefinitivelyAbsent);
    assert_eq!(h.state_of("EFF-GONE").await.as_deref(), Some("Absent"));
}

/// The tombstone, not the clock, is what makes absence permanent. Wind the clock
/// back to before the deadline and the create is still refused.
#[tokio::test]
async fn a_tombstoned_identity_stays_refused_after_the_clock_winds_back() {
    let h = Harness::new().await;
    h.clock.set(DEADLINE + 1);
    h.resolve("EFF-GONE", OperationKind::Book).await;

    h.clock.set(NOW);
    let attempted = h.create("EFF-GONE").await;

    assert_eq!(attempted, EffectOutcome::DefinitivelyAbsent);
    assert_eq!(h.booking_count().await, 0, "and nothing was created");
}

/// A create whose deadline passes *while it waits* is refused, and tombstoned.
///
/// The pause sits inside the write transaction, before the clock is read, so a
/// council that judged expiry on arrival cannot pass: its check already succeeded.
#[tokio::test]
async fn a_create_that_reaches_the_write_late_is_refused() {
    let clock = Arc::new(TestClock::at(NOW));
    let mover = MovesTheClockOnce {
        clock: Arc::clone(&clock),
        to_ms: DEADLINE + 1,
        at: PausePoint::BeforeExpiryWrite,
        fired: AtomicUsize::new(0),
    };

    let dir = TempDir::new().expect("a temp dir");
    let council = Council::build(
        dir.path().join("council.sqlite"),
        Arc::new(CouncilSigner::new(SigningKey::from_bytes(&[7u8; 32]))),
        Arc::clone(&clock) as Arc<_>,
        Arc::new(mover),
        TTL,
    )
    .await
    .expect("open");
    council.seed(SLOTS).await.expect("seed");

    let grant = council
        .registry()
        .availability("TH-A", "SLOT-A")
        .await
        .expect("availability")
        .expect("slot")
        .grant;

    let outcome = council
        .registry()
        .create_booking(&CreateBooking {
            effect_intent_id: "EFF-LATE".to_owned(),
            expires_at_ms: DEADLINE,
            venue_id: "TH-A".to_owned(),
            slot_id: "SLOT-A".to_owned(),
            attendees: 20,
            asserted_fee_pence: 4_500,
            principal: "lucy".to_owned(),
            grant,
        })
        .await
        .expect("create");

    assert_eq!(outcome, EffectOutcome::DefinitivelyAbsent);

    let state: Option<String> = sqlx::query("SELECT state FROM effects WHERE effect_intent_id = ?")
        .bind("EFF-LATE")
        .fetch_optional(council.pool())
        .await
        .expect("read")
        .map(|row| row.get("state"));
    assert_eq!(
        state.as_deref(),
        Some("Absent"),
        "asserted from the database, not inferred from the response"
    );

    let bookings: i64 = sqlx::query("SELECT COUNT(*) AS n FROM bookings")
        .fetch_one(council.pool())
        .await
        .expect("count")
        .get("n");
    assert_eq!(bookings, 0);
}

/// A booking committed before its deadline stays returnable long after it, or a
/// retry past the deadline would see nothing and book the room twice.
#[tokio::test]
async fn a_created_effect_stays_discoverable_past_its_deadline() {
    let h = Harness::new().await;
    let created = h.create("EFF-1").await;

    h.clock.set(DEADLINE + 1_000_000);

    assert_eq!(
        h.resolve("EFF-1", OperationKind::Book).await,
        created,
        "the same canonical facts, long past the deadline"
    );
    assert_eq!(h.create("EFF-1").await, created, "and a retry, too");
    assert_eq!(h.booking_count().await, 1);
}

/// A `Created` row must carry the *complete* facts, not just a reference —
/// otherwise the verifier would have to take venue, fee and headcount from the
/// caller's own context, and the domain's binding would compare the plan against
/// itself.
#[tokio::test]
async fn a_resolved_creation_carries_the_complete_facts() {
    let h = Harness::new().await;
    h.create("EFF-1").await;
    h.clock.set(DEADLINE + 1);

    let EffectOutcome::BookingCreated(facts) = h.resolve("EFF-1", OperationKind::Book).await else {
        panic!("expected a created booking");
    };

    assert_eq!(facts.venue_id, "TH-A");
    assert_eq!(facts.slot_id, "SLOT-A");
    assert_eq!(facts.attendees, 20);
    assert_eq!(facts.fee_pence, 4_500);
    assert_eq!(facts.principal, "lucy");
}

// -------------------------------------------- the council's facts are its own

/// The gate for the defect where a signature over an echo passes for provenance.
///
/// Assert a fee the catalogue disagrees with and the council refuses to book at
/// it. Terminal, because this identity's plan rests on a price that was never
/// real.
#[tokio::test]
async fn a_create_asserting_the_wrong_fee_is_rejected() {
    let h = Harness::new().await;
    let grant = h.grant_for("TH-A", "SLOT-A").await;

    let outcome = h
        .create_with("EFF-CHEAP", "TH-A", "SLOT-A", 20, 1, grant)
        .await;

    match outcome {
        EffectOutcome::ProviderRejected { reason } => {
            assert!(
                reason.contains("4500"),
                "the reason names the real fee: {reason}"
            );
        }
        other => panic!("expected a rejection, got {other:?}"),
    }
    assert_eq!(h.booking_count().await, 0);
    assert_eq!(h.state_of("EFF-CHEAP").await.as_deref(), Some("Rejected"));
}

/// The fee that lands in the booking is the catalogue's, and this is the test that
/// can tell the difference: change the catalogue *after* the grant was issued and
/// the create is refused on the row version — so the only way to book is to assert
/// the current fee, and the only fee the response can carry is the one the
/// catalogue holds.
#[tokio::test]
async fn the_stored_fee_follows_the_catalogue() {
    let h = Harness::new().await;
    h.council
        .seed(&[SeedSlot {
            venue_id: "TH-A",
            slot_id: "SLOT-A",
            fee_pence: 7_000,
            capacity: 30,
            accessible: true,
            available: true,
        }])
        .await
        .expect("reseed");

    let grant = h.grant_for("TH-A", "SLOT-A").await;
    let outcome = h
        .create_with("EFF-DEARER", "TH-A", "SLOT-A", 20, 7_000, grant)
        .await;

    let EffectOutcome::BookingCreated(facts) = outcome else {
        panic!("expected a created booking");
    };
    assert_eq!(facts.fee_pence, 7_000);
}

/// `available` was missing from the catalogue, the wire and the encoding in an
/// earlier draft, so a client would have had to invent it — and `true` is the
/// obvious guess. The council refuses regardless of what we believed.
#[tokio::test]
async fn a_create_for_an_unavailable_slot_is_rejected() {
    let h = Harness::new().await;
    let grant = h.grant_for("TH-SHUT", "SLOT-A").await;

    let outcome = h
        .create_with("EFF-SHUT", "TH-SHUT", "SLOT-A", 20, 4_500, grant)
        .await;

    match outcome {
        EffectOutcome::ProviderRejected { reason } => {
            assert!(reason.contains("not available"), "reason: {reason}");
        }
        other => panic!("expected a rejection, got {other:?}"),
    }
    assert_eq!(h.booking_count().await, 0);
}

#[tokio::test]
async fn a_create_for_an_unknown_slot_is_rejected() {
    let h = Harness::new().await;
    let grant = h.grant_for("TH-A", "SLOT-A").await;

    let outcome = h
        .create_with("EFF-NOWHERE", "TH-NOPE", "SLOT-A", 20, 4_500, grant)
        .await;

    assert!(
        matches!(outcome, EffectOutcome::ProviderRejected { .. }),
        "got {outcome:?}"
    );
    assert_eq!(h.booking_count().await, 0);
}

#[tokio::test]
async fn a_create_over_capacity_is_rejected() {
    let h = Harness::new().await;
    let grant = h.grant_for("TH-A", "SLOT-A").await;

    let outcome = h
        .create_with("EFF-CROWD", "TH-A", "SLOT-A", 31, 4_500, grant)
        .await;

    match outcome {
        EffectOutcome::ProviderRejected { reason } => {
            assert!(reason.contains("holds 30"), "reason: {reason}");
        }
        other => panic!("expected a rejection, got {other:?}"),
    }
}

// ------------------------------------------------------ the availability grant

/// Lucy's scenario. Every field the create carries still matches; only the room's
/// accessibility changed, and the row version is what notices.
#[tokio::test]
async fn a_grant_is_stale_once_only_accessibility_changed() {
    let h = Harness::new().await;
    let grant = h.grant_for("TH-A", "SLOT-A").await;

    // The lift breaks. Fee, capacity and availability are untouched, so every
    // condition a booking checks would still pass.
    h.council
        .seed(&[SeedSlot {
            venue_id: "TH-A",
            slot_id: "SLOT-A",
            fee_pence: 4_500,
            capacity: 30,
            accessible: false,
            available: true,
        }])
        .await
        .expect("reseed");

    let outcome = h
        .create_with("EFF-LIFT", "TH-A", "SLOT-A", 20, 4_500, grant)
        .await;

    match outcome {
        EffectOutcome::ProviderRejected { reason } => {
            assert!(reason.contains("version"), "reason: {reason}");
        }
        other => panic!("expected a stale-grant rejection, got {other:?}"),
    }
    assert_eq!(h.booking_count().await, 0);
}

/// The failure a client-side freshness check cannot catch: our clock runs *behind*
/// the council's, so we would happily present an observation the council considers
/// dead. It refuses on its own clock, with the catalogue row untouched.
#[tokio::test]
async fn a_grant_past_its_window_is_refused_with_the_row_unchanged() {
    let h = Harness::new().await;
    let grant = h.grant_for("TH-A", "SLOT-A").await;

    // The council's clock moves past the grant's window. The row does not change,
    // so its version still matches — only the window has passed.
    h.clock.set(NOW + TTL + 1);

    let outcome = h
        .registry()
        .create_booking(&CreateBooking {
            effect_intent_id: "EFF-STALE".to_owned(),
            // A deadline still in the future, so this is the grant's window
            // failing and not the effect's.
            expires_at_ms: NOW + TTL + 60_000,
            venue_id: "TH-A".to_owned(),
            slot_id: "SLOT-A".to_owned(),
            attendees: 20,
            asserted_fee_pence: 4_500,
            principal: "lucy".to_owned(),
            grant,
        })
        .await
        .expect("create");

    match outcome {
        EffectOutcome::ProviderRejected { reason } => {
            assert!(reason.contains("expired"), "reason: {reason}");
        }
        other => panic!("expected an expired-grant rejection, got {other:?}"),
    }
    assert_eq!(h.booking_count().await, 0);
}

/// A warrant for a cheap accessible room must not vouch for the booking of a
/// different one.
#[tokio::test]
async fn a_grant_for_another_slot_is_refused() {
    let h = Harness::new().await;
    let elsewhere = h.grant_for("TH-SHUT", "SLOT-A").await;

    let outcome = h
        .create_with("EFF-SWAP", "TH-A", "SLOT-A", 20, 4_500, elsewhere)
        .await;

    match outcome {
        EffectOutcome::ProviderRejected { reason } => {
            assert!(reason.contains("TH-SHUT"), "reason: {reason}");
        }
        other => panic!("expected a wrong-slot rejection, got {other:?}"),
    }
}

/// A grant nobody minted, and one minted by the wrong key.
#[tokio::test]
async fn a_forged_grant_is_refused() {
    let h = Harness::new().await;

    let nonsense = h
        .create_with(
            "EFF-JUNK",
            "TH-A",
            "SLOT-A",
            20,
            4_500,
            "not-a-grant".to_owned(),
        )
        .await;
    assert!(
        matches!(nonsense, EffectOutcome::ProviderRejected { .. }),
        "got {nonsense:?}"
    );

    let impostor = CouncilSigner::new(SigningKey::from_bytes(&[9u8; 32]));
    let forged = impostor
        .mint_grant(&GrantClaims {
            venue_id: "TH-A".to_owned(),
            slot_id: "SLOT-A".to_owned(),
            row_version: 1,
            valid_until_ms: NOW + TTL,
        })
        .expect("mint");

    let signed_by_another = h
        .create_with("EFF-FORGED", "TH-A", "SLOT-A", 20, 4_500, forged)
        .await;
    assert!(
        matches!(signed_by_another, EffectOutcome::ProviderRejected { .. }),
        "got {signed_by_another:?}"
    );
    assert_eq!(h.booking_count().await, 0);
}

// ------------------------------------------------------------- cancellation

#[tokio::test]
async fn a_cancellation_has_its_own_identity_and_is_idempotent() {
    let h = Harness::new().await;
    let booking = reference(&h.create("EFF-BOOK").await).to_owned();

    let first = h.cancel("EFF-CANCEL", &booking).await;
    let again = h.cancel("EFF-CANCEL", &booking).await;

    assert_eq!(
        first,
        EffectOutcome::CancellationApplied {
            booking_reference: booking.clone()
        }
    );
    assert_eq!(first, again);
    assert_eq!(
        h.resolve("EFF-CANCEL", OperationKind::Cancel).await,
        first,
        "and resolving the cancellation identity gives the same answer"
    );
}

/// The booking identity and the cancellation identity are different effects, and
/// resolving one must not answer for the other.
#[tokio::test]
async fn the_two_identities_answer_separately() {
    let h = Harness::new().await;
    let created = h.create("EFF-BOOK").await;
    let booking = reference(&created).to_owned();
    h.cancel("EFF-CANCEL", &booking).await;

    assert_eq!(h.resolve("EFF-BOOK", OperationKind::Book).await, created);
    assert_eq!(
        h.resolve("EFF-CANCEL", OperationKind::Cancel).await,
        EffectOutcome::CancellationApplied {
            booking_reference: booking
        }
    );
}

/// Cancelling something that was never booked can never succeed, so it is terminal
/// rather than an error the caller retries forever.
#[tokio::test]
async fn cancelling_a_booking_that_does_not_exist_is_rejected_durably() {
    let h = Harness::new().await;

    let refused = h.cancel("EFF-GHOST", "TH-99999").await;
    match &refused {
        EffectOutcome::ProviderRejected { reason } => {
            assert!(reason.contains("TH-99999"), "reason: {reason}");
        }
        other => panic!("expected a rejection, got {other:?}"),
    }

    assert_eq!(
        h.resolve("EFF-GHOST", OperationKind::Cancel).await,
        refused,
        "and a lost response resolves to the same rejection, never to absence"
    );
}

/// `CancellationApplied` for a second identity would be a lie: `cancelled_by`
/// names the first one, and this identity did nothing.
#[tokio::test]
async fn cancelling_under_a_second_identity_is_rejected() {
    let h = Harness::new().await;
    let booking = reference(&h.create("EFF-BOOK").await).to_owned();
    h.cancel("EFF-CANCEL-1", &booking).await;

    let second = h.cancel("EFF-CANCEL-2", &booking).await;

    match second {
        EffectOutcome::ProviderRejected { reason } => {
            assert!(reason.contains("EFF-CANCEL-1"), "reason: {reason}");
        }
        other => panic!("expected a rejection, got {other:?}"),
    }
}

/// A cancellation refused for its own expiry tombstones *its* identity and leaves
/// the booking's row alone.
#[tokio::test]
async fn an_expired_cancellation_tombstones_only_itself() {
    let h = Harness::new().await;
    let booking = reference(&h.create("EFF-BOOK").await).to_owned();

    h.clock.set(DEADLINE + 1);
    let refused = h.cancel("EFF-CANCEL", &booking).await;

    assert_eq!(refused, EffectOutcome::DefinitivelyAbsent);
    assert_eq!(h.state_of("EFF-CANCEL").await.as_deref(), Some("Absent"));
    assert_eq!(
        h.state_of("EFF-BOOK").await.as_deref(),
        Some("Created"),
        "the booking's own row is untouched"
    );
}

// ----------------------------------------------- commit before the response

/// The half a test reading *after* the response can never prove: killed before
/// the commit, nothing is discoverable.
///
/// Simulated by failing the transaction at `BeforeSettleCommit` — the same
/// observation a `SIGKILL` there would produce, without needing a subprocess. The
/// subprocess harness exists for slice E's crash matrix; this is the ordering
/// property, and it is D's to prove.
#[tokio::test]
async fn nothing_is_discoverable_before_the_settlement_commits() {
    let observer = Arc::new(RecordsPauses::default());
    let h = Harness::with_pauses(Arc::clone(&observer) as Arc<_>).await;
    h.clock.set(DEADLINE + 1);

    // Read the registry from a *separate* connection while the writer is paused
    // mid-transaction. It must see nothing.
    let pool = h.council.pool().clone();
    observer.on_reach(PausePoint::BeforeSettleCommit, {
        let pool = pool.clone();
        Arc::new(move |id: String| {
            let pool = pool.clone();
            Box::pin(async move {
                let seen: Option<String> =
                    sqlx::query("SELECT state FROM effects WHERE effect_intent_id = ?")
                        .bind(&id)
                        .fetch_optional(&pool)
                        .await
                        .expect("read")
                        .map(|row| row.get("state"));
                assert_eq!(
                    seen, None,
                    "the tombstone was visible before its transaction committed"
                );
            })
        })
    });

    let answer = h.resolve("EFF-GONE", OperationKind::Book).await;

    assert_eq!(answer, EffectOutcome::DefinitivelyAbsent);
    assert_eq!(
        h.state_of("EFF-GONE").await.as_deref(),
        Some("Absent"),
        "and it is durable once the transaction commits"
    );
    assert_eq!(observer.count(PausePoint::BeforeSettleCommit), 1);
    assert_eq!(observer.count(PausePoint::AfterSettleCommit), 1);
}

/// The other half: after the commit and before the answer is written, the answer
/// is already durable — so a caller who never saw it gets the same one on asking
/// again.
#[tokio::test]
async fn the_answer_is_durable_before_it_is_observable() {
    let observer = Arc::new(RecordsPauses::default());
    let h = Harness::with_pauses(Arc::clone(&observer) as Arc<_>).await;
    h.clock.set(DEADLINE + 1);

    let pool = h.council.pool().clone();
    observer.on_reach(PausePoint::AfterSettleCommit, {
        let pool = pool.clone();
        Arc::new(move |id: String| {
            let pool = pool.clone();
            Box::pin(async move {
                let seen: Option<String> =
                    sqlx::query("SELECT state FROM effects WHERE effect_intent_id = ?")
                        .bind(&id)
                        .fetch_optional(&pool)
                        .await
                        .expect("read")
                        .map(|row| row.get("state"));
                assert_eq!(
                    seen.as_deref(),
                    Some("Absent"),
                    "the answer was not durable before it became observable"
                );
            })
        })
    });

    h.resolve("EFF-GONE", OperationKind::Book).await;
    assert_eq!(observer.count(PausePoint::AfterSettleCommit), 1);
}

/// The pause points fire on the create path too, so slice E can crash a booking
/// either side of its commit.
#[tokio::test]
async fn a_create_pauses_at_every_point() {
    let observer = Arc::new(RecordsPauses::default());
    let h = Harness::with_pauses(Arc::clone(&observer) as Arc<_>).await;

    h.create("EFF-1").await;

    assert_eq!(observer.count(PausePoint::BeforeExpiryWrite), 1);
    assert_eq!(observer.count(PausePoint::BeforeSettleCommit), 1);
    assert_eq!(observer.count(PausePoint::AfterSettleCommit), 1);
}

// ------------------------------------------- the version cannot be stepped around

/// The row version can be advanced but never held still or wound back.
///
/// A grant binds to a version, so what matters is that a version it once named
/// can never be current again. PR review found the original trigger fired only
/// `WHEN NEW.row_version = OLD.row_version`, so an update that *named* a version
/// slipped past it — and a grant for version 1, after a write that set the row
/// back to 1, matched a row that had since changed.
#[tokio::test]
async fn a_write_cannot_hold_or_rewind_the_row_version() {
    let h = Harness::new().await;

    let start = h.row_version("TH-A", "SLOT-A").await;

    // An ordinary update, not naming the version at all.
    sqlx::query("UPDATE venue_slots SET accessible = 0 WHERE venue_id = ? AND slot_id = ?")
        .bind("TH-A")
        .bind("SLOT-A")
        .execute(h.council.pool())
        .await
        .expect("update");
    let bumped = h.row_version("TH-A", "SLOT-A").await;
    assert!(bumped > start, "an ordinary update advances it");

    // An update that tries to set the version back to where it was.
    sqlx::query(
        "UPDATE venue_slots SET accessible = 1, row_version = ? WHERE venue_id = ? AND slot_id = ?",
    )
    .bind(start)
    .bind("TH-A")
    .bind("SLOT-A")
    .execute(h.council.pool())
    .await
    .expect("update");
    assert!(
        h.row_version("TH-A", "SLOT-A").await > bumped,
        "naming an older version does not get you one"
    );
}

/// And the grant that follows from it: a stale version cannot be made current
/// again by a write that names it.
#[tokio::test]
async fn a_grant_cannot_be_revived_by_rewinding_the_version() {
    let h = Harness::new().await;
    let grant = h.grant_for("TH-A", "SLOT-A").await;
    let issued_at = h.row_version("TH-A", "SLOT-A").await;

    // The lift breaks, then someone tries to put the version back.
    sqlx::query("UPDATE venue_slots SET accessible = 0 WHERE venue_id = ? AND slot_id = ?")
        .bind("TH-A")
        .bind("SLOT-A")
        .execute(h.council.pool())
        .await
        .expect("update");
    sqlx::query("UPDATE venue_slots SET row_version = ? WHERE venue_id = ? AND slot_id = ?")
        .bind(issued_at)
        .bind("TH-A")
        .bind("SLOT-A")
        .execute(h.council.pool())
        .await
        .expect("update");

    let outcome = h
        .create_with("EFF-REVIVED", "TH-A", "SLOT-A", 20, 4_500, grant)
        .await;
    assert!(
        matches!(outcome, EffectOutcome::ProviderRejected { .. }),
        "the stale grant must stay stale: {outcome:?}"
    );
    assert_eq!(h.booking_count().await, 0);
}

// ------------------------------------- a corrupt catalogue is no answer, not a lie

/// A stored value that cannot be represented is refused, not clamped.
///
/// The direction is the reason. Clamping a negative fee reports a free room and
/// clamping a negative capacity reports one holding 65535 people — both
/// *permissive*, both passing the guard they should fail. SQL would refuse the
/// eventual booking, but a false fact would already have crossed the boundary and
/// been committed as verified.
#[tokio::test]
async fn a_negative_stored_fee_is_refused_rather_than_reported_as_free() {
    let h = Harness::new().await;
    sqlx::query("UPDATE venue_slots SET fee_pence = -1 WHERE venue_id = ? AND slot_id = ?")
        .bind("TH-A")
        .bind("SLOT-A")
        .execute(h.council.pool())
        .await
        .expect("update");

    let answer = h.registry().availability("TH-A", "SLOT-A").await;
    assert!(
        answer.is_err(),
        "a corrupt catalogue must yield no answer, not a free room"
    );
}

#[tokio::test]
async fn a_negative_stored_capacity_is_refused_rather_than_reported_as_huge() {
    let h = Harness::new().await;
    sqlx::query("UPDATE venue_slots SET capacity = -1 WHERE venue_id = ? AND slot_id = ?")
        .bind("TH-A")
        .bind("SLOT-A")
        .execute(h.council.pool())
        .await
        .expect("update");

    assert!(
        h.registry().availability("TH-A", "SLOT-A").await.is_err(),
        "a corrupt catalogue must yield no answer"
    );
}

// ------------------------------------------------------------------ one clock

/// The council must read one clock, and it must be the injectable one.
///
/// An earlier design put `unixepoch()` in the write predicate, which reads
/// `SQLite`'s host clock through its VFS — a second clock, and the one a test cannot
/// move. This greps the SQL rather than trusting the intention.
#[test]
fn the_council_reads_no_clock_inside_sql() {
    let sources = [
        include_str!("../src/registry.rs"),
        include_str!("../src/lib.rs"),
        include_str!("../migrations/0001_council.sql"),
    ];

    for forbidden in [
        "unixepoch",
        "CURRENT_TIMESTAMP",
        "current_timestamp",
        "datetime(",
        "julianday(",
        "strftime(",
        "DEFAULT (date",
    ] {
        for source in sources {
            // The clock module's own prose names these to explain why they are
            // absent, so only the SQL-bearing sources are checked.
            assert!(
                !source.contains(forbidden),
                "{forbidden:?} appears in the council's SQL sources"
            );
        }
    }
}

// -------------------------------------------------------------------- doubles

type PauseAction =
    Arc<dyn Fn(String) -> std::pin::Pin<Box<dyn Future<Output = ()> + Send>> + Send + Sync>;

/// Counts every pause, and optionally runs something at one of them.
#[derive(Default)]
struct RecordsPauses {
    before_expiry: AtomicUsize,
    before_commit: AtomicUsize,
    after_commit: AtomicUsize,
    actions: std::sync::Mutex<Vec<(PausePoint, PauseAction)>>,
}

impl std::fmt::Debug for RecordsPauses {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str("RecordsPauses")
    }
}

impl RecordsPauses {
    fn on_reach(&self, point: PausePoint, action: PauseAction) {
        self.actions
            .lock()
            .expect("not poisoned")
            .push((point, action));
    }

    fn count(&self, point: PausePoint) -> usize {
        match point {
            PausePoint::BeforeExpiryWrite => self.before_expiry.load(Ordering::SeqCst),
            PausePoint::BeforeSettleCommit => self.before_commit.load(Ordering::SeqCst),
            PausePoint::AfterSettleCommit => self.after_commit.load(Ordering::SeqCst),
        }
    }
}

#[async_trait::async_trait]
impl Pauses for RecordsPauses {
    async fn reach(&self, point: PausePoint, effect_intent_id: &str) {
        match point {
            PausePoint::BeforeExpiryWrite => &self.before_expiry,
            PausePoint::BeforeSettleCommit => &self.before_commit,
            PausePoint::AfterSettleCommit => &self.after_commit,
        }
        .fetch_add(1, Ordering::SeqCst);

        let actions: Vec<PauseAction> = self
            .actions
            .lock()
            .expect("not poisoned")
            .iter()
            .filter(|(at, _)| *at == point)
            .map(|(_, action)| Arc::clone(action))
            .collect();

        for action in actions {
            action(effect_intent_id.to_owned()).await;
        }
    }
}

/// Moves the clock the first time it reaches a point, and only then.
struct MovesTheClockOnce {
    clock: Arc<TestClock>,
    to_ms: i64,
    at: PausePoint,
    fired: AtomicUsize,
}

impl std::fmt::Debug for MovesTheClockOnce {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str("MovesTheClockOnce")
    }
}

#[async_trait::async_trait]
impl Pauses for MovesTheClockOnce {
    async fn reach(&self, point: PausePoint, _effect_intent_id: &str) {
        if point == self.at && self.fired.fetch_add(1, Ordering::SeqCst) == 0 {
            self.clock.set(self.to_ms);
        }
    }
}
