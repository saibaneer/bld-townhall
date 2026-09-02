//! A10–A19: the gateway against the real server and council.
//!
//! Nothing here is mocked. A mock would prove the gateway agrees with my idea of
//! the wire, which is the belief actually under test.

mod harness;

use bld_types::{BookingId, BookingRequirements, CouncilBookingRef, Money, TimeWindow};
use harness::{LUCY, MARCO, PRIYA, World, arm_fault, council_count, fault_fired, world};
use std::time::Duration;
use townhall_gateway::{Gateway, GatewayError, RetryPolicy, Turn};

fn requirements() -> BookingRequirements {
    BookingRequirements {
        purpose: "community meeting".to_owned(),
        requested_date: "2026-09-10".to_owned(),
        time_window: TimeWindow {
            from: "14:00".to_owned(),
            to: "17:00".to_owned(),
        },
        attendees: 20,
        wheelchair_accessible: true,
        max_fee: Money::from_pence(5_000),
    }
}

fn gateway(world: &World, bearer: &str) -> Gateway {
    Gateway::new(world.server_url.clone(), bearer)
}

/// One fixed inbound message identity — the value a redelivery repeats.
fn townhall_channel_identity() -> townhall_channel::InboundIdentity {
    townhall_channel::InboundIdentity::new("sim", "acct", "msg-create-1")
}

/// Drive a booking to `AwaitingBooking`, returning its current version.
async fn awaiting(gw: &Gateway, id: &BookingId) -> u64 {
    let created = gw.create(id, &requirements()).await.expect("create");
    let mut version = created.version;
    for (behaviour, body) in [
        (
            "select-venue",
            Some(serde_json::json!({"venue_id": "TH-A", "slot_id": "SLOT-A"})),
        ),
        ("verify-slot", None),
    ] {
        let turn = gw
            .propose_at(id, version, behaviour, body)
            .await
            .expect("turn");
        let Turn::Committed { version: next, .. } = turn else {
            panic!("{behaviour} did not commit: {turn:?}");
        };
        version = next;
    }
    version
}

// ------------------------------------------------------------------ A10

/// Every field of the projection survives the round trip.
///
/// The DTOs here are written independently of the server's, so a field renamed
/// on one side only fails at this line rather than silently deserializing to a
/// default somewhere downstream.
#[tokio::test]
async fn a10_create_round_trips_every_field() {
    let world = world();
    let gw = gateway(&world, LUCY);
    let id = BookingId::new("BKG-A10");

    let created = gw.create(&id, &requirements()).await.expect("create");
    assert_eq!(created.id, "BKG-A10");
    assert_eq!(created.version, 0);
    assert_eq!(created.state, "Draft");
    assert_eq!(created.requirements.purpose, "community meeting");
    assert_eq!(created.requirements.requested_date, "2026-09-10");
    assert_eq!(created.requirements.from, "14:00");
    assert_eq!(created.requirements.to, "17:00");
    assert_eq!(created.requirements.attendees, 20);
    assert!(created.requirements.wheelchair_accessible);
    assert_eq!(created.requirements.max_fee_pence, 5_000);
    assert_eq!(created.selected_venue, None);
    assert_eq!(created.booking_ref, None);
    assert_eq!(created.available_behaviours, vec!["SelectVenue", "Cancel"]);

    // And reading it back agrees with what create said.
    assert_eq!(gw.read(&id).await.expect("read"), created);
}

// ------------------------------------------------------------------ A11 / A12

/// The two 409 shapes are two different answers.
///
/// The owner learns the version so a retry can carry an `If-Match`; a stranger
/// learns only that the identifier is taken. A gateway collapsing both into one
/// variant leaves a caller unable to tell "retry with this `ETag`" from "choose
/// another id".
#[tokio::test]
async fn a11_a12_duplicate_create_distinguishes_owner_from_stranger() {
    let world = world();
    let lucy = gateway(&world, LUCY);
    let priya = gateway(&world, PRIYA);

    // The DERIVED id — the redelivery protection this test is actually about.
    // A hand-picked "BKG-A12" tested nothing about the seam M6B will stand on:
    // that the same message names the same booking across restarts.
    let message = townhall_channel_identity();
    let id = message.booking_id();
    assert_eq!(id, townhall_channel_identity().booking_id());

    lucy.create(&id, &requirements()).await.expect("create");

    match lucy.create(&id, &requirements()).await {
        Err(GatewayError::Existing { current }) => assert_eq!(current, 0),
        other => panic!("the owner must learn the version: {other:?}"),
    }
    match priya.create(&id, &requirements()).await {
        Err(GatewayError::IdentifierUnavailable) => {}
        other => panic!("a stranger must learn nothing else: {other:?}"),
    }

    // Exactly one booking exists, whatever the callers were told.
    assert_eq!(lucy.read(&id).await.expect("read").version, 0);
}

// ------------------------------------------------------------------ A13

/// A proposal carries the version the caller just read.
#[tokio::test]
async fn a13_propose_sends_the_version_it_was_given() {
    let world = world();
    let gw = gateway(&world, LUCY);
    let id = BookingId::new("BKG-A13");
    let version = awaiting(&gw, &id).await;

    // A stale tag is refused with the current version, so the header was
    // genuinely sent and genuinely compared.
    match gw.propose_at(&id, version - 1, "book", None).await {
        Err(GatewayError::Stale { current }) => assert_eq!(current, version),
        other => panic!("a stale precondition must be refused: {other:?}"),
    }
    // And the fresh one commits.
    assert!(matches!(
        gw.propose_at(&id, version, "book", None)
            .await
            .expect("turn"),
        Turn::Committed { .. }
    ));
}

// ------------------------------------------------------------------ A14

/// The statuses that need a distinguisher, each reached on purpose.
#[tokio::test]
#[allow(clippy::too_many_lines)] // one sweep, deliberately: the table IS the test
async fn a14_the_status_contract_is_keyed_on_more_than_the_number() {
    let world = world();
    let lucy = gateway(&world, LUCY);
    let priya = gateway(&world, PRIYA);

    // 404 for absent AND for invisible — deliberately indistinguishable.
    let hers = BookingId::new("BKG-A14");
    lucy.create(&hers, &requirements()).await.expect("create");
    assert!(matches!(
        priya.read(&hers).await,
        Err(GatewayError::UnknownBooking)
    ));
    assert!(matches!(
        priya.read(&BookingId::new("BKG-NOBODY")).await,
        Err(GatewayError::UnknownBooking)
    ));

    // 409 Undefined: a behaviour absent from this state's menu, carrying the
    // menu that IS available.
    let turn = lucy
        .propose_at(&hers, 0, "book", None)
        .await
        .expect("a turn, not an error");
    let Turn::NotAvailable { menu } = turn else {
        panic!("Book from Draft is Undefined: {turn:?}");
    };
    assert_eq!(menu, vec!["SelectVenue", "Cancel"]);

    // 403: a capability refusal, as a Denied turn with the domain's own name.
    let priyas = BookingId::new("BKG-A14-PRIYA");
    let version = awaiting(&priya, &priyas).await;
    let turn = priya
        .propose_at(&priyas, version, "book", None)
        .await
        .expect("a turn");
    assert_eq!(
        turn,
        Turn::Denied {
            reason: "BookingAuthorityRequired".to_owned()
        },
        "an authority denial is a turn outcome, not a transport error"
    );

    // 403 again, a DIFFERENT guard: Marco's fee ceiling. Same status, same
    // shape — only the domain's error name separates them, which is why the
    // gateway carries the name rather than the number.
    let marco = gateway(&world, MARCO);
    let his = BookingId::new("BKG-A14-MARCO");
    let created = marco.create(&his, &requirements()).await.expect("create");
    let selected = marco
        .propose_at(
            &his,
            created.version,
            "select-venue",
            Some(serde_json::json!({"venue_id": "TH-A", "slot_id": "SLOT-A"})),
        )
        .await
        .expect("turn");
    let Turn::Committed { version, .. } = selected else {
        panic!("select-venue: {selected:?}");
    };
    assert_eq!(
        marco
            .propose_at(&his, version, "verify-slot", None)
            .await
            .expect("turn"),
        Turn::Denied {
            reason: "FeeExceededAuthority".to_owned()
        }
    );

    // 422 malformed: a well-formed request whose BODY the behaviour cannot
    // decode. No ETag, no domain error name — the other 422.
    let bad = lucy
        .propose_at(
            &hers,
            0,
            "select-venue",
            Some(serde_json::json!({"nope": 1})),
        )
        .await;
    assert!(
        matches!(bad, Err(GatewayError::Malformed(_))),
        "a malformed body is not a domain denial: {bad:?}"
    );

    // 422 as a DOMAIN DENIAL — the other shape of the same number, which the
    // first version of this test never exercised at all: a data guard (the
    // capacity check) refusing with the domain's own name, ETag attached.
    let crowded = BookingId::new("BKG-A14-CROWDED");
    let mut wants_too_many = requirements();
    wants_too_many.attendees = 999;
    let created = lucy
        .create(&crowded, &wants_too_many)
        .await
        .expect("create");
    let selected = lucy
        .propose_at(
            &crowded,
            created.version,
            "select-venue",
            Some(serde_json::json!({"venue_id": "TH-A", "slot_id": "SLOT-A"})),
        )
        .await
        .expect("turn");
    let Turn::Committed { version, .. } = selected else {
        panic!("select-venue: {selected:?}");
    };
    assert_eq!(
        lucy.propose_at(&crowded, version, "verify-slot", None)
            .await
            .expect("turn"),
        Turn::Denied {
            reason: "CapacityInsufficient".to_owned()
        },
        "a data guard's 422 is a Denied turn carrying the domain's name"
    );

    // 400 through the gateway's own API is UNREPRESENTABLE — typed lookups
    // cannot send a malformed query, like 428 cannot omit If-Match. The first
    // version asserted `BadRequest(_) | Ok(_)` here, which cannot fail; the
    // classification is unit-tested against constructed responses instead.

    // 401: no usable credential.
    let anonymous = Gateway::new(world.server_url.clone(), "not-a-token");
    assert!(matches!(
        anonymous.read(&hers).await,
        Err(GatewayError::Unauthenticated)
    ));
}

// ------------------------------------------------------------------ A15 / A16

/// 202 needs an armed fault, and convergence is the caller's own second step.
///
/// An unfaulted council answers synchronously, so `book` returns 200 — a test
/// expecting 202 from one would be asserting against a system that does not
/// exist. The fault is what makes the acceptance case reachable on purpose.
#[tokio::test]
async fn a15_a16_acceptance_returns_before_convergence() {
    let world = world();
    let gw = gateway(&world, LUCY);
    let id = BookingId::new("BKG-A15");
    let version = awaiting(&gw, &id).await;

    // The effect identity the council will see for this book.
    let effect = format!("EFF-{}-BOOK-{version}", id.as_str());
    let fault = arm_fault(&world, &effect, "create", "drop_response").await;

    let turn = gw
        .propose_at(&id, version, "book", None)
        .await
        .expect("turn");
    let Turn::Accepted { retry_after } = turn else {
        panic!("a dropped response must surface as Accepted, got {turn:?}");
    };
    assert_eq!(
        fault_fired(&world, fault).await,
        1,
        "the drop genuinely fired — without this the premise of the test is unchecked"
    );
    assert!(retry_after > Duration::ZERO);

    // A16: the seam. The call RETURNED, with the booking still in flight — so a
    // caller has something to say now and something else to say later. A
    // gateway that converged internally would have blocked here and collapsed
    // the two messages into one.
    let mid = gw.read(&id).await.expect("read");
    assert_eq!(
        mid.state, "BookingInProgress",
        "the chase must still be running when propose_at returns"
    );

    // A15: convergence is a separate call, honours the wait, and never re-POSTs.
    let started = std::time::Instant::now();
    let settled = gw.converge(&id, retry_after).await.expect("converge");
    assert!(
        started.elapsed() >= retry_after,
        "the gateway must wait at least Retry-After before its first poll — a \
         gateway that polls immediately hammers the server it was just told to \
         leave alone (deepseek: the plan promised this assertion and the first \
         build forgot it)"
    );
    assert_eq!(settled.state, "Booked");
    assert!(settled.booking_ref.is_some());

    // One council booking, not two — the whole point of not re-POSTing.
    assert_eq!(council_count(&world, "SELECT COUNT(*) FROM bookings"), 1);
}

// ------------------------------------------------------------------ A17

/// Convergence gives up typed rather than looping forever — deterministically.
///
/// The first version allowed `Ok(_)`, "in case the reconciler won the race" —
/// which made it a test that could not fail. Killing the council removes the
/// race: the effect can NEVER settle, so anything but `NotConverged` is a bug.
#[tokio::test]
async fn a17_convergence_is_bounded() {
    let mut world = world();
    let gw = gateway(&world, LUCY).with_policy(RetryPolicy {
        max_convergence_polls: 2,
        convergence_deadline: Duration::from_millis(400),
    });
    let id = BookingId::new("BKG-A17");
    let version = awaiting(&gw, &id).await;

    let effect = format!("EFF-{}-BOOK-{version}", id.as_str());
    arm_fault(&world, &effect, "create", "drop_response").await;
    let turn = gw
        .propose_at(&id, version, "book", None)
        .await
        .expect("turn");
    let Turn::Accepted { .. } = turn else {
        panic!("expected Accepted: {turn:?}");
    };

    // Now nothing can ever settle this effect.
    world.kill_council();

    match gw.converge(&id, Duration::from_millis(50)).await {
        Err(GatewayError::NotConverged { attempts }) => {
            assert!(attempts <= 2, "the bound is the polls, not luck");
        }
        other => panic!("an unsettleable chase must end NotConverged: {other:?}"),
    }
}

/// The deadline caps every sleep — a server saying Retry-After: 3600 against a
/// short deadline must not block for an hour before the first clock check.
#[tokio::test]
async fn a17_the_deadline_caps_the_sleep_itself() {
    let mut world = world();
    let gw = gateway(&world, LUCY).with_policy(RetryPolicy {
        max_convergence_polls: 8,
        convergence_deadline: Duration::from_millis(300),
    });
    let id = BookingId::new("BKG-A17B");
    let version = awaiting(&gw, &id).await;
    let effect = format!("EFF-{}-BOOK-{version}", id.as_str());
    arm_fault(&world, &effect, "create", "drop_response").await;
    let Turn::Accepted { .. } = gw
        .propose_at(&id, version, "book", None)
        .await
        .expect("turn")
    else {
        panic!("expected Accepted");
    };
    world.kill_council();

    // An HOUR of first_wait against a 300ms deadline: with the sleep capped by
    // the remaining deadline this returns promptly; the first implementation
    // slept the whole hour before consulting the clock.
    let started = std::time::Instant::now();
    let outcome = gw.converge(&id, Duration::from_secs(3600)).await;
    assert!(
        matches!(outcome, Err(GatewayError::NotConverged { .. })),
        "{outcome:?}"
    );
    assert!(
        started.elapsed() < Duration::from_secs(5),
        "the deadline did not bound the sleep: {:?}",
        started.elapsed()
    );
}

// ------------------------------------------------------------------ A18

/// M5.1's ownership, as the client sees it.
#[tokio::test]
async fn a18_ownership_reaches_the_client() {
    let world = world();
    let lucy = gateway(&world, LUCY);
    let priya = gateway(&world, PRIYA);
    let id = BookingId::new("BKG-A18");
    lucy.create(&id, &requirements()).await.expect("create");

    for outcome in [priya.read(&id).await.err(), priya.audit(&id).await.err()] {
        assert!(
            matches!(outcome, Some(GatewayError::UnknownBooking)),
            "a foreign read must be indistinguishable from absent: {outcome:?}"
        );
    }
    // Paired: the owner sees it, so this did not pass by refusing everyone.
    assert!(lucy.read(&id).await.is_ok());
    assert!(lucy.audit(&id).await.is_ok());

    // The lookups are scoped the same way, and a foreign reference is an empty
    // list rather than a refusal.
    assert_eq!(lucy.cancellable().await.expect("lookup").len(), 1);
    assert!(priya.cancellable().await.expect("lookup").is_empty());
}

// ------------------------------------------------------------------ A14b

/// A request id the caller chose comes back; one it did not is still recorded.
///
/// Through the GATEWAY, both halves — the first version made raw `reqwest` calls,
/// so the gateway's own recording (the thing under test) was never touched, and
/// in fact the gateway threw the header away.
#[tokio::test]
async fn a14b_request_ids_survive_the_round_trip() {
    let world = world();
    let id = BookingId::new("BKG-REQID");

    let chosen = gateway(&world, LUCY).with_request_id("req-of-my-own");
    chosen.create(&id, &requirements()).await.expect("create");
    assert_eq!(
        chosen.last_request_id().as_deref(),
        Some("req-of-my-own"),
        "the middleware echoes a supplied id verbatim, and the gateway keeps it"
    );

    let minted = gateway(&world, LUCY);
    minted.read(&id).await.expect("read");
    let value = minted
        .last_request_id()
        .expect("the server mints one when the caller sends none");
    assert!(
        value.starts_with("req-") && value != "req-of-my-own",
        "unexpected id {value:?}"
    );
}

// ------------------------------------------------------------------ M6A's gate

/// **M6A's gate, clause (a).** Creation → `Booked` → `Cancelled`, through the
/// gateway alone, clean and faulted.
///
/// This is M6B's whole journey minus the human. If the gateway cannot do it, the
/// orchestrator cannot either — and finding that out here costs one slice
/// instead of two.
#[tokio::test]
async fn m6a_gate_a_full_journey_through_the_gateway_alone() {
    let world = world();
    let gw = gateway(&world, LUCY);

    // --- clean: the council answers, so every turn settles synchronously.
    let clean = BookingId::new("BKG-GATE-CLEAN");
    let version = awaiting(&gw, &clean).await;
    let booked = gw
        .propose_at(&clean, version, "book", None)
        .await
        .expect("book");
    let Turn::Committed { state, version } = booked else {
        panic!("an answering council settles synchronously: {booked:?}");
    };
    assert_eq!(state, "Booked", "no convergence step should be needed");

    let reference = gw
        .read(&clean)
        .await
        .expect("read")
        .booking_ref
        .expect("a council reference");

    // The reference is findable through the authoritative lookup — the path
    // `CANCEL TH-…` will take in M6B.
    let found = gw
        .by_reference(&CouncilBookingRef::new(reference.clone()))
        .await
        .expect("lookup");
    assert_eq!(found.len(), 1);
    assert_eq!(found[0].id, "BKG-GATE-CLEAN");

    let cancelled = gw
        .propose_at(
            &clean,
            version,
            "cancel",
            Some(serde_json::json!({"reason": "no longer needed"})),
        )
        .await
        .expect("cancel");
    let Turn::Committed { state, .. } = cancelled else {
        panic!("a clean cancel settles synchronously too: {cancelled:?}");
    };
    assert_eq!(state, "Cancelled");

    // --- faulted: the book response is lost, so the chase owns the outcome.
    //
    // This half runs through the RECORDING PROXY, because "never re-POSTs"
    // cannot be witnessed at the council: it is idempotent on effect identity,
    // so an erroneous second POST leaves exactly one row and the wrong
    // implementation walks free. The requests themselves are the witness.
    let proxy = harness::RecordingProxy::in_front_of(&world.server_url);
    let gw = Gateway::new(proxy.url.clone(), LUCY);
    let faulted = BookingId::new("BKG-GATE-FAULT");
    let version = awaiting(&gw, &faulted).await;
    let effect = format!("EFF-{}-BOOK-{version}", faulted.as_str());
    let fault = arm_fault(&world, &effect, "create", "drop_response").await;

    let turn = gw
        .propose_at(&faulted, version, "book", None)
        .await
        .expect("book");
    let Turn::Accepted { retry_after } = turn else {
        panic!("a dropped response is an acceptance: {turn:?}");
    };
    assert_eq!(
        fault_fired(&world, fault).await,
        1,
        "the drop genuinely fired"
    );
    let settled = gw.converge(&faulted, retry_after).await.expect("converge");
    assert_eq!(settled.state, "Booked");
    assert_eq!(
        proxy.count("POST", "/behaviours/book"),
        1,
        "convergence must never re-POST the behaviour — the chase owns the \
         effect (ADR-019), and a second POST risks a second council booking \
         that idempotency would then hide from a row count: {:?}",
        proxy.requests()
    );

    let cancelled = gw
        .propose_at(
            &faulted,
            settled.version,
            "cancel",
            Some(serde_json::json!({"reason": "done"})),
        )
        .await
        .expect("cancel");
    let final_state = match cancelled {
        Turn::Committed { state, .. } => state,
        Turn::Accepted { retry_after } => {
            gw.converge(&faulted, retry_after)
                .await
                .expect("converge")
                .state
        }
        other => panic!("unexpected cancel outcome: {other:?}"),
    };
    assert_eq!(final_state, "Cancelled");

    // The council holds exactly two bookings, both cancelled — one per journey,
    // and no duplicate from the chase.
    assert_eq!(council_count(&world, "SELECT COUNT(*) FROM bookings"), 2);
    assert_eq!(
        council_count(
            &world,
            "SELECT COUNT(*) FROM bookings WHERE cancelled_by IS NOT NULL"
        ),
        2
    );
}

/// The catalogue, round-tripped — because the first `VenueRow` had a field the
/// wire has never sent and was missing two it does, and stayed green precisely
/// because nothing drove it.
#[tokio::test]
async fn the_catalogue_round_trips() {
    let world = world();
    let rows = gateway(&world, LUCY).venues().await.expect("venues");
    assert_eq!(rows.len(), 4, "spec §11's four seeded venues");
    let th_a = rows
        .iter()
        .find(|row| row.venue_id == "TH-A")
        .expect("TH-A");
    assert_eq!(th_a.slot_id, "SLOT-A");
    assert_eq!(th_a.capacity, 30);
    assert!(th_a.accessible);
    assert_eq!(th_a.fee_pence, 4_500);
    assert!(th_a.available);

    let facts = gateway(&world, LUCY)
        .slot("TH-A", "SLOT-A")
        .await
        .expect("slot")
        .expect("facts");
    assert_eq!(facts.fee_pence, 4_500);
}

// ------------------------------------------------------------------ A14: the 503s

/// The two 503 shapes are two different situations, and the gateway must not
/// conflate them — the council going quiet is not the service being down, and
/// Lucy is owed a different sentence for each.
#[tokio::test]
async fn a14_the_two_503_shapes_are_distinguished() {
    let mut world = world();
    let gw = gateway(&world, LUCY);

    // A booking parked where its next step needs the provider.
    let id = BookingId::new("BKG-503");
    let created = gw.create(&id, &requirements()).await.expect("create");
    let selected = gw
        .propose_at(
            &id,
            created.version,
            "select-venue",
            Some(serde_json::json!({"venue_id": "TH-A", "slot_id": "SLOT-A"})),
        )
        .await
        .expect("turn");
    let Turn::Committed { version, .. } = selected else {
        panic!("select-venue: {selected:?}");
    };

    world.kill_council();

    // 503 as a DOMAIN DENIAL: verify-slot needs facts the council can no longer
    // give, and the domain refuses with its own name — FactsUnavailable — which
    // arrives with an ETag and a detail. The gateway surfaces it as a Denied
    // turn, because the caller's booking is fine; the world is not.
    let turn = gw
        .propose_at(&id, version, "verify-slot", None)
        .await
        .expect("a turn, not a transport error");
    assert_eq!(
        turn,
        Turn::Denied {
            reason: "FactsUnavailable".to_owned()
        },
        "an unreachable provider is the domain's refusal, not a malformed request"
    );

    // 503 PLAIN: the venues catalogue cannot be asked at all. Driven through
    // the GATEWAY — the first version used raw reqwest and never observed
    // `GatewayError::Unavailable` at all, so the subject of the test was
    // bypassed by the test.
    let plain = gw.venues().await;
    assert!(
        matches!(plain, Err(GatewayError::Unavailable(_))),
        "a dead catalogue is Unavailable, not ProviderSilent: {plain:?}"
    );
}

// ------------------------------------------------------------------ A17: 429

/// Contention surfaces immediately and typed — and the 429 was telling the
/// truth about why a blind retry is wrong.
///
/// Writing this test found the behaviour: the first version had the gateway
/// re-POST the same version after a wait, and the retry came back
/// `Stale {current: 3}` — because the contended turn had ALREADY COMMITTED
/// `BookingInProgress`. The wire's 429 body says "re-read and retry"; this test
/// now proves both halves — the typed refusal, and the re-read that shows what
/// the 429 was hiding.
#[tokio::test]
async fn a17_contention_surfaces_typed_and_a_reread_tells_the_truth() {
    // The deterministic-429 seam: zero reclassification attempts (ADR-021's
    // sanctioned zero).
    let world = harness::world_with(&["--reclassify-attempts", "0"]);
    let gw = gateway(&world, LUCY);

    let id = BookingId::new("BKG-429");
    let version = awaiting(&gw, &id).await;

    let started = std::time::Instant::now();
    let outcome = gw.propose_at(&id, version, "book", None).await;
    assert!(
        matches!(outcome, Err(GatewayError::Contended)),
        "zero sanctioned attempts must surface as Contended: {outcome:?}"
    );
    assert!(
        started.elapsed() < Duration::from_secs(1),
        "no invented client-side waits — the caller decides what to do next"
    );

    // The re-read the 429 demanded: the turn was NOT nothing. The version
    // advanced and the booking is mid-book — which is exactly why re-POSTing
    // the old version could never have been right.
    let truth = gw.read(&id).await.expect("read");
    assert!(
        truth.version > version,
        "the contended turn committed: {} -> {}",
        version,
        truth.version
    );
    assert_eq!(truth.state, "BookingInProgress");
}
