//! The adapter against the real council, over a real socket.
//!
//! Real TCP rather than a router-in-memory: the point of slice D is that the
//! protocol survives a network, and an in-process call proves nothing about
//! serialisation, status codes or a connection that drops.
//!
//! These are the client's own gates. Whether the *coordinator* still works over
//! HTTP is a separate suite — this one is about whether the adapter turns a socket
//! into the traits the boundary speaks, and refuses what it should.

use bld_kernel::{Capability, Unknown, VerificationError, Verifier};
use bld_types::{
    AvailabilityGrant, EffectAttempt, EffectIntentId, Money, PrincipalId, SlotId, VenueId,
};
use council_client::{CouncilClient, CouncilVerifier};
use council_wire::{CouncilKey, CouncilSigner};
use ed25519_dalek::SigningKey;
use mock_council::{Council, SeedSlot, clock::TestClock, pause::NeverPauses};
use std::sync::Arc;
use tempfile::TempDir;
use townhall_domain::{BookingEffect, OperationKind, VenueFacts, VerifiedProviderFact};
use townhall_service::AvailabilitySource;

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
        venue_id: "TH-B",
        slot_id: "SLOT-A",
        fee_pence: 4_500,
        capacity: 30,
        accessible: false,
        available: true,
    },
];

struct Harness {
    _dir: TempDir,
    council: Arc<Council>,
    client: CouncilClient,
    clock: Arc<TestClock>,
    /// A key that is not the council's, for the forgery gates.
    impostor: CouncilSigner,
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
                Arc::clone(&clock) as Arc<_>,
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

        Self {
            _dir: dir,
            council,
            client: CouncilClient::new(format!("http://{address}"), CouncilKey::new(public)),
            clock,
            impostor: CouncilSigner::new(SigningKey::from_bytes(&[9u8; 32])),
        }
    }

    fn attempt(id: &str) -> EffectAttempt {
        EffectAttempt {
            id: EffectIntentId::new(id),
            expires_at_ms: DEADLINE,
        }
    }

    /// Read availability the way the coordinator does, and keep the observation.
    async fn observe(&self, venue: &str, slot: &str) -> Option<(VenueFacts, AvailabilityGrant)> {
        match self
            .client
            .read(&VenueId::new(venue), &SlotId::new(slot))
            .await
        {
            townhall_domain::ObservedAvailability::Answered(observation) => {
                observation.map(|verified| {
                    let observation = verified.into_inner();
                    (observation.facts, observation.grant)
                })
            }
            townhall_domain::ObservedAvailability::Unavailable => None,
        }
    }

    async fn book(
        &self,
        id: &str,
        venue: &str,
        slot: &str,
    ) -> Result<VerifiedProviderFact, String> {
        let (facts, grant) = self
            .observe(venue, slot)
            .await
            .ok_or("no availability answer")?;

        let raw = self
            .client
            .execute(
                &BookingEffect::Book {
                    principal: PrincipalId::new("lucy"),
                    attendees: 20,
                    facts,
                    grant,
                },
                &Self::attempt(id),
            )
            .await
            .map_err(|Unknown { .. }| "unknown".to_owned())?;

        CouncilVerifier::new(CouncilKey::new(
            self.council.registry().signer().verifying_key(),
        ))
        .verify(raw)
        .map(bld_kernel::Verified::into_inner)
        .map_err(|error| format!("{error:?}"))
    }
}

// -------------------------------------------------------------- availability

/// The facts a guard will read, carried over a socket and verified.
#[tokio::test]
async fn availability_arrives_verified_with_a_grant() {
    let h = Harness::new().await;

    let (facts, grant) = h.observe("TH-A", "SLOT-A").await.expect("an answer");

    assert_eq!(facts.venue_id, VenueId::new("TH-A"));
    assert_eq!(facts.capacity, 30);
    assert!(facts.wheelchair_accessible);
    assert!(facts.available);
    assert_eq!(facts.fee, Money::from_pence(4_500));
    assert!(
        !grant.on_the_wire().is_empty(),
        "the observation carries a warrant"
    );
}

/// `available` was missing from the catalogue, the wire and the encoding in an
/// earlier draft, so the client would have had to invent it — and `true` is the
/// obvious guess. This proves the value crosses the socket rather than being
/// assumed.
#[tokio::test]
async fn availability_carries_every_field_a_guard_reads() {
    let h = Harness::new().await;
    h.council
        .seed(&[SeedSlot {
            venue_id: "TH-A",
            slot_id: "SLOT-A",
            fee_pence: 7_000,
            capacity: 12,
            accessible: false,
            available: false,
        }])
        .await
        .expect("reseed");

    let (facts, _) = h.observe("TH-A", "SLOT-A").await.expect("an answer");

    assert_eq!(facts.capacity, 12);
    assert!(!facts.wheelchair_accessible);
    assert!(!facts.available, "availability is read, not assumed");
    assert_eq!(facts.fee, Money::from_pence(7_000));
}

#[tokio::test]
async fn an_unknown_slot_yields_no_answer() {
    let h = Harness::new().await;
    assert!(h.observe("TH-NOWHERE", "SLOT-A").await.is_none());
}

/// A council that cannot be reached is no answer, not a false one — and since
/// ADR-021 it is its own SHAPE of no-answer: `Unavailable` (the wire's 503),
/// never `Answered(None)` (an answer meaning "nothing there", the wire's 422).
#[tokio::test]
async fn an_unreachable_council_yields_unavailable_not_an_answer() {
    let client = CouncilClient::new(
        // Port 1 on loopback, which nothing is listening on.
        "http://127.0.0.1:1",
        CouncilKey::new(CouncilSigner::new(SigningKey::from_bytes(&[7u8; 32])).verifying_key()),
    );

    assert!(matches!(
        client
            .read(&VenueId::new("TH-A"), &SlotId::new("SLOT-A"))
            .await,
        townhall_domain::ObservedAvailability::Unavailable
    ));
}

// ------------------------------------------------------------------ booking

/// Lucy's booking, over HTTP, with complete canonical facts coming back.
#[tokio::test]
async fn a_booking_returns_the_councils_own_facts() {
    let h = Harness::new().await;

    let fact = h.book("EFF-1", "TH-A", "SLOT-A").await.expect("booked");

    let VerifiedProviderFact::BookingExists {
        effect_intent_id,
        booking_ref,
        venue_id,
        slot_id,
        attendees,
        fee,
        principal,
    } = fact
    else {
        panic!("expected BookingExists, got {fact:?}");
    };

    assert_eq!(effect_intent_id, EffectIntentId::new("EFF-1"));
    assert!(!booking_ref.as_str().is_empty());
    assert_eq!(venue_id, VenueId::new("TH-A"));
    assert_eq!(slot_id, SlotId::new("SLOT-A"));
    assert_eq!(attendees, 20);
    // The council's number, read from its catalogue — not the one we sent back to
    // ourselves.
    assert_eq!(fee, Money::from_pence(4_500));
    assert_eq!(principal, PrincipalId::new("lucy"));
}

/// One identity, one booking, across two HTTP calls.
#[tokio::test]
async fn a_retry_over_http_returns_the_original() {
    let h = Harness::new().await;

    let first = h.book("EFF-1", "TH-A", "SLOT-A").await.expect("first");
    let again = h.book("EFF-1", "TH-A", "SLOT-A").await.expect("second");

    assert_eq!(first, again);
}

/// The gate for round 4's first critical: the request must carry the *persisted*
/// deadline, not one the adapter arrived at itself.
///
/// Book with one deadline, then resolve with a different one. The council bound
/// the first and refuses the second as a conflict — which is only observable
/// because the deadline travels in the attempt rather than being re-derived per
/// call. An adapter that computed its own would have bound something neither the
/// intent nor this test knows.
#[tokio::test]
async fn the_deadline_the_council_binds_is_the_one_we_sent() {
    let h = Harness::new().await;
    h.book("EFF-1", "TH-A", "SLOT-A").await.expect("booked");

    let mismatched = EffectAttempt {
        id: EffectIntentId::new("EFF-1"),
        expires_at_ms: DEADLINE - 5_000,
    };
    let raw =
        townhall_service::EffectResolver::resolve(&h.client, &mismatched, OperationKind::Book)
            .await
            .expect("an answer");

    let refused = h.client.verifier().verify(answered(raw));
    match refused {
        Err(VerificationError::Unknown(detail)) => {
            assert!(
                detail.as_str().contains("contradicts"),
                "a conflict must stay Unknown, never become a fact: {detail}"
            );
        }
        other => panic!("expected Unknown for a deadline conflict, got {other:?}"),
    }
}

/// A fee we made up is refused, and terminally — the council will not book at a
/// price it never quoted.
#[tokio::test]
async fn a_fee_we_invented_is_rejected() {
    let h = Harness::new().await;
    let (mut facts, grant) = h.observe("TH-A", "SLOT-A").await.expect("an answer");
    facts.fee = Money::from_pence(1);

    let raw = h
        .client
        .execute(
            &BookingEffect::Book {
                principal: PrincipalId::new("lucy"),
                attendees: 20,
                facts,
                grant,
            },
            &Harness::attempt("EFF-CHEAP"),
        )
        .await
        .expect("an answer");

    let fact = h.client.verifier().verify(raw).expect("verified");
    match fact.get() {
        VerifiedProviderFact::ProviderRejected { reason, .. } => {
            assert!(reason.as_str().contains("4500"), "reason: {reason}");
        }
        other => panic!("expected a rejection, got {other:?}"),
    }
}

/// Lucy's room, with the lift broken between authorising and booking.
///
/// Every field the request carries still matches. Only the catalogue's version
/// moved, and the grant is what notices.
#[tokio::test]
async fn a_stale_grant_is_rejected_over_http() {
    let h = Harness::new().await;
    let (facts, grant) = h.observe("TH-A", "SLOT-A").await.expect("an answer");

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

    let raw = h
        .client
        .execute(
            &BookingEffect::Book {
                principal: PrincipalId::new("lucy"),
                attendees: 20,
                facts,
                grant,
            },
            &Harness::attempt("EFF-LIFT"),
        )
        .await
        .expect("an answer");

    let fact = h.client.verifier().verify(raw).expect("verified");
    assert!(
        matches!(fact.get(), VerifiedProviderFact::ProviderRejected { .. }),
        "a stale grant must be refused: {:?}",
        fact.get()
    );
}

// ------------------------------------------------------------- cancellation

#[tokio::test]
async fn a_cancellation_travels_under_its_own_identity() {
    let h = Harness::new().await;
    let booked = h.book("EFF-BOOK", "TH-A", "SLOT-A").await.expect("booked");
    let VerifiedProviderFact::BookingExists { booking_ref, .. } = booked else {
        panic!("expected a booking");
    };

    let raw = h
        .client
        .execute(
            &BookingEffect::CancelBooking {
                booking_ref: booking_ref.clone(),
                principal: bld_types::PrincipalId::new("lucy"),
            },
            &Harness::attempt("EFF-CANCEL"),
        )
        .await
        .expect("an answer");

    let fact = h.client.verifier().verify(raw).expect("verified");
    assert_eq!(
        *fact.get(),
        VerifiedProviderFact::CancellationExists {
            effect_intent_id: EffectIntentId::new("EFF-CANCEL"),
            booking_ref,
        },
        "the fact is about the cancellation's identity, not the booking's"
    );
}

/// Unwrap an answer, refusing the not-yet arm: these tests ask about effects
/// whose answers are settled or conflicted, so a `NotYetVisible` here means
/// the fixture is wrong, not the protocol.
fn answered(
    resolved: townhall_service::Resolved<council_wire::SignedEffectResponse>,
) -> council_wire::SignedEffectResponse {
    match resolved {
        townhall_service::Resolved::Answer(raw) => raw,
        townhall_service::Resolved::NotYetVisible => {
            panic!("expected an answer; the council said 'not yet'")
        }
    }
}

// -------------------------------------------------------------- reconciliation

/// The typed path a reconciler needs: ask about an intent that was never
/// delivered, before its deadline, and get told nothing has settled.
///
/// Since ADR-020 that reply has its own type: the authenticated,
/// identity-bound `Resolved::NotYetVisible` — a pursuit signal that never
/// reaches the verifier and never becomes a fact. Reading it as absence is
/// still how a live booking gets cancelled underneath us; what it MAY now do
/// is authorize a resend of the same identity, which is the reconciler's
/// decision, not this client's.
#[tokio::test]
async fn resolving_an_undelivered_intent_before_its_deadline_is_not_yet() {
    let h = Harness::new().await;

    let resolved = townhall_service::EffectResolver::resolve(
        &h.client,
        &Harness::attempt("EFF-NEVER-SENT"),
        OperationKind::Book,
    )
    .await
    .expect("a usable reply");

    assert_eq!(
        resolved,
        townhall_service::Resolved::NotYetVisible,
        "unsettled and pre-deadline: the council's signed 'nothing yet', typed"
    );
}

/// Past the deadline the same question has a definitive answer, and only then.
#[tokio::test]
async fn resolving_an_undelivered_intent_after_its_deadline_is_absence() {
    let h = Harness::new().await;
    h.clock.set(DEADLINE + 1);

    let raw = townhall_service::EffectResolver::resolve(
        &h.client,
        &Harness::attempt("EFF-NEVER-SENT"),
        OperationKind::Book,
    )
    .await
    .expect("an answer");

    let fact = h.client.verifier().verify(answered(raw)).expect("verified");
    assert_eq!(
        *fact.get(),
        VerifiedProviderFact::EffectAbsent {
            effect_intent_id: EffectIntentId::new("EFF-NEVER-SENT"),
        }
    );
}

/// A booking made through `execute` is discoverable through `resolve` — the same
/// fact, from the other trait. The reconciler and the caller are asking about the
/// same world.
#[tokio::test]
async fn resolve_finds_what_execute_created() {
    let h = Harness::new().await;
    let created = h.book("EFF-1", "TH-A", "SLOT-A").await.expect("booked");

    let raw = townhall_service::EffectResolver::resolve(
        &h.client,
        &Harness::attempt("EFF-1"),
        OperationKind::Book,
    )
    .await
    .expect("an answer");

    assert_eq!(
        *h.client
            .verifier()
            .verify(answered(raw))
            .expect("verified")
            .get(),
        created
    );
}

/// Asking about a booking identity as though it were a cancellation is our bug,
/// and must not produce a fact.
#[tokio::test]
async fn resolving_with_the_wrong_kind_is_unknown() {
    let h = Harness::new().await;
    h.book("EFF-1", "TH-A", "SLOT-A").await.expect("booked");

    let raw = townhall_service::EffectResolver::resolve(
        &h.client,
        &Harness::attempt("EFF-1"),
        OperationKind::Cancel,
    )
    .await
    .expect("an answer");

    match h.client.verifier().verify(answered(raw)) {
        Err(VerificationError::Unknown(_)) => {}
        other => panic!("expected Unknown for a kind conflict, got {other:?}"),
    }
}

// --------------------------------------------------------------- provenance

/// A response whose every field is right, signed by the wrong key.
///
/// This is the gate for the gap an earlier draft of this slice had: the crate
/// graph stops the *proposer* naming `Verified<T>`, but it does not make an
/// unauthenticated HTTP body genuine.
#[tokio::test]
async fn a_field_perfect_response_from_the_wrong_key_is_refused() {
    let h = Harness::new().await;
    let genuine = h.book("EFF-1", "TH-A", "SLOT-A").await.expect("booked");
    let VerifiedProviderFact::BookingExists {
        booking_ref,
        attendees,
        fee,
        ..
    } = genuine
    else {
        panic!("expected a booking");
    };

    let outcome = council_wire::EffectOutcome::BookingCreated(council_wire::BookingFacts {
        booking_reference: booking_ref.as_str().to_owned(),
        venue_id: "TH-A".to_owned(),
        slot_id: "SLOT-A".to_owned(),
        attendees,
        fee_pence: fee.pence(),
        principal: "lucy".to_owned(),
    });

    let forged = council_wire::SignedEffectResponse {
        effect_intent_id: "EFF-1".to_owned(),
        signature: Some(h.impostor.sign_effect("EFF-1", &outcome).expect("sign")),
        outcome,
    };

    match h.client.verifier().verify(forged) {
        Err(VerificationError::Rejected(_)) => {}
        other => panic!("expected Rejected for a foreign signature, got {other:?}"),
    }
}

/// And one with no signature at all. A missing signature and a wrong one are
/// different failures, and both must be refused.
#[tokio::test]
async fn an_unsigned_response_is_refused() {
    let h = Harness::new().await;

    let unsigned = council_wire::SignedEffectResponse {
        effect_intent_id: "EFF-1".to_owned(),
        outcome: council_wire::EffectOutcome::DefinitivelyAbsent,
        signature: None,
    };

    match h.client.verifier().verify(unsigned) {
        Err(VerificationError::Rejected(_)) => {}
        other => panic!("expected Rejected for an unsigned response, got {other:?}"),
    }
}

/// A validly signed response about a *different* effect verifies — it really is
/// the council's — and is left for the domain to refuse against the persisted
/// intent. Duplicating that check here would put one fact in two places, and the
/// kernel's contract assigns it to the domain.
#[tokio::test]
async fn a_signed_response_for_another_identity_verifies_and_is_the_domains_to_refuse() {
    let h = Harness::new().await;
    h.book("EFF-1", "TH-A", "SLOT-A").await.expect("booked");

    let raw = townhall_service::EffectResolver::resolve(
        &h.client,
        &Harness::attempt("EFF-1"),
        OperationKind::Book,
    )
    .await
    .expect("an answer");

    let fact = h
        .client
        .verifier()
        .verify(answered(raw))
        .expect("a genuine council response verifies");

    assert_eq!(
        fact.get().effect_intent_id(),
        &EffectIntentId::new("EFF-1"),
        "the fact names the effect it is about, for the domain to bind"
    );
}
