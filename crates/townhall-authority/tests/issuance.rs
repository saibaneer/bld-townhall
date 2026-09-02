//! The issuer's battery: one grant per challenge, and every denial provable on
//! its own.
//!
//! The acceptance gate reads "wrong/expired/replayed/tampered challenge/grant
//! denied **independently of prompt**". "Independently" is the load-bearing
//! word: a test that only shows *a* denial cannot say which check produced it,
//! and a reordering that made a different check fire first would keep passing.
//! So every case here arranges exactly one defect and names the error.

use bld_types::{
    ActorId, ApprovalChallengeId, Behaviour, BookingId, BookingRequirements, DelegationId, Money,
    PrincipalId, ServiceId, TimeWindow,
};
use std::sync::Mutex;
use townhall_authority::{
    ApprovalCode, ApprovalDenied, ApprovalRequest, ApprovalStore, AssuranceLevel, AuthorityPolicy,
    AuthorityService, BehaviourSet, BindingRef, Entropy, MAX_ATTEMPTS, MemoryApprovalStore,
    PendingScope, ResolveError,
};

const NOW: u64 = 1_700_000_000_000;
const REPLY_WINDOW_MS: u64 = 600_000;
const GRANT_TTL_MS: u64 = 3_600_000;

/// A deterministic entropy source: a fixed code, and counted identifiers.
///
/// Deterministic so a wrong code is wrong for a stated reason rather than by
/// luck, and counted so two challenges in one test cannot collide silently.
struct FixedEntropy {
    code: String,
    next: Mutex<u64>,
}

impl FixedEntropy {
    fn new(code: &str) -> Self {
        Self {
            code: code.to_owned(),
            next: Mutex::new(0),
        }
    }
}

impl Entropy for FixedEntropy {
    fn code(&self) -> ApprovalCode {
        ApprovalCode::new(self.code.clone()).expect("the test's code is well formed")
    }

    fn identifier(&self) -> String {
        let mut next = self.next.lock().expect("uncontended");
        *next += 1;
        format!("id-{next:04}")
    }
}

type Service = AuthorityService<MemoryApprovalStore, FixedEntropy>;

fn service() -> Service {
    AuthorityService::new(
        MemoryApprovalStore::new(),
        FixedEntropy::new("7312"),
        AuthorityPolicy {
            reply_window_ms: REPLY_WINDOW_MS,
            grant_ttl_ms: GRANT_TTL_MS,
            assurance: AssuranceLevel::SmsReply,
        },
    )
}

fn lucys_binding() -> BindingRef {
    BindingRef {
        principal: PrincipalId::new("lucy"),
        version: 1,
    }
}

/// Lucy asks; Lucy is the grantor and the subject.
fn lucys_request() -> ApprovalRequest {
    ApprovalRequest {
        scope: PendingScope {
            service: ServiceId::new("demo-council-town-hall"),
            agent: "TownHallAgent".to_owned(),
            booking: BookingId::new("sms-lucy-0001"),
            behaviours: BehaviourSet::new([Behaviour::Book, Behaviour::Cancel]),
            requirements: BookingRequirements {
                purpose: "town hall booking".to_owned(),
                requested_date: "2026-09-10".to_owned(),
                time_window: TimeWindow {
                    from: "14:00".to_owned(),
                    to: "17:00".to_owned(),
                },
                attendees: 20,
                wheelchair_accessible: true,
                max_fee: Money::from_pence(5_000),
            },
        },
        binding: lucys_binding(),
        grantor: PrincipalId::new("lucy"),
        subject: PrincipalId::new("lucy"),
    }
}

/// The happy path, and the gate's "£45 scope permitted" half.
///
/// The £50 ceiling is what Lucy approved; the council's slot costs £45. The
/// grant must permit the cheaper booking — a grant that only permitted an exact
/// £50 would refuse every real slot.
#[tokio::test]
async fn an_answered_challenge_yields_one_grant_that_permits_the_forty_five_pound_booking() {
    let service = service();
    let raised = service
        .begin(&lucys_request(), NOW)
        .await
        .expect("challenge");

    assert!(
        raised.preview.contains("Reply YES 7312 to approve."),
        "the preview must carry the code the person is to send back"
    );

    let grant = service
        .submit(
            &raised.id,
            "7312",
            &lucys_binding(),
            AssuranceLevel::SmsReply,
            NOW + 1_000,
        )
        .await
        .expect("a correct code from the bound channel");

    assert_eq!(grant.grantor().as_str(), "lucy");
    assert_eq!(grant.subject().as_str(), "lucy");
    assert_eq!(grant.assurance(), AssuranceLevel::SmsReply);
    assert_eq!(grant.max_fee(), Money::from_pence(5_000));
    assert!(
        Money::from_pence(4_500).pence() <= grant.max_fee().pence(),
        "the £45 slot must fit inside the £50 ceiling Lucy approved"
    );
    assert!(grant.permits(
        Behaviour::Book,
        &BookingId::new("sms-lucy-0001"),
        NOW + 1_000
    ));
}

/// The grant's clock starts at the approval, not at the offer.
///
/// # The bug this pins
///
/// The first design had one deadline. Approving in the last second of the reply
/// window then issued a grant that had already expired — and every test that
/// approved immediately would have passed.
#[tokio::test]
async fn a_grant_approved_at_the_last_second_still_has_its_full_life() {
    let service = service();
    let raised = service
        .begin(&lucys_request(), NOW)
        .await
        .expect("challenge");
    let last_moment = NOW + REPLY_WINDOW_MS - 1;

    let grant = service
        .submit(
            &raised.id,
            "7312",
            &lucys_binding(),
            AssuranceLevel::SmsReply,
            last_moment,
        )
        .await
        .expect("answered inside the window");

    assert_eq!(grant.expires_at_ms(), last_moment + GRANT_TTL_MS);
    assert!(grant.permits(
        Behaviour::Book,
        &BookingId::new("sms-lucy-0001"),
        last_moment + GRANT_TTL_MS - 1
    ));
    assert!(
        !grant.permits(
            Behaviour::Book,
            &BookingId::new("sms-lucy-0001"),
            last_moment + GRANT_TTL_MS
        ),
        "the grant must stop at its own deadline"
    );
}

/// One challenge yields at most one grant.
///
/// # Why this is not "a grant may be used once"
///
/// ADR-025 records the distinction as a trap: a grant is presented on every
/// call of the workflow it authorizes, so refusing its second USE would break
/// create → select → verify → book while passing a naively-written test. What
/// must not happen twice is ISSUANCE.
#[tokio::test]
async fn a_replayed_approval_does_not_mint_a_second_grant() {
    let service = service();
    let raised = service
        .begin(&lucys_request(), NOW)
        .await
        .expect("challenge");

    let first = service
        .submit(
            &raised.id,
            "7312",
            &lucys_binding(),
            AssuranceLevel::SmsReply,
            NOW + 1_000,
        )
        .await
        .expect("the first correct answer");

    let second = service
        .submit(
            &raised.id,
            "7312",
            &lucys_binding(),
            AssuranceLevel::SmsReply,
            NOW + 2_000,
        )
        .await;

    assert_eq!(second, Err(ApprovalDenied::Replay("approved")));

    // And the grant that DID issue is still usable, many times over.
    for moment in [NOW + 3_000, NOW + 4_000, NOW + 5_000] {
        let resolved = service
            .resolve(first.delegation(), moment)
            .await
            .expect("a live grant resolves on every presentation");
        assert_eq!(resolved, first, "resolution must not alter the grant");
    }
}

/// A late answer is refused as late, whatever the code says.
#[tokio::test]
async fn an_expired_challenge_is_denied_before_the_code_is_read() {
    let service = service();
    let raised = service
        .begin(&lucys_request(), NOW)
        .await
        .expect("challenge");

    let denied = service
        .submit(
            &raised.id,
            "7312",
            &lucys_binding(),
            AssuranceLevel::SmsReply,
            NOW + REPLY_WINDOW_MS,
        )
        .await;

    assert_eq!(denied, Err(ApprovalDenied::ChallengeExpired));
}

/// A wrong code costs an attempt, and says how many are left.
#[tokio::test]
async fn a_wrong_code_costs_one_attempt() {
    let service = service();
    let raised = service
        .begin(&lucys_request(), NOW)
        .await
        .expect("challenge");

    assert_eq!(
        service
            .submit(
                &raised.id,
                "0000",
                &lucys_binding(),
                AssuranceLevel::SmsReply,
                NOW + 1_000
            )
            .await,
        Err(ApprovalDenied::WrongCode {
            attempts_left: MAX_ATTEMPTS - 1
        })
    );

    // The right code still works while attempts remain.
    service
        .submit(
            &raised.id,
            "7312",
            &lucys_binding(),
            AssuranceLevel::SmsReply,
            NOW + 2_000,
        )
        .await
        .expect("one wrong guess must not spend the challenge");
}

/// The attempt bound is what makes a four-digit code safe.
#[tokio::test]
async fn the_attempt_bound_spends_the_challenge_and_the_right_code_no_longer_helps() {
    let service = service();
    let raised = service
        .begin(&lucys_request(), NOW)
        .await
        .expect("challenge");

    for remaining in (0..MAX_ATTEMPTS).rev() {
        let denied = service
            .submit(
                &raised.id,
                "0000",
                &lucys_binding(),
                AssuranceLevel::SmsReply,
                NOW + 1_000,
            )
            .await;
        let expected = if remaining == 0 {
            ApprovalDenied::AttemptsExceeded
        } else {
            ApprovalDenied::WrongCode {
                attempts_left: remaining,
            }
        };
        assert_eq!(denied, Err(expected), "at {remaining} attempts left");
    }

    assert_eq!(
        service
            .submit(
                &raised.id,
                "7312",
                &lucys_binding(),
                AssuranceLevel::SmsReply,
                NOW + 2_000
            )
            .await,
        Err(ApprovalDenied::Replay("exhausted")),
        "a spent challenge must not accept the correct code afterwards"
    );
}

/// A reply from another channel is refused, and does NOT burn an attempt.
///
/// # Why the attempt must survive
///
/// Consuming one here would let anyone who learns a challenge id spend Lucy's
/// three tries from another number — a denial of service on her booking.
/// Refusing before the code check gives an attacker nothing in exchange,
/// because they never reach the code at all.
#[tokio::test]
async fn a_reply_from_another_channel_is_denied_and_costs_lucy_nothing() {
    let service = service();
    let raised = service
        .begin(&lucys_request(), NOW)
        .await
        .expect("challenge");
    let stranger = BindingRef {
        principal: PrincipalId::new("mallory"),
        version: 1,
    };

    assert_eq!(
        service
            .submit(
                &raised.id,
                "7312",
                &stranger,
                AssuranceLevel::SmsReply,
                NOW + 1_000
            )
            .await,
        Err(ApprovalDenied::WrongChannel)
    );

    let challenge = service
        .store()
        .load_challenge(&raised.id)
        .await
        .expect("loadable")
        .expect("still there");
    assert_eq!(
        challenge.attempts_left(),
        MAX_ATTEMPTS,
        "a stranger's guess must not spend the bound person's attempts"
    );
}

/// A binding that has moved beneath the challenge can no longer answer it.
///
/// The recurring defect of this project's reviews is state outliving the moment
/// it was true. A challenge bound only to a principal would still verify after
/// the number behind that principal had been re-verified or reassigned.
#[tokio::test]
async fn a_binding_at_a_newer_revision_cannot_answer_an_older_challenge() {
    let service = service();
    let raised = service
        .begin(&lucys_request(), NOW)
        .await
        .expect("challenge");
    let reverified = BindingRef {
        principal: PrincipalId::new("lucy"),
        version: 2,
    };

    assert_eq!(
        service
            .submit(
                &raised.id,
                "7312",
                &reverified,
                AssuranceLevel::SmsReply,
                NOW + 1_000
            )
            .await,
        Err(ApprovalDenied::WrongChannel)
    );
}

/// `NO` is terminal, and a later `YES` does not revive it.
#[tokio::test]
async fn a_rejected_challenge_stays_rejected() {
    let service = service();
    let raised = service
        .begin(&lucys_request(), NOW)
        .await
        .expect("challenge");

    service
        .reject(&raised.id, "7312", &lucys_binding(), NOW + 1_000)
        .await
        .expect("Lucy may decline");

    assert_eq!(
        service
            .submit(
                &raised.id,
                "7312",
                &lucys_binding(),
                AssuranceLevel::SmsReply,
                NOW + 2_000
            )
            .await,
        Err(ApprovalDenied::Replay("rejected")),
        "a declined request must not be revivable by a later YES"
    );
}

/// Rejection needs the code too — otherwise anyone could cancel it.
#[tokio::test]
async fn a_rejection_without_the_code_is_refused() {
    let service = service();
    let raised = service
        .begin(&lucys_request(), NOW)
        .await
        .expect("challenge");

    assert_eq!(
        service
            .reject(&raised.id, "0000", &lucys_binding(), NOW + 1_000)
            .await,
        Err(ApprovalDenied::WrongCode {
            attempts_left: MAX_ATTEMPTS - 1
        })
    );
    assert_eq!(
        service
            .reject(
                &raised.id,
                "7312",
                &BindingRef {
                    principal: PrincipalId::new("mallory"),
                    version: 1
                },
                NOW + 1_000
            )
            .await,
        Err(ApprovalDenied::WrongChannel)
    );
}

/// A challenge nobody raised is not an opportunity.
#[tokio::test]
async fn an_unknown_challenge_is_denied() {
    let service = service();
    assert_eq!(
        service
            .submit(
                &ApprovalChallengeId::new("never-raised"),
                "7312",
                &lucys_binding(),
                AssuranceLevel::SmsReply,
                NOW
            )
            .await,
        Err(ApprovalDenied::UnknownChallenge)
    );
}

/// The issuer caps assurance at the weakest of policy and binding.
///
/// A stored level nothing compares against is decoration; this is the
/// comparison. A binding that established only `Dev` cannot carry an SMS-level
/// grant, however the challenge was raised.
#[tokio::test]
async fn the_grant_never_claims_more_assurance_than_the_binding_established() {
    let service = service();
    let raised = service
        .begin(&lucys_request(), NOW)
        .await
        .expect("challenge");

    let grant = service
        .submit(
            &raised.id,
            "7312",
            &lucys_binding(),
            AssuranceLevel::Dev,
            NOW + 1_000,
        )
        .await
        .expect("answered");

    assert_eq!(
        grant.assurance(),
        AssuranceLevel::Dev,
        "an SMS-level policy must not lift a Dev-level binding"
    );
    assert!(!grant.assurance().meets(AssuranceLevel::SmsReply));
}

/// Revocation stops the next resolution, and is idempotent.
#[tokio::test]
async fn revocation_takes_effect_at_once_and_twice_is_not_an_error() {
    let service = service();
    let raised = service
        .begin(&lucys_request(), NOW)
        .await
        .expect("challenge");
    let grant = service
        .submit(
            &raised.id,
            "7312",
            &lucys_binding(),
            AssuranceLevel::SmsReply,
            NOW + 1_000,
        )
        .await
        .expect("answered");

    service
        .resolve(grant.delegation(), NOW + 2_000)
        .await
        .expect("live before revocation");

    assert!(
        service
            .revoke(grant.delegation(), NOW + 3_000)
            .await
            .expect("ok"),
        "the first revocation is the one that did it"
    );
    assert!(
        !service
            .revoke(grant.delegation(), NOW + 4_000)
            .await
            .expect("ok"),
        "a second REVOKE is a safety exit, not an error"
    );
    assert_eq!(
        service.resolve(grant.delegation(), NOW + 5_000).await,
        Err(ResolveError::Revoked)
    );
}

/// An expired grant stops resolving, without anyone revoking it.
#[tokio::test]
async fn an_expired_grant_no_longer_resolves() {
    let service = service();
    let raised = service
        .begin(&lucys_request(), NOW)
        .await
        .expect("challenge");
    let grant = service
        .submit(
            &raised.id,
            "7312",
            &lucys_binding(),
            AssuranceLevel::SmsReply,
            NOW + 1_000,
        )
        .await
        .expect("answered");

    assert_eq!(
        service
            .resolve(grant.delegation(), grant.expires_at_ms())
            .await,
        Err(ResolveError::Expired)
    );
}

/// A reference nobody issued resolves to nothing.
#[tokio::test]
async fn an_unissued_reference_resolves_to_nothing() {
    let service = service();
    assert_eq!(
        service.resolve(&DelegationId::new("guessed"), NOW).await,
        Err(ResolveError::Unknown)
    );
}

/// The gate's "tampered grant" half, at the service seam.
///
/// # Why a hostile store rather than editing a row
///
/// Reaching past the port to overwrite a row would mean giving the production
/// store an overwrite method — itself a minting path, added for a test. The
/// byte-level battery lives in the codec's own tests, where the private
/// constructor is in scope; what belongs HERE is the seam's behaviour when a
/// row does not decode.
///
/// ADR-025 records why this test is not written against the HTTP layer: that
/// layer refuses the delegation header as its first statement, so a tampered
/// envelope posted there is denied by the reservation and the test would pass
/// against code that checks nothing.
#[tokio::test]
async fn a_delegation_that_does_not_decode_is_refused_rather_than_half_believed() {
    struct CorruptStore;

    #[async_trait::async_trait]
    impl ApprovalStore for CorruptStore {
        async fn insert_challenge(
            &self,
            _challenge: &townhall_authority::ChallengeRecord,
        ) -> Result<(), townhall_authority::StoreError> {
            Ok(())
        }
        async fn load_challenge(
            &self,
            _id: &ApprovalChallengeId,
        ) -> Result<Option<townhall_authority::ChallengeRecord>, townhall_authority::StoreError>
        {
            Ok(None)
        }
        async fn record_failed_attempt(
            &self,
            _id: &ApprovalChallengeId,
            _now_ms: u64,
        ) -> Result<(u8, townhall_authority::ChallengeStatus), townhall_authority::StoreError>
        {
            unreachable!("this store never holds a challenge")
        }
        async fn settle_with_grant(
            &self,
            _id: &ApprovalChallengeId,
            _grant: &townhall_authority::DelegationRecord,
        ) -> Result<townhall_authority::Settled, townhall_authority::StoreError> {
            unreachable!("this store never holds a challenge")
        }
        async fn settle_rejected(
            &self,
            _id: &ApprovalChallengeId,
        ) -> Result<townhall_authority::Settled, townhall_authority::StoreError> {
            unreachable!("this store never holds a challenge")
        }
        async fn load_delegation(
            &self,
            id: &DelegationId,
        ) -> Result<Option<townhall_authority::DelegationRecord>, townhall_authority::StoreError>
        {
            Ok(Some(townhall_authority::DelegationRecord {
                id: id.clone(),
                grantor: PrincipalId::new("lucy"),
                subject: PrincipalId::new("lucy"),
                service: ServiceId::new("demo-council-town-hall"),
                issued_at_ms: 0,
                // Live by every column the store indexes — so the ONLY thing
                // that can refuse this is the decode itself.
                expires_at_ms: u64::MAX,
                revoked_at_ms: None,
                envelope: b"not an envelope".to_vec(),
            }))
        }
        async fn revoke_delegation(
            &self,
            _id: &DelegationId,
            _at_ms: u64,
        ) -> Result<bool, townhall_authority::StoreError> {
            Ok(false)
        }
    }

    let service = AuthorityService::new(
        CorruptStore,
        FixedEntropy::new("7312"),
        AuthorityPolicy::default(),
    );

    assert_eq!(
        service
            .resolve(&DelegationId::new("delegation-1"), NOW)
            .await,
        Err(ResolveError::Unreadable),
        "an unreadable row must not resolve to a usable grant"
    );
}

/// A grant reaches its own booking and no other.
///
/// ADR-022's concealment came from a row predicate; this is the same property
/// one layer up. A grant for Lucy's first booking must not reach her second.
#[tokio::test]
async fn a_grant_reaches_only_the_resource_it_names() {
    let service = service();
    let raised = service
        .begin(&lucys_request(), NOW)
        .await
        .expect("challenge");
    let grant = service
        .submit(
            &raised.id,
            "7312",
            &lucys_binding(),
            AssuranceLevel::SmsReply,
            NOW + 1_000,
        )
        .await
        .expect("answered");
    let at = NOW + 2_000;

    assert!(grant.permits(Behaviour::Book, &BookingId::new("sms-lucy-0001"), at));
    assert!(
        !grant.permits(Behaviour::Book, &BookingId::new("sms-lucy-0002"), at),
        "one approval must not reach a neighbouring booking"
    );
    assert!(
        !grant.permits(
            Behaviour::UpdateRequirements,
            &BookingId::new("sms-lucy-0001"),
            at
        ),
        "a behaviour nobody approved must not be permitted"
    );
    assert_eq!(
        grant.constraints().resources(),
        &[BookingId::new("sms-lucy-0001")],
        "one booking is what a one-element resource list MEANS"
    );
}

/// Marco cancels Lucy's booking: ADR-020's promise, kept by three principals.
#[tokio::test]
async fn a_delegated_grant_separates_the_owner_from_the_requester() {
    let service = service();
    let mut request = lucys_request();
    request.grantor = PrincipalId::new("lucy");
    request.subject = PrincipalId::new("marco");
    request.scope.behaviours = BehaviourSet::new([Behaviour::Cancel]);

    let raised = service.begin(&request, NOW).await.expect("challenge");
    let grant = service
        .submit(
            &raised.id,
            "7312",
            &lucys_binding(),
            AssuranceLevel::SmsReply,
            NOW + 1_000,
        )
        .await
        .expect("Lucy approves from her own channel");

    assert_eq!(
        grant.grantor().as_str(),
        "lucy",
        "the booking's owner is the grantor"
    );
    assert_eq!(
        grant.subject().as_str(),
        "marco",
        "the cancellation is attributed to Marco"
    );
    assert_eq!(
        grant.actor(),
        &ActorId::new("agent:marco"),
        "the workload presenting the grant acts for the subject"
    );
    assert!(grant.permits(
        Behaviour::Cancel,
        &BookingId::new("sms-lucy-0001"),
        NOW + 2_000
    ));
    assert!(
        !grant.permits(
            Behaviour::Book,
            &BookingId::new("sms-lucy-0001"),
            NOW + 2_000
        ),
        "a cancellation delegation must not also book"
    );
}

/// The envelope's round trip is compared against the ISSUED value.
///
/// # Why not against a hand-built grant
///
/// A hand-built expectation asserts that the codec agrees with the test's idea
/// of a grant. Comparing against what the issuer produced asserts the thing
/// that matters: a grant reloaded after a restart is the grant that was issued
/// (ADR-025).
#[tokio::test]
async fn a_reloaded_grant_equals_the_one_that_was_issued() {
    let service = service();
    let raised = service
        .begin(&lucys_request(), NOW)
        .await
        .expect("challenge");
    let issued = service
        .submit(
            &raised.id,
            "7312",
            &lucys_binding(),
            AssuranceLevel::SmsReply,
            NOW + 1_000,
        )
        .await
        .expect("answered");

    let reloaded = service
        .resolve(issued.delegation(), NOW + 2_000)
        .await
        .expect("resolvable");

    assert_eq!(reloaded, issued);
    assert_eq!(reloaded.scope_hash(), issued.scope_hash());
    assert_eq!(reloaded.assurance(), issued.assurance());
    assert_eq!(reloaded.constraints(), issued.constraints());
}

/// Two challenges over one booking do not share a code's fate.
#[tokio::test]
async fn a_second_challenge_is_its_own_challenge() {
    let service = service();
    let first = service.begin(&lucys_request(), NOW).await.expect("first");
    let second = service.begin(&lucys_request(), NOW).await.expect("second");

    assert_ne!(first.id, second.id, "each challenge needs its own identity");

    service
        .reject(&first.id, "7312", &lucys_binding(), NOW + 1_000)
        .await
        .expect("decline the first");

    service
        .submit(
            &second.id,
            "7312",
            &lucys_binding(),
            AssuranceLevel::SmsReply,
            NOW + 2_000,
        )
        .await
        .expect("declining one offer must not decline the other");
}

/// Two correct replies arrive at once; exactly one grant exists.
///
/// # Why this test exists, and what the mutation battery proved
///
/// Replay is guarded twice: the service checks the challenge's status before
/// spending an attempt, and the store checks it again inside its atomic
/// `settle_with_grant`. Removing EITHER one left all twenty sequential tests
/// passing — each is sufficient on its own when the replies are ordered.
///
/// Only the store's check holds when they are not. This is the sole witness for
/// it: without atomicity both threads see `Pending`, both settle, and one
/// challenge yields two grants — spec §17's "one challenge -> at most one
/// grant", broken by a race that no sequential test can see.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn two_simultaneous_correct_replies_yield_exactly_one_grant() {
    use std::sync::Arc;
    use std::sync::atomic::{AtomicUsize, Ordering};

    // Repeated, because a race that loses by scheduler luck once will not lose
    // every time. Deterministic interleaving would need loom, and loom cannot
    // reach a `&dyn` port behind a generic — recorded as the reason this is a
    // repeat-count test rather than an exhaustive one.
    for round in 0..64 {
        let service = Arc::new(service());
        let raised = service
            .begin(&lucys_request(), NOW)
            .await
            .expect("challenge");
        let granted = Arc::new(AtomicUsize::new(0));
        let replayed = Arc::new(AtomicUsize::new(0));

        let barrier = Arc::new(std::sync::Barrier::new(2));
        let handles: Vec<_> = (0..2)
            .map(|_| {
                let service = Arc::clone(&service);
                let id = raised.id.clone();
                let granted = Arc::clone(&granted);
                let replayed = Arc::clone(&replayed);
                let barrier = Arc::clone(&barrier);
                tokio::spawn(async move {
                    barrier.wait();
                    match service
                        .submit(
                            &id,
                            "7312",
                            &lucys_binding(),
                            AssuranceLevel::SmsReply,
                            NOW + 1_000,
                        )
                        .await
                    {
                        Ok(_) => granted.fetch_add(1, Ordering::SeqCst),
                        Err(ApprovalDenied::Replay(_)) => replayed.fetch_add(1, Ordering::SeqCst),
                        Err(other) => panic!("neither granted nor replayed: {other}"),
                    };
                })
            })
            .collect();
        for handle in handles {
            handle.await.expect("no task panicked");
        }

        assert_eq!(
            granted.load(Ordering::SeqCst),
            1,
            "round {round}: one challenge must yield exactly one grant"
        );
        assert_eq!(
            replayed.load(Ordering::SeqCst),
            1,
            "round {round}: the reply that lost must be refused as a replay"
        );
    }
}
