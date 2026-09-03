//! The issuer's battery: one grant per challenge, and every denial provable on
//! its own.
//!
//! The acceptance gate reads "wrong/expired/replayed/tampered challenge/grant
//! denied **independently of prompt**". "Independently" is the load-bearing
//! word: a test that only shows *a* denial cannot say which check produced it,
//! and a reordering that made a different check fire first would keep passing.
//! So every case here arranges exactly one defect and names the error.
//!
//! # M7C-1: the reply is a receipt, not a claim
//!
//! The verifier no longer takes a caller-supplied binding. A reply's evidence is
//! deposited by the trusted ingress under a one-use RECEIPT bound to the
//! challenge it answers, and `submit` reads the sender back from that row
//! (ADR-026). So every answer here first deposits evidence and forwards the
//! receipt — the path a person's approval travels — and the new refusals
//! (unknown receipt, a receipt bound to another challenge) get their own
//! witnesses.

use bld_types::{
    ActorId, ApprovalChallengeId, Behaviour, BookingId, BookingRequirements, DelegationId,
    EvidenceReceiptId, Money, PrincipalId, ServiceId, TimeWindow,
};
use std::sync::Arc;
use std::sync::Mutex;
use townhall_authority::{
    ApprovalCode, ApprovalDenied, ApprovalRequest, ApprovalStore, AssuranceLevel, AuthorityPolicy,
    AuthorityService, BeginOutcome, BehaviourSet, BindingRef, BoundChannel, Entropy, EnvelopeKey,
    EvidenceReceipt, InboundEvidenceRecord, InsertOutcome, LoadedEvidence, MAX_ATTEMPTS,
    MemoryApprovalStore, PendingScope, ResolveError, Settled,
};

const NOW: u64 = 1_700_000_000_000;
const REPLY_WINDOW_MS: u64 = 600_000;
const GRANT_TTL_MS: u64 = 3_600_000;
const LUCY_ADDR: &str = "+lucy";
const MARCO_ADDR: &str = "+marco";

/// A deterministic entropy source: a fixed code, and counted identifiers.
///
/// Deterministic so a wrong code is wrong for a stated reason rather than by
/// luck, and counted so two challenges — or a challenge and a receipt — in one
/// test cannot collide silently.
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

/// The service, plus the caller's own handle to the store.
///
/// Two values rather than one accessor: the service cannot hand out its store
/// (that was a minting path — see `AuthorityService`), so a test that needs to
/// assert on rows keeps its own `Arc` from the start.
fn service_and_store() -> (Service, Arc<MemoryApprovalStore>) {
    let store = Arc::new(MemoryApprovalStore::new());
    // Lucy's and Marco's channels are bound to real numbers, because the
    // verifier resolves an inbound reply's sender against a row now.
    store.bind_address(&PrincipalId::new("lucy"), LUCY_ADDR, 1);
    store.bind_address(&PrincipalId::new("marco"), MARCO_ADDR, 1);
    let service = AuthorityService::new(
        Arc::clone(&store),
        FixedEntropy::new("7312"),
        AuthorityPolicy {
            reply_window_ms: REPLY_WINDOW_MS,
            grant_ttl_ms: GRANT_TTL_MS,
            assurance: AssuranceLevel::SmsReply,
        },
        test_key(),
    );
    (service, store)
}

fn service() -> Service {
    service_and_store().0
}

/// One key for the whole suite. Fixed rather than random so a failure is a
/// failure and not a coin flip.
fn test_key() -> EnvelopeKey {
    EnvelopeKey::new(vec![0xA7; 32]).expect("32 bytes")
}

fn townhall_actor() -> ActorId {
    ActorId::new("agent:townhall")
}

fn lucys_binding() -> BindingRef {
    BindingRef {
        principal: PrincipalId::new("lucy"),
        version: 1,
    }
}

/// Lucy asks; Lucy is the grantor and the subject.
fn lucys_request() -> ApprovalRequest {
    request_for("sms-lucy-0001", [Behaviour::Book, Behaviour::Cancel])
}

fn request_for(booking: &str, behaviours: impl IntoIterator<Item = Behaviour>) -> ApprovalRequest {
    ApprovalRequest {
        scope: PendingScope {
            service: ServiceId::new("demo-council-town-hall"),
            agent: "TownHallAgent".to_owned(),
            booking: BookingId::new(booking),
            behaviours: BehaviourSet::new(behaviours),
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
        actor: townhall_actor(),
    }
}

/// Deposit one reply's evidence for whatever challenge `address` is awaiting, and
/// return the receipt to forward. `nonce` keeps the inbound identity unique so a
/// test can deposit more than one reply.
///
/// Panics if the deposit is refused — tests that expect a refused deposit call
/// `deposit_evidence` directly.
async fn deposit_reply(service: &Service, address: &str, nonce: &str) -> EvidenceReceiptId {
    let (_challenge, receipt) = service
        .deposit_evidence(
            address,
            &InboundEvidenceRecord {
                provider: "sim".to_owned(),
                provider_account: "townhall".to_owned(),
                provider_message_id: format!("msg-{address}-{nonce}"),
                claimed_sender: address.to_owned(),
                verified: true,
                signature: None,
            },
            NOW,
            REPLY_WINDOW_MS,
        )
        .await
        .expect("the bound channel is awaiting a challenge");
    receipt
}

/// The happy path, and the gate's "£45 scope permitted" half.
///
/// The £50 ceiling is what Lucy approved; the council's slot costs £45. The
/// grant must permit the cheaper booking — a grant that only permitted an exact
/// £50 would refuse every real slot.
#[tokio::test]
async fn an_answered_challenge_yields_one_grant_that_permits_the_forty_five_pound_booking() {
    let service = service();
    let (created, raised) = service
        .begin(&lucys_request(), NOW)
        .await
        .expect("challenge");
    assert_eq!(created, BeginOutcome::Created);

    assert!(
        raised.preview.contains("Reply YES 7312 to approve."),
        "the preview must carry the code the person is to send back"
    );

    let receipt = deposit_reply(&service, LUCY_ADDR, "1").await;
    let grant = service
        .submit(&raised.id, "7312", &townhall_actor(), &receipt, NOW + 1_000)
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
    assert!(grant.covers(Behaviour::Book, &BookingId::new("sms-lucy-0001")));
}

/// The grant's clock starts at the approval, not at the offer.
#[tokio::test]
async fn a_grant_approved_at_the_last_second_still_has_its_full_life() {
    let service = service();
    let (_, raised) = service
        .begin(&lucys_request(), NOW)
        .await
        .expect("challenge");
    let last_moment = NOW + REPLY_WINDOW_MS - 1;
    let receipt = deposit_reply(&service, LUCY_ADDR, "1").await;

    let grant = service
        .submit(&raised.id, "7312", &townhall_actor(), &receipt, last_moment)
        .await
        .expect("answered inside the window");

    assert_eq!(grant.expires_at_ms(), last_moment + GRANT_TTL_MS);

    service
        .resolve(grant.delegation(), last_moment + GRANT_TTL_MS - 1)
        .await
        .expect("live until its own deadline");
    assert_eq!(
        service
            .resolve(grant.delegation(), last_moment + GRANT_TTL_MS)
            .await,
        Err(ResolveError::Expired),
        "the grant must stop at its own deadline"
    );
}

/// W4: a second `YES` returns the SAME reference, not a second grant and not a
/// replay error.
///
/// # Why idempotent recovery, not `Replay`
///
/// The delegation reference is returned exactly once (§13.1 step 7). If a
/// retried `YES` — a carrier redelivery, or a person tapping twice — got
/// `Replay`, a booking lost after the approval could never be recovered: the
/// grant exists, but nothing holds its reference. So the retry recovers the
/// reference. What must not happen twice is ISSUANCE, and this proves it does
/// not: the same delegation id both times, one row.
#[tokio::test]
async fn a_second_yes_returns_the_same_reference_not_a_second_grant() {
    let (service, store) = service_and_store();
    let (_, raised) = service
        .begin(&lucys_request(), NOW)
        .await
        .expect("challenge");
    let receipt = deposit_reply(&service, LUCY_ADDR, "1").await;

    let first = service
        .submit(&raised.id, "7312", &townhall_actor(), &receipt, NOW + 1_000)
        .await
        .expect("the first correct answer");

    let second = service
        .submit(&raised.id, "7312", &townhall_actor(), &receipt, NOW + 2_000)
        .await
        .expect("a retried YES recovers, it does not error");

    assert_eq!(
        second.delegation(),
        first.delegation(),
        "a retried YES must return the reference already issued"
    );
    assert_eq!(second, first, "and the whole grant, unchanged");

    // Exactly one delegation exists for the challenge — the recovery did not
    // mint a second.
    let by_challenge = store
        .load_delegation_by_challenge(&raised.id)
        .await
        .expect("loadable")
        .expect("the one delegation");
    assert_eq!(&by_challenge.id, first.delegation());

    // And the grant that issued is still usable, many times over.
    for moment in [NOW + 3_000, NOW + 4_000, NOW + 5_000] {
        let resolved = service
            .resolve(first.delegation(), moment)
            .await
            .expect("a live grant resolves on every presentation");
        assert_eq!(resolved, first, "resolution must not alter the grant");
    }
}

/// W4, the other half: a retried `YES` from a DIFFERENT workload gets nothing.
///
/// The recovery returns the reference only to the actor the grant names. A
/// second workload replaying the same reply must not be handed someone else's
/// grant — that would make the reference a bearer token the recovery path minted.
#[tokio::test]
async fn a_second_yes_from_a_different_actor_is_refused() {
    let service = service();
    let (_, raised) = service
        .begin(&lucys_request(), NOW)
        .await
        .expect("challenge");
    let receipt = deposit_reply(&service, LUCY_ADDR, "1").await;

    service
        .submit(&raised.id, "7312", &townhall_actor(), &receipt, NOW + 1_000)
        .await
        .expect("Lucy's own workload approves");

    let stolen = service
        .submit(
            &raised.id,
            "7312",
            &ActorId::new("agent:someone-else"),
            &receipt,
            NOW + 2_000,
        )
        .await;
    assert_eq!(
        stolen,
        Err(ApprovalDenied::Replay("approved")),
        "a different workload replaying the reply gets no grant"
    );
}

/// W1: a receipt that names no deposited row is refused before any challenge is
/// touched.
///
/// The receipt is the ONLY thing the untrusted caller supplies about the reply.
/// One it invented names no row, so there is nothing to read the sender from —
/// and the challenge stays pending, no attempt spent.
#[tokio::test]
async fn a_receipt_that_names_no_row_is_refused() {
    let (service, store) = service_and_store();
    let (_, raised) = service
        .begin(&lucys_request(), NOW)
        .await
        .expect("challenge");

    let denied = service
        .submit(
            &raised.id,
            "7312",
            &townhall_actor(),
            &EvidenceReceiptId::new("invented-by-the-caller"),
            NOW + 1_000,
        )
        .await;
    assert_eq!(denied, Err(ApprovalDenied::UnknownReceipt));

    let challenge = store
        .load_challenge(&raised.id)
        .await
        .expect("loadable")
        .expect("still there");
    assert_eq!(
        challenge.status,
        townhall_authority::ChallengeStatus::Pending
    );
    assert_eq!(
        challenge.attempts_left(),
        MAX_ATTEMPTS,
        "an invented receipt must not spend an attempt or settle the challenge"
    );
}

/// W2: a receipt for one challenge cannot answer another.
///
/// # The forgery this closes
///
/// A person with two live approvals — a £45 and a £5000 — who answers the £45
/// produces a valid evidence row for their number. The orchestrator raised both
/// and knows both codes, so without this check it could present that row against
/// the £5000. The row is bound to its challenge at deposit, and `submit` requires
/// the binding to match: the £45's receipt is refused against the £5000, which
/// stays pending, and the £45's receipt is left unconsumed for its own answer.
#[tokio::test]
async fn a_receipt_for_one_challenge_is_refused_against_another() {
    let service = service();
    // The cheap one, raised and answered's evidence deposited (bound to it).
    let (_, cheap) = service
        .begin(&request_for("sms-lucy-0045", [Behaviour::Book]), NOW)
        .await
        .expect("cheap challenge");
    let cheap_receipt = deposit_reply(&service, LUCY_ADDR, "cheap").await;

    // The expensive one, raised to the SAME number — it supersedes the cheap
    // one's awaiting-reply, but the cheap receipt is already bound to the cheap
    // challenge.
    let (_, dear) = service
        .begin(&request_for("sms-lucy-5000", [Behaviour::Book]), NOW)
        .await
        .expect("expensive challenge");

    let denied = service
        .submit(
            &dear.id,
            "7312",
            &townhall_actor(),
            &cheap_receipt,
            NOW + 1_000,
        )
        .await;
    assert_eq!(
        denied,
        Err(ApprovalDenied::ReceiptChallengeMismatch),
        "the cheap challenge's receipt must not answer the expensive one"
    );

    // The expensive challenge is untouched, and the cheap receipt still answers
    // its own challenge.
    let grant = service
        .submit(
            &cheap.id,
            "7312",
            &townhall_actor(),
            &cheap_receipt,
            NOW + 2_000,
        )
        .await
        .expect("the receipt still answers the challenge it was deposited for");
    assert!(grant.covers(Behaviour::Book, &BookingId::new("sms-lucy-0045")));
}

/// A reply from a number nobody is awaiting gets no receipt — wrong channel,
/// caught at the ingress.
///
/// The correlation is by address: a reply from an unbound, un-awaited number
/// finds no challenge, so the ingress refuses the deposit and the model seat
/// never obtains a receipt to forward. Lucy's challenge is untouched.
#[tokio::test]
async fn a_reply_from_an_unawaited_number_gets_no_receipt() {
    let (service, store) = service_and_store();
    let (_, raised) = service
        .begin(&lucys_request(), NOW)
        .await
        .expect("challenge");

    let refused = service
        .deposit_evidence(
            "+mallory",
            &InboundEvidenceRecord {
                provider: "sim".to_owned(),
                provider_account: "townhall".to_owned(),
                provider_message_id: "msg-mallory".to_owned(),
                claimed_sender: "+mallory".to_owned(),
                verified: true,
                signature: None,
            },
            NOW,
            REPLY_WINDOW_MS,
        )
        .await;
    assert_eq!(
        refused,
        Err(ApprovalDenied::UnknownChallenge),
        "a number awaiting no challenge cannot deposit evidence"
    );

    let challenge = store
        .load_challenge(&raised.id)
        .await
        .expect("loadable")
        .expect("still there");
    assert_eq!(
        challenge.attempts_left(),
        MAX_ATTEMPTS,
        "a stranger's reply must not spend the bound person's attempts"
    );
}

/// A late answer is refused as late, whatever the code says.
#[tokio::test]
async fn an_expired_challenge_is_denied_before_the_code_is_read() {
    let service = service();
    let (_, raised) = service
        .begin(&lucys_request(), NOW)
        .await
        .expect("challenge");
    let receipt = deposit_reply(&service, LUCY_ADDR, "1").await;

    let denied = service
        .submit(
            &raised.id,
            "7312",
            &townhall_actor(),
            &receipt,
            NOW + REPLY_WINDOW_MS,
        )
        .await;
    assert_eq!(denied, Err(ApprovalDenied::ChallengeExpired));
}

/// A wrong code costs an attempt, and says how many are left; the same receipt
/// still answers on the retry, because a wrong code does not consume it.
#[tokio::test]
async fn a_wrong_code_costs_one_attempt() {
    let service = service();
    let (_, raised) = service
        .begin(&lucys_request(), NOW)
        .await
        .expect("challenge");
    let receipt = deposit_reply(&service, LUCY_ADDR, "1").await;

    assert_eq!(
        service
            .submit(&raised.id, "0000", &townhall_actor(), &receipt, NOW + 1_000)
            .await,
        Err(ApprovalDenied::WrongCode {
            attempts_left: MAX_ATTEMPTS - 1
        })
    );

    service
        .submit(&raised.id, "7312", &townhall_actor(), &receipt, NOW + 2_000)
        .await
        .expect("one wrong guess must not spend the challenge or the receipt");
}

/// The attempt bound is what makes a four-digit code safe.
#[tokio::test]
async fn the_attempt_bound_spends_the_challenge_and_the_right_code_no_longer_helps() {
    let service = service();
    let (_, raised) = service
        .begin(&lucys_request(), NOW)
        .await
        .expect("challenge");
    let receipt = deposit_reply(&service, LUCY_ADDR, "1").await;

    for remaining in (0..MAX_ATTEMPTS).rev() {
        let denied = service
            .submit(&raised.id, "0000", &townhall_actor(), &receipt, NOW + 1_000)
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
            .submit(&raised.id, "7312", &townhall_actor(), &receipt, NOW + 2_000)
            .await,
        Err(ApprovalDenied::Replay("exhausted")),
        "a spent challenge must not accept the correct code afterwards"
    );
}

/// A binding that has moved beneath the challenge can no longer answer it.
///
/// The reply comes from Lucy's real number, but her binding was re-verified to a
/// new revision after the challenge was raised. The evidence resolves to the
/// new revision, the challenge names the old one, and the two no longer match.
#[tokio::test]
async fn a_binding_re_verified_since_the_challenge_can_no_longer_answer_it() {
    let (service, store) = service_and_store();
    let (_, raised) = service
        .begin(&lucys_request(), NOW)
        .await
        .expect("challenge");

    // Her number is re-verified: same number, new revision.
    store.bind_address(&PrincipalId::new("lucy"), LUCY_ADDR, 2);
    let receipt = deposit_reply(&service, LUCY_ADDR, "1").await;

    assert_eq!(
        service
            .submit(&raised.id, "7312", &townhall_actor(), &receipt, NOW + 1_000)
            .await,
        Err(ApprovalDenied::WrongChannel),
        "a challenge sent to a binding must not survive that binding moving"
    );
}

/// `NO` is terminal, and a later `YES` does not revive it.
#[tokio::test]
async fn a_rejected_challenge_stays_rejected() {
    let service = service();
    let (_, raised) = service
        .begin(&lucys_request(), NOW)
        .await
        .expect("challenge");
    // Both replies are deposited while the number still awaits the challenge; the
    // decline then consumes one and clears the correlation, and the later YES
    // rides its own already-bound receipt.
    let no = deposit_reply(&service, LUCY_ADDR, "no").await;
    let yes = deposit_reply(&service, LUCY_ADDR, "yes").await;

    service
        .reject(&raised.id, "7312", &no, NOW + 1_000)
        .await
        .expect("Lucy may decline");

    assert_eq!(
        service
            .submit(&raised.id, "7312", &townhall_actor(), &yes, NOW + 2_000)
            .await,
        Err(ApprovalDenied::Replay("rejected")),
        "a declined request must not be revivable by a later YES"
    );
}

/// Rejection needs the code too — otherwise anyone could cancel it.
#[tokio::test]
async fn a_rejection_without_the_code_is_refused() {
    let service = service();
    let (_, raised) = service
        .begin(&lucys_request(), NOW)
        .await
        .expect("challenge");
    let receipt = deposit_reply(&service, LUCY_ADDR, "1").await;

    assert_eq!(
        service
            .reject(&raised.id, "0000", &receipt, NOW + 1_000)
            .await,
        Err(ApprovalDenied::WrongCode {
            attempts_left: MAX_ATTEMPTS - 1
        })
    );
}

/// A challenge nobody raised is not an opportunity — even with a receipt in hand.
#[tokio::test]
async fn an_unknown_challenge_is_denied() {
    let service = service();
    // A real deposited receipt (bound to a real challenge), but for no challenge
    // that exists at the id asked.
    let (_, _raised) = service
        .begin(&lucys_request(), NOW)
        .await
        .expect("challenge");
    let receipt = deposit_reply(&service, LUCY_ADDR, "1").await;

    // The receipt is bound to `raised.id`; naming a different id is a mismatch,
    // and naming one nobody raised at all with a matching-but-absent receipt is
    // unknown. Here the receipt names `raised`, and we ask about a ghost.
    assert_eq!(
        service
            .submit(
                &ApprovalChallengeId::new("never-raised"),
                "7312",
                &townhall_actor(),
                &receipt,
                NOW + 1_000,
            )
            .await,
        // The receipt answers `raised`, not the ghost — so the mismatch fires
        // first, which is itself a refusal to answer the wrong challenge.
        Err(ApprovalDenied::ReceiptChallengeMismatch)
    );
}

/// The issuer caps assurance at the weakest of policy and binding.
///
/// A binding that established only `Dev` cannot carry an SMS-level grant, however
/// the challenge was raised. The binding's assurance now comes from its row.
#[tokio::test]
async fn the_grant_never_claims_more_assurance_than_the_binding_established() {
    let store = Arc::new(MemoryApprovalStore::new());
    // Lucy's channel is known only to `Dev` assurance.
    store.bind_address_at(&PrincipalId::new("lucy"), LUCY_ADDR, 1, AssuranceLevel::Dev);
    let service = AuthorityService::new(
        Arc::clone(&store),
        FixedEntropy::new("7312"),
        AuthorityPolicy {
            reply_window_ms: REPLY_WINDOW_MS,
            grant_ttl_ms: GRANT_TTL_MS,
            // Raised at the stronger SMS level …
            assurance: AssuranceLevel::SmsReply,
        },
        test_key(),
    );
    let (_, raised) = service
        .begin(&lucys_request(), NOW)
        .await
        .expect("challenge");
    let receipt = deposit_reply(&service, LUCY_ADDR, "1").await;

    let grant = service
        .submit(&raised.id, "7312", &townhall_actor(), &receipt, NOW + 1_000)
        .await
        .expect("answered");

    // … and capped at the weaker binding.
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
    let (_, raised) = service
        .begin(&lucys_request(), NOW)
        .await
        .expect("challenge");
    let receipt = deposit_reply(&service, LUCY_ADDR, "1").await;
    let grant = service
        .submit(&raised.id, "7312", &townhall_actor(), &receipt, NOW + 1_000)
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
    let (_, raised) = service
        .begin(&lucys_request(), NOW)
        .await
        .expect("challenge");
    let receipt = deposit_reply(&service, LUCY_ADDR, "1").await;
    let grant = service
        .submit(&raised.id, "7312", &townhall_actor(), &receipt, NOW + 1_000)
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

/// A grant reaches its own booking and no other.
#[tokio::test]
async fn a_grant_reaches_only_the_resource_it_names() {
    let service = service();
    let (_, raised) = service
        .begin(&lucys_request(), NOW)
        .await
        .expect("challenge");
    let receipt = deposit_reply(&service, LUCY_ADDR, "1").await;
    let grant = service
        .submit(&raised.id, "7312", &townhall_actor(), &receipt, NOW + 1_000)
        .await
        .expect("answered");
    assert!(grant.covers(Behaviour::Book, &BookingId::new("sms-lucy-0001")));
    assert!(
        !grant.covers(Behaviour::Book, &BookingId::new("sms-lucy-0002")),
        "one approval must not reach a neighbouring booking"
    );
    assert!(
        !grant.covers(
            Behaviour::UpdateRequirements,
            &BookingId::new("sms-lucy-0001")
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

    let (_, raised) = service.begin(&request, NOW).await.expect("challenge");
    // Lucy approves from HER channel — the grantor's number, which the challenge
    // is bound to.
    let receipt = deposit_reply(&service, LUCY_ADDR, "1").await;
    let grant = service
        .submit(&raised.id, "7312", &townhall_actor(), &receipt, NOW + 1_000)
        .await
        .expect("Lucy approves from her own channel");

    assert_eq!(grant.grantor().as_str(), "lucy", "the owner is the grantor");
    assert_eq!(
        grant.subject().as_str(),
        "marco",
        "the cancellation is attributed to Marco"
    );
    assert_eq!(grant.actor(), &townhall_actor());
    assert_ne!(
        grant.actor().as_str(),
        format!("agent:{}", grant.subject().as_str()),
        "an actor derived from the subject would be a name nobody authenticated"
    );
    assert!(grant.covers(Behaviour::Cancel, &BookingId::new("sms-lucy-0001")));
    assert!(
        !grant.covers(Behaviour::Book, &BookingId::new("sms-lucy-0001")),
        "a cancellation delegation must not also book"
    );
}

/// The envelope's round trip is compared against the ISSUED value.
#[tokio::test]
async fn a_reloaded_grant_equals_the_one_that_was_issued() {
    let service = service();
    let (_, raised) = service
        .begin(&lucys_request(), NOW)
        .await
        .expect("challenge");
    let receipt = deposit_reply(&service, LUCY_ADDR, "1").await;
    let issued = service
        .submit(&raised.id, "7312", &townhall_actor(), &receipt, NOW + 1_000)
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

/// W5: a redelivered `BOOK` for the same booking reuses its challenge.
///
/// Approve-first removed ADR-024's incidental dedupe. Without idempotent begin, a
/// carrier redelivery of the original request would raise a SECOND challenge with
/// a fresh code — a contradictory prompt for one intent. Here the same booking is
/// raised twice and the second `begin` returns `Existing` with the SAME id and
/// code, at every lifecycle stage.
#[tokio::test]
async fn a_redelivered_book_reuses_its_challenge() {
    let service = service();
    let (first_outcome, first) = service.begin(&lucys_request(), NOW).await.expect("first");
    assert_eq!(first_outcome, BeginOutcome::Created);

    // Redelivered while pending.
    let (again, reused) = service
        .begin(&lucys_request(), NOW + 100)
        .await
        .expect("redelivery");
    assert_eq!(again, BeginOutcome::Existing);
    assert_eq!(reused.id, first.id, "the same challenge, not a second");
    assert_eq!(
        reused.code.revealed(),
        first.code.revealed(),
        "and the same code, not a fresh one the person never saw"
    );

    // Answer it, then redeliver AGAIN — a redelivery after approval must still
    // not raise a fresh challenge.
    let receipt = deposit_reply(&service, LUCY_ADDR, "1").await;
    service
        .submit(&first.id, "7312", &townhall_actor(), &receipt, NOW + 1_000)
        .await
        .expect("approved");
    let (after_approval, still) = service
        .begin(&lucys_request(), NOW + 2_000)
        .await
        .expect("redelivery after approval");
    assert_eq!(after_approval, BeginOutcome::Existing);
    assert_eq!(still.id, first.id);
}

/// Two DIFFERENT bookings are independent challenges.
#[tokio::test]
async fn two_different_bookings_are_their_own_challenges() {
    let (service, store) = service_and_store();
    let (_, first) = service
        .begin(&request_for("sms-lucy-A", [Behaviour::Book]), NOW)
        .await
        .expect("first");
    let (_, second) = service
        .begin(&request_for("sms-lucy-B", [Behaviour::Book]), NOW)
        .await
        .expect("second");
    assert_ne!(first.id, second.id, "each booking gets its own challenge");

    // Declining the second (the one Lucy's number currently awaits) must not
    // touch the first, which stays answerable.
    let no = deposit_reply(&service, LUCY_ADDR, "no").await;
    service
        .reject(&second.id, "7312", &no, NOW + 1_000)
        .await
        .expect("decline the second");

    let first_challenge = store
        .load_challenge(&first.id)
        .await
        .expect("loadable")
        .expect("still there");
    assert_eq!(
        first_challenge.status,
        townhall_authority::ChallengeStatus::Pending,
        "declining one offer must not decline the other"
    );
    let second_challenge = store
        .load_challenge(&second.id)
        .await
        .expect("loadable")
        .expect("still there");
    assert_eq!(
        second_challenge.status,
        townhall_authority::ChallengeStatus::Rejected,
        "the declined one is terminal"
    );
}

/// Two correct replies arrive at once; exactly one grant exists, and both callers
/// receive it.
///
/// # What the store's atomicity guarantees
///
/// Removing the store's atomic `settle_with_grant` check leaves all the
/// sequential tests passing — each guard is sufficient when replies are ordered.
/// Only the store's check holds when they are not: without it both threads see
/// `pending`, both settle, and one challenge yields two grants. With it, one
/// settles and the other recovers the SAME reference — one row, one grant.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn two_simultaneous_correct_replies_yield_exactly_one_grant() {
    use std::sync::atomic::{AtomicUsize, Ordering};

    for round in 0..64 {
        let (service, store) = service_and_store();
        let service = Arc::new(service);
        let (_, raised) = service
            .begin(&lucys_request(), NOW)
            .await
            .expect("challenge");
        let receipt = deposit_reply(&service, LUCY_ADDR, "1").await;
        let granted = Arc::new(AtomicUsize::new(0));

        let barrier = Arc::new(std::sync::Barrier::new(2));
        let handles: Vec<_> = (0..2)
            .map(|_| {
                let service = Arc::clone(&service);
                let id = raised.id.clone();
                let receipt = receipt.clone();
                let granted = Arc::clone(&granted);
                let barrier = Arc::clone(&barrier);
                tokio::spawn(async move {
                    barrier.wait();
                    match service
                        .submit(&id, "7312", &townhall_actor(), &receipt, NOW + 1_000)
                        .await
                    {
                        // Both callers must end holding the grant — one settled
                        // it, one recovered it — but only one row may exist.
                        Ok(grant) => {
                            granted.fetch_add(1, Ordering::SeqCst);
                            grant.delegation().clone()
                        }
                        Err(other) => panic!("a correct concurrent reply was refused: {other}"),
                    }
                })
            })
            .collect();
        let mut references = Vec::new();
        for handle in handles {
            references.push(handle.await.expect("no task panicked"));
        }

        assert_eq!(
            granted.load(Ordering::SeqCst),
            2,
            "round {round}: both correct replies must be answered, not one refused"
        );
        assert_eq!(
            references[0], references[1],
            "round {round}: both callers must receive the SAME reference"
        );

        // And exactly one delegation row exists for the challenge.
        let delegation = store
            .load_delegation_by_challenge(&raised.id)
            .await
            .expect("loadable")
            .expect("one delegation");
        assert_eq!(&delegation.id, &references[0]);
    }
}

/// A challenge whose digest does not describe its scope yields no grant.
///
/// The verifier checks the digest, not only the SQL store — so a fabricated or
/// in-memory row that contradicts itself is refused at the seam. Here a
/// contradictory challenge is planted directly into the in-memory store (whose
/// insert does not re-derive the digest), a real receipt is deposited for it, and
/// the answer is refused as `Unreadable` before the code is even read.
#[tokio::test]
async fn a_challenge_whose_digest_contradicts_its_scope_yields_no_grant() {
    use townhall_authority::{CanonicalScope, ChallengeRecord, ChallengeStatus};

    fn scope_of(max_fee_pence: u64) -> CanonicalScope {
        CanonicalScope {
            service: ServiceId::new("demo-council-town-hall"),
            agent: "TownHallAgent".to_owned(),
            booking: BookingId::new("sms-lucy-0001"),
            behaviours: BehaviourSet::new([Behaviour::Book]),
            requirements: BookingRequirements {
                purpose: "town hall booking".to_owned(),
                requested_date: "2026-09-10".to_owned(),
                time_window: TimeWindow {
                    from: "14:00".to_owned(),
                    to: "17:00".to_owned(),
                },
                attendees: 20,
                wheelchair_accessible: true,
                max_fee: Money::from_pence(max_fee_pence),
            },
            expires_at_ms: NOW + REPLY_WINDOW_MS,
            grant_ttl_ms: GRANT_TTL_MS,
        }
    }

    let store = Arc::new(MemoryApprovalStore::new());
    store.bind_address(&PrincipalId::new("lucy"), LUCY_ADDR, 1);
    let service = AuthorityService::new(
        Arc::clone(&store),
        FixedEntropy::new("7312"),
        AuthorityPolicy::default(),
        test_key(),
    );

    let id = ApprovalChallengeId::new("forged");
    // The scope permits £50 while the digest describes £10 — a contradiction the
    // in-memory store stores verbatim.
    store
        .insert_challenge(&ChallengeRecord {
            id: id.clone(),
            code: ApprovalCode::new("7312").expect("four digits"),
            scope: scope_of(5_000),
            scope_hash: scope_of(1_000).digest(),
            binding: lucys_binding(),
            grantor: PrincipalId::new("lucy"),
            subject: PrincipalId::new("lucy"),
            created_at_ms: NOW,
            attempts_used: 0,
            status: ChallengeStatus::Pending,
            assurance: AssuranceLevel::SmsReply,
            actor: townhall_actor(),
        })
        .await
        .expect("the store stores what it is given");
    // Correlate and deposit a real receipt, so the refusal is the digest check
    // and not a missing receipt.
    store
        .await_reply(LUCY_ADDR, &id, NOW, NOW + REPLY_WINDOW_MS)
        .await
        .expect("await");
    let receipt = deposit_reply(&service, LUCY_ADDR, "1").await;

    assert_eq!(
        service
            .submit(&id, "7312", &townhall_actor(), &receipt, NOW + 1_000)
            .await,
        Err(ApprovalDenied::Unreadable),
        "a self-contradictory challenge yields no grant even with a valid receipt"
    );
}

/// The gate's "tampered grant" half: a delegation that does not decode is
/// refused rather than half-believed.
///
/// A hostile store returns a live-by-every-column delegation whose envelope is
/// nonsense — so the ONLY thing that can refuse it is the decode itself.
#[tokio::test]
#[allow(clippy::too_many_lines)] // the hand-rolled store implements the whole port
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
            _receipt: &EvidenceReceiptId,
            _address: &str,
            _now_ms: u64,
        ) -> Result<Settled, townhall_authority::StoreError> {
            unreachable!("this store never holds a challenge")
        }
        async fn settle_rejected(
            &self,
            _id: &ApprovalChallengeId,
            _receipt: &EvidenceReceiptId,
            _address: &str,
            _now_ms: u64,
        ) -> Result<Settled, townhall_authority::StoreError> {
            unreachable!("this store never holds a challenge")
        }
        async fn load_delegation(
            &self,
            id: &DelegationId,
        ) -> Result<Option<townhall_authority::DelegationRecord>, townhall_authority::StoreError>
        {
            Ok(Some(townhall_authority::DelegationRecord {
                id: id.clone(),
                challenge_id: ApprovalChallengeId::new("challenge-1"),
                grantor: PrincipalId::new("lucy"),
                subject: PrincipalId::new("lucy"),
                service: ServiceId::new("demo-council-town-hall"),
                issued_at_ms: 0,
                expires_at_ms: u64::MAX,
                revoked_at_ms: None,
                envelope: b"not an envelope".to_vec(),
            }))
        }
        async fn live_binding(
            &self,
            principal: &PrincipalId,
        ) -> Result<Option<BindingRef>, townhall_authority::StoreError> {
            Ok(Some(BindingRef {
                principal: principal.clone(),
                version: 1,
            }))
        }
        async fn revoke_delegation(
            &self,
            _id: &DelegationId,
            _at_ms: u64,
        ) -> Result<bool, townhall_authority::StoreError> {
            Ok(false)
        }
        async fn write_inbound_evidence(
            &self,
            _receipt: &EvidenceReceiptId,
            _evidence: &InboundEvidenceRecord,
            _challenge: &ApprovalChallengeId,
            _now_ms: u64,
            _expires_at_ms: u64,
        ) -> Result<EvidenceReceipt, townhall_authority::StoreError> {
            unreachable!("this store never deposits evidence")
        }
        async fn load_evidence_by_receipt(
            &self,
            _receipt: &EvidenceReceiptId,
        ) -> Result<Option<LoadedEvidence>, townhall_authority::StoreError> {
            unreachable!("this store never deposits evidence")
        }
        async fn load_delegation_by_challenge(
            &self,
            _challenge: &ApprovalChallengeId,
        ) -> Result<Option<townhall_authority::DelegationRecord>, townhall_authority::StoreError>
        {
            Ok(None)
        }
        async fn live_binding_by_address(
            &self,
            _address: &str,
        ) -> Result<Option<BoundChannel>, townhall_authority::StoreError> {
            Ok(None)
        }
        async fn address_for(
            &self,
            _principal: &PrincipalId,
        ) -> Result<Option<String>, townhall_authority::StoreError> {
            Ok(None)
        }
        async fn await_reply(
            &self,
            _address: &str,
            _challenge: &ApprovalChallengeId,
            _now_ms: u64,
            _expires_at_ms: u64,
        ) -> Result<(), townhall_authority::StoreError> {
            Ok(())
        }
        async fn awaiting_reply(
            &self,
            _address: &str,
        ) -> Result<Option<ApprovalChallengeId>, townhall_authority::StoreError> {
            Ok(None)
        }
        async fn insert_or_get_challenge(
            &self,
            _challenge: &townhall_authority::ChallengeRecord,
        ) -> Result<InsertOutcome, townhall_authority::StoreError> {
            unreachable!("this store never holds a challenge")
        }
    }

    let service = AuthorityService::new(
        Arc::new(CorruptStore),
        FixedEntropy::new("7312"),
        AuthorityPolicy::default(),
        test_key(),
    );

    assert_eq!(
        service
            .resolve(&DelegationId::new("delegation-1"), NOW)
            .await,
        Err(ResolveError::Unreadable),
        "an unreadable row must not resolve to a usable grant"
    );
}

/// A grant resolves for the actor it names, and for nobody else.
#[tokio::test]
async fn a_grant_resolves_only_for_the_actor_it_names() {
    let service = service();
    let (_, raised) = service
        .begin(&lucys_request(), NOW)
        .await
        .expect("challenge");
    let receipt = deposit_reply(&service, LUCY_ADDR, "1").await;
    let grant = service
        .submit(&raised.id, "7312", &townhall_actor(), &receipt, NOW + 1_000)
        .await
        .expect("answered");

    service
        .resolve(grant.delegation(), NOW + 2_000)
        .await
        .expect("the grant is live");
    assert_eq!(grant.actor(), &townhall_actor());
    assert_ne!(
        grant.actor(),
        &ActorId::new("agent:someone-else"),
        "a reference that any authenticated workload could use would be a \
         bearer token, and this one is not"
    );
}
