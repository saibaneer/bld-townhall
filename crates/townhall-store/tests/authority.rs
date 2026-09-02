//! The authority ports over real SQLite.
//!
//! # Why this file exists when `townhall-authority` already has 41 tests
//!
//! Those run against `MemoryApprovalStore`, whose atomicity is one `Mutex`. The
//! property that matters in production is a different one: that a conditional
//! `UPDATE` inside a transaction serialises two concurrent approvals, and that
//! every column survives a round trip through a schema. Neither is witnessed by
//! a `HashMap`.
//!
//! Migration 0006 is exercised by every test here simply by opening the store.

use bld_types::{
    Behaviour, BookingId, BookingRequirements, DelegationId, Money, PrincipalId, ServiceId,
    TimeWindow,
};
use std::sync::Mutex;
use townhall_authority::{
    ApprovalCode, ApprovalDenied, ApprovalRequest, ApprovalStore, AssuranceLevel, AuthorityPolicy,
    AuthorityService, BehaviourSet, BindingRef, Entropy, MAX_ATTEMPTS, PendingScope, ResolveError,
};
use townhall_store::SqliteBookingRepository;
use townhall_store::authority::{ChannelBinding, SqlApprovalStore};

const NOW: u64 = 1_700_000_000_000;
const REPLY_WINDOW_MS: u64 = 600_000;
const GRANT_TTL_MS: u64 = 3_600_000;

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
        ApprovalCode::new(self.code.clone()).expect("well formed")
    }

    fn identifier(&self) -> String {
        let mut next = self.next.lock().expect("uncontended");
        *next += 1;
        format!("sql-id-{next:04}")
    }
}

/// A store over a fresh temporary database, migrations applied.
async fn store() -> (SqlApprovalStore, tempfile::TempDir) {
    let directory = tempfile::tempdir().expect("a temporary directory");
    let path = directory.path().join("authority.db");
    let bookings = SqliteBookingRepository::open(&path)
        .await
        .expect("migrations apply");
    (SqlApprovalStore::new(bookings.pool().clone()), directory)
}

fn service(store: SqlApprovalStore) -> AuthorityService<SqlApprovalStore, FixedEntropy> {
    AuthorityService::new(
        store,
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

fn request() -> ApprovalRequest {
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
        subject: PrincipalId::new("marco"),
    }
}

/// Every field survives the schema, compared against the ISSUED grant.
#[tokio::test]
async fn a_grant_round_trips_through_sqlite_unchanged() {
    let (store, _directory) = store().await;
    let service = service(store);

    let raised = service.begin(&request(), NOW).await.expect("challenge");
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
    assert_eq!(reloaded.grantor().as_str(), "lucy");
    assert_eq!(reloaded.subject().as_str(), "marco");
    assert_eq!(reloaded.scope_hash(), issued.scope_hash());
    assert_eq!(reloaded.constraints(), issued.constraints());
    assert_eq!(reloaded.assurance(), AssuranceLevel::SmsReply);
}

/// The challenge's scope survives as DATA, so approval can resume after a
/// restart without conversational memory (spec §2).
#[tokio::test]
async fn a_challenges_scope_survives_a_reopened_database() {
    let directory = tempfile::tempdir().expect("a temporary directory");
    let path = directory.path().join("authority.db");

    let raised_id = {
        let bookings = SqliteBookingRepository::open(&path)
            .await
            .expect("migrations");
        let service = service(SqlApprovalStore::new(bookings.pool().clone()));
        service.begin(&request(), NOW).await.expect("challenge").id
    };

    // A different process, as far as the data is concerned.
    let bookings = SqliteBookingRepository::open(&path)
        .await
        .expect("migrations");
    let service = service(SqlApprovalStore::new(bookings.pool().clone()));

    let grant = service
        .submit(
            &raised_id,
            "7312",
            &lucys_binding(),
            AssuranceLevel::SmsReply,
            NOW + 1_000,
        )
        .await
        .expect("a challenge raised before the restart is still answerable");

    assert_eq!(
        grant.constraints().resources(),
        &[BookingId::new("sms-lucy-0001")],
        "the booking the person approved must survive the restart, or the \
         approval resumed from memory rather than from durable state"
    );
    assert_eq!(grant.max_fee(), Money::from_pence(5_000));
}

/// Two concurrent correct replies against ONE database: exactly one grant.
///
/// This is the SQL half of the memory store's concurrency test. The witness is
/// the conditional `UPDATE ... WHERE status = 'pending'` inside a transaction;
/// without it both replies would insert, and `delegations.challenge_id UNIQUE`
/// would turn the second into an error rather than a clean replay.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn two_simultaneous_replies_over_sqlite_yield_exactly_one_grant() {
    use std::sync::Arc;
    use std::sync::atomic::{AtomicUsize, Ordering};

    for round in 0..8 {
        let (store, _directory) = store().await;
        let service = Arc::new(service(store));
        let raised = service.begin(&request(), NOW).await.expect("challenge");

        let granted = Arc::new(AtomicUsize::new(0));
        let refused = Arc::new(AtomicUsize::new(0));
        let barrier = Arc::new(std::sync::Barrier::new(2));

        let handles: Vec<_> = (0..2)
            .map(|_| {
                let service = Arc::clone(&service);
                let id = raised.id.clone();
                let granted = Arc::clone(&granted);
                let refused = Arc::clone(&refused);
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
                        Err(ApprovalDenied::Replay(_)) => refused.fetch_add(1, Ordering::SeqCst),
                        Err(other) => panic!("neither granted nor refused as a replay: {other}"),
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
            "round {round}: one challenge, one grant"
        );
        assert_eq!(refused.load(Ordering::SeqCst), 1, "round {round}");
    }
}

/// The attempt counter is durable, so a restart does not hand back three fresh
/// guesses.
#[tokio::test]
async fn the_attempt_count_survives_a_reopened_database() {
    let directory = tempfile::tempdir().expect("a temporary directory");
    let path = directory.path().join("authority.db");

    let raised_id = {
        let bookings = SqliteBookingRepository::open(&path)
            .await
            .expect("migrations");
        let service = service(SqlApprovalStore::new(bookings.pool().clone()));
        let raised = service.begin(&request(), NOW).await.expect("challenge");
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
        raised.id
    };

    let bookings = SqliteBookingRepository::open(&path)
        .await
        .expect("migrations");
    let service = service(SqlApprovalStore::new(bookings.pool().clone()));

    assert_eq!(
        service
            .submit(
                &raised_id,
                "0000",
                &lucys_binding(),
                AssuranceLevel::SmsReply,
                NOW + 2_000
            )
            .await,
        Err(ApprovalDenied::WrongCode {
            attempts_left: MAX_ATTEMPTS - 2
        }),
        "a restart must not refill the attempt budget — the bound is what makes \
         a four-digit code safe"
    );
}

/// Revocation is durable and idempotent.
#[tokio::test]
async fn revocation_survives_a_reopened_database() {
    let directory = tempfile::tempdir().expect("a temporary directory");
    let path = directory.path().join("authority.db");

    let delegation = {
        let bookings = SqliteBookingRepository::open(&path)
            .await
            .expect("migrations");
        let service = service(SqlApprovalStore::new(bookings.pool().clone()));
        let raised = service.begin(&request(), NOW).await.expect("challenge");
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
        assert!(
            service
                .revoke(grant.delegation(), NOW + 2_000)
                .await
                .expect("revocable")
        );
        grant.delegation().clone()
    };

    let bookings = SqliteBookingRepository::open(&path)
        .await
        .expect("migrations");
    let service = service(SqlApprovalStore::new(bookings.pool().clone()));

    assert_eq!(
        service.resolve(&delegation, NOW + 3_000).await,
        Err(ResolveError::Revoked),
        "a revoked grant must stay revoked across a restart"
    );
    assert!(
        !service
            .revoke(&delegation, NOW + 4_000)
            .await
            .expect("idempotent"),
        "a second REVOKE is not an error"
    );
}

/// One address binds at most one live principal — enforced by the schema.
#[tokio::test]
async fn one_address_cannot_hold_two_live_bindings() {
    let (store, _directory) = store().await;

    let lucy = ChannelBinding {
        id: "binding-lucy".to_owned(),
        address: "+447700900123".to_owned(),
        principal: PrincipalId::new("lucy"),
        version: 1,
        assurance: AssuranceLevel::SmsReply,
        withdrawn: false,
    };
    store
        .bind_channel(&lucy, Some("provider: verified"), NOW)
        .await
        .expect("the first binding");

    let mallory = ChannelBinding {
        id: "binding-mallory".to_owned(),
        address: "+447700900123".to_owned(),
        principal: PrincipalId::new("mallory"),
        version: 1,
        assurance: AssuranceLevel::SmsReply,
        withdrawn: false,
    };
    assert!(
        store.bind_channel(&mallory, None, NOW).await.is_err(),
        "a second live binding for one number would make \"who texted?\" \
         resolve by row order"
    );

    let found = store
        .live_binding("+447700900123")
        .await
        .expect("queryable")
        .expect("bound");
    assert_eq!(found.principal.as_str(), "lucy");
    assert_eq!(found.reference(), lucys_binding());
}

/// A withdrawn binding is history, not an answer.
#[tokio::test]
async fn a_withdrawn_binding_does_not_answer_who_is_this_number() {
    let (store, _directory) = store().await;

    store
        .bind_channel(
            &ChannelBinding {
                id: "binding-old".to_owned(),
                address: "+447700900999".to_owned(),
                principal: PrincipalId::new("previous-owner"),
                version: 3,
                assurance: AssuranceLevel::SmsReply,
                withdrawn: true,
            },
            None,
            NOW,
        )
        .await
        .expect("history may be written");

    assert_eq!(
        store
            .live_binding("+447700900999")
            .await
            .expect("queryable"),
        None,
        "a withdrawn binding must not identify the number's current holder"
    );
}

/// Bumping a binding's revision strands the challenges bound to the old one.
///
/// The recurring defect of this project's reviews is state outliving the moment
/// it was true. This is that defect's fix, end to end over real rows.
#[tokio::test]
async fn re_verifying_a_number_strands_the_challenge_bound_to_its_old_revision() {
    let (store, _directory) = store().await;

    store
        .bind_channel(
            &ChannelBinding {
                id: "binding-lucy".to_owned(),
                address: "+447700900123".to_owned(),
                principal: PrincipalId::new("lucy"),
                version: 1,
                assurance: AssuranceLevel::SmsReply,
                withdrawn: false,
            },
            None,
            NOW,
        )
        .await
        .expect("bound");

    let service = service(store.clone());
    let raised = service.begin(&request(), NOW).await.expect("challenge");

    let bumped = store
        .bump_binding("binding-lucy", NOW + 500)
        .await
        .expect("bumpable")
        .expect("the binding exists");
    assert_eq!(bumped, 2);

    let now_current = store
        .live_binding("+447700900123")
        .await
        .expect("queryable")
        .expect("still bound")
        .reference();

    assert_eq!(
        service
            .submit(
                &raised.id,
                "7312",
                &now_current,
                AssuranceLevel::SmsReply,
                NOW + 1_000
            )
            .await,
        Err(ApprovalDenied::WrongChannel),
        "a challenge sent to a number must not be answerable after that number \
         was re-verified"
    );
}

/// A reference nobody issued resolves to nothing, from a real table.
#[tokio::test]
async fn an_unissued_reference_resolves_to_nothing_over_sqlite() {
    let (store, _directory) = store().await;
    let service = service(store);

    assert_eq!(
        service.resolve(&DelegationId::new("guessed"), NOW).await,
        Err(ResolveError::Unknown)
    );
}

/// The stored digest must describe the stored scope, or the row is refused.
///
/// # Why this is worth a test
///
/// `scope_hash` is denormalized beside `scope` so a tamper check never
/// re-derives the thing it is checking. Denormalization creates the possibility
/// of disagreement, and the honest response to disagreement is refusal —
/// choosing which copy to believe would be choosing whose version of the
/// approval stands.
#[tokio::test]
async fn a_challenge_whose_digest_contradicts_its_scope_is_refused() {
    let directory = tempfile::tempdir().expect("a temporary directory");
    let path = directory.path().join("authority.db");
    let bookings = SqliteBookingRepository::open(&path)
        .await
        .expect("migrations");
    let store = SqlApprovalStore::new(bookings.pool().clone());
    let service = service(store.clone());

    let raised = service.begin(&request(), NOW).await.expect("challenge");
    assert!(
        store
            .load_challenge(&raised.id)
            .await
            .expect("loadable")
            .is_some(),
        "the row reads back cleanly before anything is edited"
    );

    // Reach past the port on purpose: a contradiction between the two columns
    // cannot be produced through the API, and exists in production only as
    // corruption. Editing the digest is the cheaper of the two edits and the
    // one an attacker with row access would reach for, since re-deriving a
    // digest over an edited scope is the harder half.
    sqlx::query("UPDATE approval_challenges SET scope_hash = ? WHERE id = ?")
        .bind("0".repeat(64))
        .bind(raised.id.as_str())
        .execute(bookings.pool())
        .await
        .expect("the edit lands");

    let refused = store.load_challenge(&raised.id).await;
    assert!(
        refused.is_err(),
        "a challenge whose digest does not describe its scope must be refused, \
         not read as whichever column the decoder happened to trust"
    );

    // And the refusal reaches the verifier as a denial, not as an approval.
    assert!(
        service
            .submit(
                &raised.id,
                "7312",
                &lucys_binding(),
                AssuranceLevel::SmsReply,
                NOW + 1_000
            )
            .await
            .is_err(),
        "an unreadable challenge must never yield a grant"
    );
}
