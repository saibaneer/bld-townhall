//! M4's acceptance gate, and the crashes on our side of the wire.
//!
//! The scenario the whole milestone exists for (guidance §"M4 acceptance
//! gate"): the council creates the booking, the response is dropped, and
//! recovery under the SAME effect identity converges to exactly one `Booked`.
//! *"If that scenario can duplicate a booking, M4 is not complete."*
//!
//! Our side dies for real here: `bld-driver` is a separate process that aborts
//! at armed moments (see its header), and the council is the real binary. The
//! reconciliation that heals things afterwards runs in-test, against the same
//! database files the dead processes left behind — which is exactly what a
//! restart is.

use bld_types::{BookingId, EffectIntentId};
use council_client::{CouncilClient, CouncilVerifier};
use council_wire::CouncilKey;
use std::{
    io::BufRead as _,
    process::{Command, Stdio},
    sync::Arc,
};
use townhall_service::{Attended, Coordinator, Reconciliation};
use townhall_store::{BookingRepository as _, SqliteBookingRepository, StoreClock};

const KEY_HEX: &str = "0707070707070707070707070707070707070707070707070707070707070707";

/// A store clock the test can move — reconciliation cadences and effect
/// deadlines are real times, and this suite's no-sleep rule means the clock
/// moves instead of the test waiting.
#[derive(Debug)]
struct MovableClock(std::sync::atomic::AtomicI64);

impl MovableClock {
    fn now() -> i64 {
        townhall_store::SystemStoreClock.now_ms()
    }
    fn advance(&self, by_ms: i64) {
        self.0.fetch_add(by_ms, std::sync::atomic::Ordering::SeqCst);
    }
}

impl StoreClock for MovableClock {
    fn now_ms(&self) -> i64 {
        self.0.load(std::sync::atomic::Ordering::SeqCst)
    }
}

struct World {
    _dir: tempfile::TempDir,
    council_url: String,
    bld_db: std::path::PathBuf,
    council_db: std::path::PathBuf,
    /// Held so the child dies with the world.
    council: std::process::Child,
}

impl Drop for World {
    fn drop(&mut self) {
        let _ = self.council.kill();
        let _ = self.council.wait();
    }
}

/// Spawn the real council binary. `cargo` builds it first, so the test never
/// depends on build order — and the wait for readiness is the READY line.
fn spawn_council(dir: &std::path::Path) -> World {
    spawn_council_at(dir, None)
}

/// As above, with the council's own clock pinned — a restart "later".
fn spawn_council_at(dir: &std::path::Path, clock_ms: Option<i64>) -> World {
    let status = Command::new(env!("CARGO"))
        .args(["build", "-p", "mock-council", "--features", "test-faults"])
        .status()
        .expect("cargo build mock-council");
    assert!(status.success(), "the council must build");

    let binary =
        std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("../../target/debug/mock-council");
    let council_db = dir.join("council.sqlite");
    let mut command = Command::new(binary);
    command
        .arg("--db")
        .arg(&council_db)
        .args(["--key-hex", KEY_HEX, "--port", "0"])
        .stdin(Stdio::piped())
        .stdout(Stdio::piped());
    if let Some(ms) = clock_ms {
        command.args(["--clock", &ms.to_string()]);
    }
    let mut child = command.spawn().expect("spawn the council");

    let stdout = child.stdout.take().expect("piped stdout");
    let mut lines = std::io::BufReader::new(stdout).lines();
    let ready = lines.next().expect("a line").expect("readable");
    let port: u16 = ready
        .strip_prefix("READY ")
        .unwrap_or_else(|| panic!("expected READY, got {ready:?}"))
        .parse()
        .expect("a port");

    World {
        _dir: tempfile::TempDir::new().expect("unused"),
        council_url: format!("http://127.0.0.1:{port}"),
        bld_db: dir.join("townhall.sqlite"),
        council_db,
        council: child,
    }
}

/// Run the driver against the world and wait for it to exit — cleanly or by
/// its own abort, which is the point.
fn run_driver(world: &World, booking_id: &str, die: &str) -> std::process::ExitStatus {
    Command::new(env!("CARGO_BIN_EXE_bld_driver"))
        .arg("--db")
        .arg(&world.bld_db)
        .args(["--council-url", &world.council_url])
        .args(["--key-hex", KEY_HEX])
        .args(["--booking-id", booking_id])
        .args(["--die", die])
        .status()
        .expect("run bld-driver")
}

/// The reconciler, opened over whatever the dead processes left behind — with a
/// clock the test can move past cadences and deadlines.
async fn reconciler_over(
    world: &World,
) -> (
    Reconciliation<
        SqliteBookingRepository,
        CouncilClient,
        CouncilVerifier,
        CouncilClient,
        CouncilClient,
    >,
    Arc<SqliteBookingRepository>,
    Arc<MovableClock>,
) {
    let clock = Arc::new(MovableClock(std::sync::atomic::AtomicI64::new(
        MovableClock::now(),
    )));
    let repo = Arc::new(
        SqliteBookingRepository::open_with(
            &world.bld_db,
            townhall_store::DEFAULT_EFFECT_TTL_MS,
            Arc::clone(&clock) as Arc<dyn StoreClock>,
        )
        .await
        .expect("reopen the repository — a restart"),
    );

    let key = || {
        CouncilKey::new(
            council_wire::CouncilSigner::new(council_wire::CouncilSigningKey::from_bytes(
                &key_bytes(),
            ))
            .verifying_key(),
        )
    };
    let client = || CouncilClient::new(&world.council_url, key());
    let reconciliation = Reconciliation::new(
        Arc::new(Coordinator::new(
            Arc::clone(&repo),
            Arc::new(client()),
            Arc::new(CouncilVerifier::new(key())),
            Arc::new(client()),
        )),
        Arc::new(client()),
    );
    (reconciliation, repo, clock)
}

fn key_bytes() -> [u8; 32] {
    [7u8; 32]
}

async fn council_bookings(world: &World) -> i64 {
    use sqlx::Row as _;
    let pool = sqlx::sqlite::SqlitePoolOptions::new()
        .max_connections(1)
        .connect_with(
            sqlx::sqlite::SqliteConnectOptions::new()
                .filename(&world.council_db)
                .read_only(true),
        )
        .await
        .expect("open the council's database read-only");
    sqlx::query("SELECT COUNT(*) AS n FROM bookings")
        .fetch_one(&pool)
        .await
        .expect("count")
        .get("n")
}

async fn council_knows(world: &World, effect: &str) -> bool {
    use sqlx::Row as _;
    let pool = sqlx::sqlite::SqlitePoolOptions::new()
        .max_connections(1)
        .connect_with(
            sqlx::sqlite::SqliteConnectOptions::new()
                .filename(&world.council_db)
                .read_only(true),
        )
        .await
        .expect("open read-only");
    let n: i64 = sqlx::query("SELECT COUNT(*) AS n FROM effects WHERE effect_intent_id = ?")
        .bind(effect)
        .fetch_one(&pool)
        .await
        .expect("count")
        .get("n");
    n > 0
}

// ------------------------------------------------------------ the acceptance gate

/// THE scenario: the council books the room, the answer is eaten, our process
/// exits with an unresolved turn — and reconciliation under the same identity
/// converges to exactly one Booked.
#[tokio::test]
async fn the_dropped_response_converges_to_exactly_one_booking() {
    let dir = tempfile::tempdir().expect("tempdir");
    let world = spawn_council(dir.path());

    // Arm the drop for the booking's own effect identity — scoped, so nothing
    // else's request can steal it.
    let id = BookingId::new("BKG-ACCEPT");
    let effect = townhall_service::effect_identity_for(
        &id,
        townhall_domain::OperationKind::Book,
        2, // Book departs from version 2: create(0) -> select(1) -> verify(2)
    );
    let armed: serde_json::Value = reqwest::Client::new()
        .post(format!("{}/test/faults", world.council_url))
        .json(&serde_json::json!({
            "effect_intent_id": effect.as_str(),
            "route": "create",
            "fault": "drop_response",
        }))
        .send()
        .await
        .expect("arm")
        .json()
        .await
        .expect("json");
    let fault_id = armed["fault_id"].as_u64().expect("id");

    // Our process runs the whole turn and exits cleanly with Unresolved: the
    // council answered, the wire ate it.
    let status = run_driver(&world, "BKG-ACCEPT", "never");
    assert!(status.success(), "the driver's turn ends, unresolved");

    // The fault fired — asserted, not inferred — and the council holds the room.
    let consumed: serde_json::Value = reqwest::Client::new()
        .get(format!("{}/test/faults/{fault_id}", world.council_url))
        .send()
        .await
        .expect("status")
        .json()
        .await
        .expect("json");
    assert_eq!(consumed["consumed"], 1);
    assert_eq!(council_bookings(&world).await, 1, "the room IS booked");

    // Recovery: a fresh process (this test) reconciles. One turn is enough —
    // the council recognises the identity and reports the booking it made.
    let (reconciliation, repo, clock) = reconciler_over(&world).await;
    clock.advance(60_000); // past the retry cadence, on our side only
    let due = reconciliation.due(10).await.expect("due");
    assert_eq!(due, vec![effect.clone()], "recovery FINDS its own work");
    let attended = reconciliation.attend(&effect).await.expect("attend");
    assert_eq!(attended, Attended::Settled);

    let healed = repo.load(&id).await.expect("load");
    assert_eq!(healed.state.name(), "Booked");
    assert_eq!(
        council_bookings(&world).await,
        1,
        "EXACTLY one booking — if this is 2, M4 is not complete"
    );
}

/// Test 1, first half: our process dies AFTER the intent commits and BEFORE one
/// byte reaches the council — and recovery FINISHES THE JOB (ADR-020, the
/// owner's decision of 2026-08-25). The intent is Lucy's durable authorization;
/// an outbox that never sends is not an outbox. Recovery queries, receives the
/// council's authenticated "nothing yet", and resends under the same identity.
#[tokio::test]
async fn a_crash_before_the_call_is_finished_by_recovery() {
    let dir = tempfile::tempdir().expect("tempdir");
    let world = spawn_council(dir.path());

    let status = run_driver(&world, "BKG-DIES-EARLY", "before-call");
    assert!(!status.success(), "the driver aborted, as armed");

    let id = BookingId::new("BKG-DIES-EARLY");
    let effect =
        townhall_service::effect_identity_for(&id, townhall_domain::OperationKind::Book, 2);

    // The crash state, asserted from the two databases BEFORE recovery runs:
    // the intent is durable, and the provider has nothing — read from its
    // file, not through an endpoint that would bind the identity on first
    // sight.
    let (reconciliation, repo, clock) = reconciler_over(&world).await;
    let intent = repo
        .load_effect(&effect)
        .await
        .expect("the intent is durable");
    assert_eq!(intent.effect_intent_id, effect);
    assert!(!council_knows(&world, effect.as_str()).await);

    // Recovery: query → the council's signed, identity-bound "nothing yet" →
    // resend the persisted plan under the same identity → Booked. One turn.
    clock.advance(60_000);
    let attended = reconciliation.attend(&effect).await.expect("attend");
    assert_eq!(
        attended,
        Attended::Settled,
        "recovery completes what the state still wants"
    );
    let healed = repo.load(&id).await.expect("load");
    assert_eq!(healed.state.name(), "Booked");
    assert_eq!(
        council_bookings(&world).await,
        1,
        "exactly one booking — finished, not duplicated"
    );
}

/// Test 1, second half — the ADR-016 case, preserved: the same crash, but the
/// deadline passes before any recovery runs. The council tombstones the
/// identity, the resend never happens (there is nothing to authorize it: the
/// answer is definitive absence, not "not yet"), and the booking fails closed
/// to re-proposable. "It failed, try again" is literally true.
#[tokio::test]
async fn a_crash_before_the_call_that_outlives_its_deadline_fails_closed() {
    let dir = tempfile::tempdir().expect("tempdir");
    let world = spawn_council(dir.path());

    let status = run_driver(&world, "BKG-DIES-UNSEEN", "before-call");
    assert!(!status.success(), "the driver aborted, as armed");

    let id = BookingId::new("BKG-DIES-UNSEEN");
    let effect =
        townhall_service::effect_identity_for(&id, townhall_domain::OperationKind::Book, 2);
    assert!(!council_knows(&world, effect.as_str()).await);

    // The deadline is the COUNCIL's to judge, on the COUNCIL's clock (ADR-016)
    // — advancing our store clock moves only our cadence, never its verdict. So
    // "later" is a council restart with its clock ahead, which is what actually
    // happens when real time passes. (An earlier draft advanced our clock and
    // expected absence: the council rightly kept answering "not yet", which is
    // ADR-016 §2 refusing to let OUR clock manufacture absence.)
    drop(world);
    let world = spawn_council_at(dir.path(), Some(MovableClock::now() + 600_000));
    let (reconciliation, repo, clock) = reconciler_over(&world).await;
    clock.advance(townhall_store::MAX_CADENCE_MS + townhall_store::DEFAULT_EFFECT_TTL_MS);
    let attended = reconciliation.attend(&effect).await.expect("attend");
    assert_eq!(attended, Attended::Settled);
    let healed = repo.load(&id).await.expect("load");
    assert_eq!(healed.state.name(), "AwaitingBooking");
    assert_eq!(council_bookings(&world).await, 0, "still nothing out there");
}

/// Test 5: our process dies AFTER the council commits and BEFORE the evidence
/// lands locally. The two records disagree — and reconciliation under the same
/// identity adopts the booking that already exists, rather than making another.
#[tokio::test]
async fn a_crash_after_the_councils_commit_adopts_rather_than_duplicates() {
    let dir = tempfile::tempdir().expect("tempdir");
    let world = spawn_council(dir.path());

    let status = run_driver(&world, "BKG-DIES-LATE", "after-call");
    assert!(!status.success(), "the driver aborted, as armed");
    assert_eq!(
        council_bookings(&world).await,
        1,
        "the council committed before we died"
    );

    let id = BookingId::new("BKG-DIES-LATE");
    let effect =
        townhall_service::effect_identity_for(&id, townhall_domain::OperationKind::Book, 2);

    let (reconciliation, repo, clock) = reconciler_over(&world).await;
    clock.advance(60_000);
    let attended = reconciliation.attend(&effect).await.expect("attend");
    assert_eq!(attended, Attended::Settled);

    let healed = repo.load(&id).await.expect("load");
    assert_eq!(healed.state.name(), "Booked");
    assert_eq!(council_bookings(&world).await, 1, "adopted, not duplicated");

    // Gate M1 — budget honesty across the crash. The dead process's attempt
    // STARTED (durably, before the wire) and never finished; recovery's
    // attempt did both. The columns must say exactly that: an implementation
    // that only counts after the wire reads 1/1 here, and one that never
    // writes `attempts_finished` reads 2/0.
    let (started, finished) = sqlx::query_as::<_, (i64, i64)>(
        "SELECT attempts_started, attempts_finished FROM effect_intents \
         WHERE effect_intent_id = ?",
    )
    .bind(effect.as_str())
    .fetch_one(repo.pool())
    .await
    .expect("the intent row");
    assert_eq!(
        (started, finished),
        (2, 1),
        "two conversations begun, one returned control — the crash is IN the ledger"
    );
}

/// Test 7: the council is UNREACHABLE — not slow, not garbled; gone. An
/// unreachable provider says NOTHING about whether the booking exists, so the
/// workflow must stay in flight and keep asking — an implementation that maps
/// "connection refused" to "nothing exists out there" books Lucy's room twice
/// the moment the council comes back.
#[tokio::test]
async fn an_unreachable_council_leaves_the_booking_unknown_and_in_flight() {
    let dir = tempfile::tempdir().expect("tempdir");
    let mut world = spawn_council(dir.path());

    // The dropped-response scenario first, so the intent is genuinely
    // unsettled: the council booked the room and nobody heard.
    let id = BookingId::new("BKG-OUTAGE");
    let effect =
        townhall_service::effect_identity_for(&id, townhall_domain::OperationKind::Book, 2);
    reqwest::Client::new()
        .post(format!("{}/test/faults", world.council_url))
        .json(&serde_json::json!({
            "effect_intent_id": effect.as_str(),
            "route": "create",
            "fault": "drop_response",
        }))
        .send()
        .await
        .expect("arm the drop");
    let status = run_driver(&world, "BKG-OUTAGE", "never");
    assert!(status.success());

    // Now the council is GONE — a real dead socket, not an armed answer.
    let _ = world.council.kill();
    let _ = world.council.wait();

    let (reconciliation, repo, clock) = reconciler_over(&world).await;
    clock.advance(60_000);
    assert_eq!(
        reconciliation.due(10).await.expect("due"),
        vec![effect.clone()],
        "recovery still finds its own work — due is a local question"
    );
    let attended = reconciliation.attend(&effect).await.expect("attend");
    assert_eq!(
        attended,
        Attended::StillUnknown {
            attempts_started: 2
        },
        "an unreachable council is no answer, and the attempt still spent budget"
    );

    // In flight, in both records we can still read: the state and the intent.
    let waiting = repo.load(&id).await.expect("load");
    assert_eq!(waiting.state.name(), "BookingInProgress");
    let intent = repo.load_effect(&effect).await.expect("intent");
    assert_eq!(intent.status.name(), "Unknown");

    // And the chase CONTINUES — the next turn asks again rather than deciding
    // the silence means something.
    clock.advance(60_000);
    assert_eq!(
        reconciliation.attend(&effect).await.expect("attend"),
        Attended::StillUnknown {
            attempts_started: 3
        },
        "still asking; silence never becomes a fact"
    );
}

/// Test 2c: a SIGNED answer of the wrong kind. It really is the council's, so
/// the verifier passes it — and the DOMAIN refuses it against the persisted
/// intent, with the refusal recorded. The honest protocol cannot produce this
/// answer, which is why it needs an armed fault.
#[tokio::test]
async fn a_signed_wrong_kind_answer_is_refused_by_the_domain_not_the_wire() {
    let dir = tempfile::tempdir().expect("tempdir");
    let world = spawn_council(dir.path());

    // A full, honest booking first.
    let status = run_driver(&world, "BKG-WRONGKIND", "never");
    assert!(status.success());
    let id = BookingId::new("BKG-WRONGKIND");

    // Now cancel it — the cancellation is its own effect, and we arm the
    // council to answer its resolve with a signed BookingCreated: wrong kind,
    // right signature.
    let (_, repo, clock) = reconciler_over(&world).await;
    let booked = repo.load(&id).await.expect("load");
    assert_eq!(booked.state.name(), "Booked");

    let cancel_effect = townhall_service::effect_identity_for(
        &id,
        townhall_domain::OperationKind::Cancel,
        booked.version,
    );

    // Drop the cancel's own response so the cancellation stays in flight...
    reqwest::Client::new()
        .post(format!("{}/test/faults", world.council_url))
        .json(&serde_json::json!({
            "effect_intent_id": cancel_effect.as_str(),
            "route": "cancel",
            "fault": "drop_response",
        }))
        .send()
        .await
        .expect("arm the drop");
    // ...and arm the wrong-kind answer for its resolve.
    reqwest::Client::new()
        .post(format!("{}/test/faults", world.council_url))
        .json(&serde_json::json!({
            "effect_intent_id": cancel_effect.as_str(),
            "route": "resolve",
            "fault": "wrong_kind",
        }))
        .send()
        .await
        .expect("arm the wrong kind");

    // Propose the cancellation through a coordinator over the same stores.
    let key = CouncilKey::new(
        council_wire::CouncilSigner::new(council_wire::CouncilSigningKey::from_bytes(&key_bytes()))
            .verifying_key(),
    );
    let denial_log = Arc::new(
        townhall_store::denials::DenialLog::open(
            dir.path().join("denials.sqlite"),
            Arc::clone(&clock) as Arc<dyn StoreClock>,
        )
        .await
        .expect("denial log"),
    );
    let coordinator = Coordinator::new(
        Arc::clone(&repo),
        Arc::new(CouncilClient::new(&world.council_url, key)),
        Arc::new(CouncilVerifier::new(key)),
        Arc::new(CouncilClient::new(&world.council_url, key)),
    )
    .with_denial_log(Arc::clone(&denial_log));

    let turn = coordinator
        .propose(
            &id,
            townhall_domain::BookingProposal::Cancel {
                reason: "no longer needed".to_owned(),
            },
            &driver_authority(),
        )
        .await
        .expect("the turn runs");
    assert!(
        turn.is_unresolved(),
        "the cancel's answer was eaten, so the turn is unresolved: {turn:?}"
    );

    // Reconcile the cancellation — through a reconciler whose coordinator
    // carries the SAME logbook, because "the refusal is in the logbook" is one
    // of this test's claims and the reconciler is the door it goes through.
    // (The PR #15 review caught the first draft asserting this from prose: the
    // attend ran through a log-less reconciler and no row was ever read.)
    let key2 = CouncilKey::new(
        council_wire::CouncilSigner::new(council_wire::CouncilSigningKey::from_bytes(&key_bytes()))
            .verifying_key(),
    );
    let reconciliation = Reconciliation::new(
        Arc::new(
            Coordinator::new(
                Arc::clone(&repo),
                Arc::new(CouncilClient::new(&world.council_url, key2)),
                Arc::new(CouncilVerifier::new(key2)),
                Arc::new(CouncilClient::new(&world.council_url, key2)),
            )
            .with_denial_log(Arc::clone(&denial_log)),
        ),
        Arc::new(CouncilClient::new(&world.council_url, key2)),
    );

    // The armed answer arrives — signed, wrong kind — passes the wire, and the
    // DOMAIN refuses it. Nothing commits, the intent stays live.
    clock.advance(60_000);
    let attended = reconciliation.attend(&cancel_effect).await.expect("attend");
    assert!(
        matches!(attended, Attended::StillUnknown { .. }),
        "a wrong-kind fact drives nothing: {attended:?}"
    );
    let still = repo.load(&id).await.expect("load");
    assert_eq!(
        still.state.name(),
        "CancellingBooking",
        "the cancellation is still in flight; the forged-shape answer moved nothing"
    );

    // And the refusal is in the logbook — read, not narrated: the fact door
    // said no to a BookingExists, and the principal is the one the signed
    // answer itself carried.
    let rows = denial_log.rows().await.expect("rows");
    assert_eq!(rows.len(), 1, "exactly the one refusal: {rows:?}");
    assert_eq!(rows[0].driver_kind, "Fact");
    assert_eq!(rows[0].driver_detail, "BookingExists");
    assert_eq!(rows[0].reason, "EffectKindMismatch");
    assert_eq!(rows[0].principal, "lucy");
}

fn driver_authority() -> townhall_domain::VerifiedAuthority {
    townhall_domain::VerifiedAuthority {
        principal: bld_types::PrincipalId::new("lucy"),
        actor: bld_types::ActorId::new("agent-1"),
        max_fee: bld_types::Money::from_pence(5_000),
        may_book: true,
        may_cancel: true,
    }
}

/// Faults that mangle the wire become Unknown, never a fact: garbage, and a
/// delay past the client's patience.
#[tokio::test]
async fn garbage_and_delay_become_unknown_never_facts() {
    let dir = tempfile::tempdir().expect("tempdir");
    let world = spawn_council(dir.path());
    let id = BookingId::new("BKG-MANGLE");
    let effect =
        townhall_service::effect_identity_for(&id, townhall_domain::OperationKind::Book, 2);

    // The create's answer is dropped FIRST, so the intent stays unsettled and
    // every attend below genuinely asks — a settled intent never calls out, its
    // armed faults go stale, and a later request consumes the wrong one. (The
    // first draft did exactly that: the "delay" assertion consumed a stale
    // "unsigned" and failed for a reason two faults removed from its own.)
    reqwest::Client::new()
        .post(format!("{}/test/faults", world.council_url))
        .json(&serde_json::json!({
            "effect_intent_id": effect.as_str(),
            "route": "create",
            "fault": "drop_response",
        }))
        .send()
        .await
        .expect("arm the drop");
    let status = run_driver(&world, "BKG-MANGLE", "never");
    assert!(status.success());

    // One reconciler and ONE movable clock across the legs: each turn's finish
    // now schedules a REAL cadence (ADR-021's repair), so a fresh clock per leg
    // would honestly answer NotDue to its own past.
    let (reconciliation, _repo, clock) = reconciler_over(&world).await;
    for fault in ["garbage", "unsigned"] {
        reqwest::Client::new()
            .post(format!("{}/test/faults", world.council_url))
            .json(&serde_json::json!({
                "effect_intent_id": effect.as_str(),
                "route": "resolve",
                "fault": fault,
            }))
            .send()
            .await
            .expect("arm");
        clock.advance(600_000);
        let attended = reconciliation.attend(&effect).await.expect("attend");
        assert!(
            matches!(attended, Attended::StillUnknown { .. }),
            "{fault}: a mangled answer is no answer, got {attended:?}"
        );
    }

    // Delay past the client's patience: the client under test gets 300ms of
    // patience and the council answers after 900ms. Lateness IS the fault, so
    // this pair of durations is the one legitimate appearance of wall-clock
    // time in this suite.
    reqwest::Client::new()
        .post(format!("{}/test/faults", world.council_url))
        .json(&serde_json::json!({
            "effect_intent_id": effect.as_str(),
            "route": "resolve",
            "fault": "delay",
            "ms": 900,
        }))
        .send()
        .await
        .expect("arm");
    let key = CouncilKey::new(
        council_wire::CouncilSigner::new(council_wire::CouncilSigningKey::from_bytes(&key_bytes()))
            .verifying_key(),
    );
    let impatient = CouncilClient::with_timeout(
        &world.council_url,
        key,
        std::time::Duration::from_millis(300),
    );
    let raw = townhall_service::EffectResolver::resolve(
        &impatient,
        &bld_types::EffectAttempt {
            id: EffectIntentId::new(effect.as_str()),
            expires_at_ms: i64::MAX / 2,
        },
        townhall_domain::OperationKind::Book,
    )
    .await;
    assert!(
        raw.is_err(),
        "an answer after our patience is no answer — Unknown, saying nothing"
    );
}

// ------------------------------------ slice F: in-flight cancellation (ADR-020)

/// A coordinator over the same store and council a dead process left behind.
fn coordinator_over(
    world: &World,
    repo: &Arc<SqliteBookingRepository>,
) -> Coordinator<SqliteBookingRepository, CouncilClient, CouncilVerifier, CouncilClient> {
    let key = CouncilKey::new(
        council_wire::CouncilSigner::new(council_wire::CouncilSigningKey::from_bytes(&key_bytes()))
            .verifying_key(),
    );
    Coordinator::new(
        Arc::clone(repo),
        Arc::new(CouncilClient::new(&world.council_url, key)),
        Arc::new(CouncilVerifier::new(key)),
        Arc::new(CouncilClient::new(&world.council_url, key)),
    )
}

/// Recovery as a PROCESS, dying on cue (test 12).
fn run_driver_reconcile(world: &World, die: &str, ahead_ms: i64) -> std::process::ExitStatus {
    Command::new(env!("CARGO_BIN_EXE_bld_driver"))
        .arg("--db")
        .arg(&world.bld_db)
        .args(["--council-url", &world.council_url])
        .args(["--key-hex", KEY_HEX])
        .args(["--reconcile"])
        .args(["--clock-ahead-ms", &ahead_ms.to_string()])
        .args(["--die", die])
        .status()
        .expect("run bld-driver --reconcile")
}

/// Cancellations in the council's own file: bookings whose `cancelled_by`
/// names a cancel effect. THE number test 11's "exactly one" is about.
async fn council_cancellations(world: &World) -> i64 {
    use sqlx::Row as _;
    let pool = sqlx::sqlite::SqlitePoolOptions::new()
        .max_connections(1)
        .connect_with(
            sqlx::sqlite::SqliteConnectOptions::new()
                .filename(&world.council_db)
                .read_only(true),
        )
        .await
        .expect("open read-only");
    sqlx::query("SELECT COUNT(*) AS n FROM bookings WHERE cancelled_by IS NOT NULL")
        .fetch_one(&pool)
        .await
        .expect("count")
        .get("n")
}

/// Server-side arrivals for one (route, identity) — the council's own number.
async fn council_requests(world: &World, route: &str, effect: &EffectIntentId) -> u64 {
    let body: serde_json::Value = reqwest::Client::new()
        .get(format!(
            "{}/test/requests/{route}/{}",
            world.council_url,
            effect.as_str()
        ))
        .send()
        .await
        .expect("counter")
        .json()
        .await
        .expect("json");
    body["count"].as_u64().expect("a count")
}

/// Book with the answer eaten (the council HOLDS the room), then ask to cancel
/// mid-ambiguity. Returns (booking id, book effect, cancel effect).
async fn ambiguous_cancellation(
    world: &World,
    repo: &Arc<SqliteBookingRepository>,
    booking: &str,
) -> (BookingId, EffectIntentId, EffectIntentId) {
    let id = BookingId::new(booking);
    let effect =
        townhall_service::effect_identity_for(&id, townhall_domain::OperationKind::Book, 2);
    reqwest::Client::new()
        .post(format!("{}/test/faults", world.council_url))
        .json(&serde_json::json!({
            "effect_intent_id": effect.as_str(),
            "route": "create",
            "fault": "drop_response",
        }))
        .send()
        .await
        .expect("arm the drop");
    let status = run_driver(world, booking, "never");
    assert!(status.success());

    let coordinator = coordinator_over(world, repo);
    let turn = coordinator
        .propose(
            &id,
            townhall_domain::BookingProposal::Cancel {
                reason: "changed my mind".to_owned(),
            },
            &driver_authority(),
        )
        .await
        .expect("the turn runs");
    assert!(
        matches!(turn, bld_kernel::BoundaryOutcome::Committed(_)),
        "Cancel mid-flight commits locally: {turn:?}"
    );
    // The cancel identity the handoff WILL mint, derived at the version the
    // settle will load: the version after the Cancel proposal's commit.
    let version = repo.load(&id).await.expect("load").version;
    let cancel =
        townhall_service::effect_identity_for(&id, townhall_domain::OperationKind::Cancel, version);
    (id, effect, cancel)
}

/// Test 9, ending (a): the booking EXISTS. The cancellation Lucy asked for
/// mid-ambiguity touches no wire at the proposal, and recovery then finds the
/// booking, hands off, sends the cancel — exactly one booking, exactly one
/// cancellation, in the council's own file.
#[tokio::test]
async fn a_cancellation_requested_during_ambiguity_ends_cancelled_when_the_booking_exists() {
    let dir = tempfile::tempdir().expect("tempdir");
    let world = spawn_council(dir.path());
    let (reconciliation, repo, clock) = reconciler_over(&world).await;
    let (id, _book, cancel) = ambiguous_cancellation(&world, &repo, "BKG-AMBIG-A").await;

    // The no-wire witness: the proposal sent NOTHING — the council has never
    // seen the cancel identity on any route.
    assert!(!council_knows(&world, cancel.as_str()).await);
    assert_eq!(council_requests(&world, "cancel", &cancel).await, 0);

    // Recovery: resolve the booking (it exists), hand off, send the cancel.
    clock.advance(60_000);
    let due = reconciliation.due(10).await.expect("due");
    let attended = reconciliation.attend(&due[0]).await.expect("attend");
    assert_eq!(attended, Attended::Settled, "the handoff");
    let attended = reconciliation.attend(&cancel).await.expect("attend");
    assert_eq!(attended, Attended::Settled, "the cancel send");

    assert_eq!(
        repo.load(&id).await.expect("load").state.name(),
        "Cancelled"
    );
    assert_eq!(council_bookings(&world).await, 1, "one booking, ever");
    assert_eq!(
        council_cancellations(&world).await,
        1,
        "one cancellation, ever"
    );
}

/// Test 9, ending (b): the booking NEVER arrived. The cancellation resolves to
/// `Cancelled` through the council's tombstone — and no cancellation effect is
/// ever minted, because there was never anything to cancel.
#[tokio::test]
async fn a_cancellation_requested_for_a_booking_nobody_received_fails_closed() {
    let dir = tempfile::tempdir().expect("tempdir");
    let world = spawn_council(dir.path());

    let status = run_driver(&world, "BKG-AMBIG-B", "before-call");
    assert!(!status.success(), "the create never left the process");
    let id = BookingId::new("BKG-AMBIG-B");
    let effect =
        townhall_service::effect_identity_for(&id, townhall_domain::OperationKind::Book, 2);
    assert!(!council_knows(&world, effect.as_str()).await);

    let (reconciliation, repo, clock) = reconciler_over(&world).await;
    let coordinator = coordinator_over(&world, &repo);
    coordinator
        .propose(
            &id,
            townhall_domain::BookingProposal::Cancel {
                reason: "give up".to_owned(),
            },
            &driver_authority(),
        )
        .await
        .expect("cancel");

    // Pre-deadline: the reconciler may only ASK here (resolve-only), and the
    // ask concludes nothing. A wanted-table that answered "send" would book
    // the room Lucy is cancelling — bookings must stay zero.
    clock.advance(60_000);
    let attended = reconciliation.attend(&effect).await.expect("attend");
    assert!(matches!(attended, Attended::StillUnknown { .. }));
    assert_eq!(council_bookings(&world).await, 0, "nothing was caused");

    // Past the deadline (the council's clock, via restart): tombstoned absence
    // settles the story. No cancel effect ever existed.
    drop(world);
    let world = spawn_council_at(dir.path(), Some(MovableClock::now() + 600_000));
    let (reconciliation, repo, clock) = reconciler_over(&world).await;
    clock.advance(townhall_store::MAX_CADENCE_MS + townhall_store::DEFAULT_EFFECT_TTL_MS);
    let attended = reconciliation.attend(&effect).await.expect("attend");
    assert_eq!(attended, Attended::Settled);
    assert_eq!(
        repo.load(&id).await.expect("load").state.name(),
        "Cancelled"
    );
    assert_eq!(council_bookings(&world).await, 0);
    // "Never minted" is read from the table itself, not inferred from an empty
    // due-list — a wrong implementation could mint a dormant or terminal cancel
    // successor that due() would never surface (map audit, row 9).
    let intents: i64 =
        sqlx::query_scalar("SELECT COUNT(*) FROM effect_intents WHERE booking_id = ?")
            .bind(id.to_string())
            .fetch_one(repo.pool())
            .await
            .expect("count");
    assert_eq!(
        intents, 1,
        "exactly the booking intent: no cancellation effect ever existed"
    );
}

/// Test 11: the cancellation commits at the council and the response is
/// dropped — recovery finds exactly ONE cancellation, not two.
#[tokio::test]
async fn a_cancellation_whose_answer_is_eaten_converges_to_one_cancellation() {
    let dir = tempfile::tempdir().expect("tempdir");
    let world = spawn_council(dir.path());
    let status = run_driver(&world, "BKG-CXLDROP", "never");
    assert!(status.success());
    let id = BookingId::new("BKG-CXLDROP");

    let (reconciliation, repo, clock) = reconciler_over(&world).await;
    let booked = repo.load(&id).await.expect("load");
    let cancel = townhall_service::effect_identity_for(
        &id,
        townhall_domain::OperationKind::Cancel,
        booked.version,
    );
    let armed: serde_json::Value = reqwest::Client::new()
        .post(format!("{}/test/faults", world.council_url))
        .json(&serde_json::json!({
            "effect_intent_id": cancel.as_str(),
            "route": "cancel",
            "fault": "drop_response",
        }))
        .send()
        .await
        .expect("arm")
        .json()
        .await
        .expect("json");
    let fault_id = armed["fault_id"].as_u64().expect("id");

    let coordinator = coordinator_over(&world, &repo);
    let turn = coordinator
        .propose(
            &id,
            townhall_domain::BookingProposal::Cancel {
                reason: "no longer needed".to_owned(),
            },
            &driver_authority(),
        )
        .await
        .expect("turn");
    assert!(turn.is_unresolved(), "the answer was eaten: {turn:?}");

    // The fault fired and the council DID cancel — the work happened, only the
    // answer died.
    let consumed: serde_json::Value = reqwest::Client::new()
        .get(format!("{}/test/faults/{fault_id}", world.council_url))
        .send()
        .await
        .expect("status")
        .json()
        .await
        .expect("json");
    assert_eq!(consumed["consumed"], 1);
    assert_eq!(council_cancellations(&world).await, 1);

    // Recovery queries, the council answers what it did, and the state
    // converges — no second cancellation is ever caused.
    clock.advance(60_000);
    let attended = reconciliation.attend(&cancel).await.expect("attend");
    assert_eq!(attended, Attended::Settled);
    assert_eq!(
        repo.load(&id).await.expect("load").state.name(),
        "Cancelled"
    );
    assert_eq!(council_cancellations(&world).await, 1, "EXACTLY one");
    assert_eq!(
        council_requests(&world, "cancel", &cancel).await,
        1,
        "recovery did not re-send what the query already answered"
    );
}

/// Test 12, the post-mark window, as a REAL death: recovery commits the
/// handoff, marks the cancel attempt, and dies at the capability's entry —
/// before one byte reaches the council. A fresh recovery must RESUME under the
/// same identity: query, the council's signed "nothing yet", resend.
#[tokio::test]
async fn a_death_between_the_handoff_and_the_cancel_call_is_resumed_under_the_same_identity() {
    let dir = tempfile::tempdir().expect("tempdir");
    let world = spawn_council(dir.path());
    {
        let (_, repo, _) = reconciler_over(&world).await;
        ambiguous_cancellation(&world, &repo, "BKG-DIES-MID").await;
    }
    let id = BookingId::new("BKG-DIES-MID");

    // Recovery, as a process, dying at the first send it decides on: round one
    // resolves the booking and commits the handoff (no capability touched);
    // round two claims the Prepared cancel, MARKS it, and aborts at the
    // capability's entry.
    let status = run_driver_reconcile(&world, "before-call", 60_000);
    assert!(!status.success(), "recovery aborted, as armed");

    // The crash state, from the two databases: locally the handoff stands and
    // the mark is spent; the council never heard the cancel identity.
    let (reconciliation, repo, clock) = reconciler_over(&world).await;
    let stranded = repo.load(&id).await.expect("load");
    assert_eq!(stranded.state.name(), "CancellingBooking");
    let cancel = stranded.active_effect.clone().expect("the cancel intent");
    let (started, finished) = sqlx::query_as::<_, (i64, i64)>(
        "SELECT attempts_started, attempts_finished FROM effect_intents \
         WHERE effect_intent_id = ?",
    )
    .bind(cancel.as_str())
    .fetch_one(repo.pool())
    .await
    .expect("row");
    assert_eq!(
        (started, finished),
        (1, 0),
        "the mark is durable, the call never returned: the crash is in the ledger"
    );
    assert!(
        !council_knows(&world, cancel.as_str()).await,
        "not one byte reached the council"
    );

    // Resume. The dead run died HOLDING its claim (written on its ahead-running
    // clock), so a fresh recovery must first outwait that lease — the fencing
    // working as designed: 60s of the dead run's clock offset + the 30s lease
    // term, comfortably passed at 120s. Then: query → signed not-yet → resend,
    // same identity, by construction.
    clock.advance(120_000);
    let attended = reconciliation.attend(&cancel).await.expect("attend");
    assert_eq!(attended, Attended::Settled);
    assert_eq!(
        repo.load(&id).await.expect("load").state.name(),
        "Cancelled"
    );
    assert_eq!(council_cancellations(&world).await, 1);
    assert_eq!(
        council_requests(&world, "cancel", &cancel).await,
        1,
        "one arrival ever: the resumed send IS the first delivery"
    );
}

/// Test 12, the pre-mark window: between the handoff's commit and the mark,
/// the process holds nothing in flight — no transaction, no socket, no claim
/// on the successor — so the crash state IS the committed database, built here
/// by the same committed operations a crash would leave and read back through
/// a fresh connection. A SIGKILL would add nothing: there is nothing mid-flight
/// to kill. Recovery's first-send leg executes the never-attempted intent.
#[tokio::test]
async fn a_death_before_the_first_mark_leaves_a_prepared_cancel_that_recovery_sends() {
    let dir = tempfile::tempdir().expect("tempdir");
    let world = spawn_council(dir.path());
    let (reconciliation, repo, clock) = reconciler_over(&world).await;
    let (id, book, cancel) = ambiguous_cancellation(&world, &repo, "BKG-PREMARK").await;

    // The handoff commits, and this connection's work ENDS — the crash.
    clock.advance(60_000);
    let attended = reconciliation.attend(&book).await.expect("attend");
    assert_eq!(attended, Attended::Settled, "the handoff committed");

    // A fresh restart over the leftover files.
    let (reconciliation, repo, clock) = reconciler_over(&world).await;
    let stranded = repo.load(&id).await.expect("load");
    assert_eq!(stranded.state.name(), "CancellingBooking");
    let (status, started): (String, i64) = sqlx::query_as(
        "SELECT status, attempts_started FROM effect_intents WHERE effect_intent_id = ?",
    )
    .bind(cancel.as_str())
    .fetch_one(repo.pool())
    .await
    .expect("row");
    assert_eq!(
        (status.as_str(), started),
        ("Prepared", 0),
        "never attempted: the crash landed before the mark"
    );

    clock.advance(60_000);
    assert_eq!(
        reconciliation.due(10).await.expect("due"),
        vec![cancel.clone()],
        "recovery finds the never-attempted successor"
    );
    let attended = reconciliation.attend(&cancel).await.expect("attend");
    assert_eq!(attended, Attended::Settled);
    assert_eq!(
        repo.load(&id).await.expect("load").state.name(),
        "Cancelled"
    );
    assert_eq!(council_cancellations(&world).await, 1);
}

/// Test 13: the same cancellation identity, retried, returns the ORIGINAL
/// result — the council's number says both calls arrived, its file says one
/// cancellation exists, and the retry's signed body matches that durable
/// record (the dropped first answer is unobservable by definition).
#[tokio::test]
async fn a_cancel_retried_under_the_same_identity_returns_the_original() {
    let dir = tempfile::tempdir().expect("tempdir");
    let world = spawn_council(dir.path());
    let status = run_driver(&world, "BKG-CXLRETRY", "never");
    assert!(status.success());
    let id = BookingId::new("BKG-CXLRETRY");

    let (_, repo, _) = reconciler_over(&world).await;
    let booked = repo.load(&id).await.expect("load");
    let booked_ref = booked.booking_ref.clone().expect("a reference");
    let cancel = townhall_service::effect_identity_for(
        &id,
        townhall_domain::OperationKind::Cancel,
        booked.version,
    );
    reqwest::Client::new()
        .post(format!("{}/test/faults", world.council_url))
        .json(&serde_json::json!({
            "effect_intent_id": cancel.as_str(),
            "route": "cancel",
            "fault": "drop_response",
        }))
        .send()
        .await
        .expect("arm");
    let coordinator = coordinator_over(&world, &repo);
    let turn = coordinator
        .propose(
            &id,
            townhall_domain::BookingProposal::Cancel {
                reason: "retry me".to_owned(),
            },
            &driver_authority(),
        )
        .await
        .expect("turn");
    assert!(turn.is_unresolved());
    assert_eq!(council_requests(&world, "cancel", &cancel).await, 1);

    // The retry: the SAME persisted attempt, straight at the capability — two
    // server-side arrivals, one durable cancellation, the original answer.
    let intent = repo.load_effect(&cancel).await.expect("the intent");
    let key = CouncilKey::new(
        council_wire::CouncilSigner::new(council_wire::CouncilSigningKey::from_bytes(&key_bytes()))
            .verifying_key(),
    );
    let client = CouncilClient::new(&world.council_url, key);
    let raw = bld_kernel::Capability::execute(
        &client,
        &intent.canonical_plan,
        &bld_types::EffectAttempt {
            id: cancel.clone(),
            expires_at_ms: intent.expires_at_ms,
        },
    )
    .await
    .expect("the retry is answered");

    assert_eq!(
        council_requests(&world, "cancel", &cancel).await,
        2,
        "both calls crossed the wire — the council's own number"
    );
    assert_eq!(council_cancellations(&world).await, 1, "one cancellation");
    let fact = bld_kernel::Verifier::verify(&CouncilVerifier::new(key), raw)
        .expect("the retry's answer is signed and verifies");
    assert_eq!(
        *fact.get(),
        townhall_domain::VerifiedProviderFact::CancellationExists {
            effect_intent_id: cancel.clone(),
            booking_ref: booked_ref,
        },
        "the retry's signed body IS the durable original outcome"
    );
}

/// The resend privilege's negative space, half one: an UNSIGNED honest
/// "nothing yet" authorizes nothing. The council computes the true answer and
/// strips the signature; a recovery that trusts the payload's shape would
/// resend — and book the room — here.
#[tokio::test]
async fn an_unsigned_not_yet_authorizes_nothing() {
    let dir = tempfile::tempdir().expect("tempdir");
    let world = spawn_council(dir.path());
    let status = run_driver(&world, "BKG-NOSIG", "before-call");
    assert!(!status.success());
    let id = BookingId::new("BKG-NOSIG");
    let effect =
        townhall_service::effect_identity_for(&id, townhall_domain::OperationKind::Book, 2);

    let armed: serde_json::Value = reqwest::Client::new()
        .post(format!("{}/test/faults", world.council_url))
        .json(&serde_json::json!({
            "effect_intent_id": effect.as_str(),
            "route": "resolve",
            "fault": "unsigned",
        }))
        .send()
        .await
        .expect("arm")
        .json()
        .await
        .expect("json");
    let fault_id = armed["fault_id"].as_u64().expect("id");

    let (reconciliation, repo, clock) = reconciler_over(&world).await;
    clock.advance(60_000);
    let attended = reconciliation.attend(&effect).await.expect("attend");
    assert!(
        matches!(attended, Attended::StillUnknown { .. }),
        "an unattributable not-yet is no authorization: {attended:?}"
    );
    let consumed: serde_json::Value = reqwest::Client::new()
        .get(format!("{}/test/faults/{fault_id}", world.council_url))
        .send()
        .await
        .expect("status")
        .json()
        .await
        .expect("json");
    assert_eq!(
        consumed["consumed"], 1,
        "the mangled reply was the one served"
    );
    assert_eq!(
        council_requests(&world, "create", &effect).await,
        0,
        "NO send followed — the blind-resend implementation dies here"
    );
    assert_eq!(council_bookings(&world).await, 0);
    assert_eq!(
        repo.load(&id).await.expect("load").state.name(),
        "BookingInProgress"
    );
}

/// Half two: a correctly SIGNED "nothing yet" naming a DIFFERENT identity
/// authorizes nothing either — the signature alone is never enough, the reply
/// must name exactly the asked attempt.
#[tokio::test]
async fn a_signed_not_yet_for_someone_else_authorizes_nothing() {
    let dir = tempfile::tempdir().expect("tempdir");
    let world = spawn_council(dir.path());
    let status = run_driver(&world, "BKG-WRONGID", "before-call");
    assert!(!status.success());
    let id = BookingId::new("BKG-WRONGID");
    let effect =
        townhall_service::effect_identity_for(&id, townhall_domain::OperationKind::Book, 2);

    reqwest::Client::new()
        .post(format!("{}/test/faults", world.council_url))
        .json(&serde_json::json!({
            "effect_intent_id": effect.as_str(),
            "route": "resolve",
            "fault": "wrong_id_not_yet",
        }))
        .send()
        .await
        .expect("arm");

    let (reconciliation, repo, clock) = reconciler_over(&world).await;
    clock.advance(60_000);
    let attended = reconciliation.attend(&effect).await.expect("attend");
    assert!(
        matches!(attended, Attended::StillUnknown { .. }),
        "someone else's not-yet says nothing about OUR attempt: {attended:?}"
    );
    assert_eq!(council_requests(&world, "create", &effect).await, 0);
    assert_eq!(council_bookings(&world).await, 0);
    assert_eq!(
        repo.load(&id).await.expect("load").state.name(),
        "BookingInProgress"
    );
}

/// As [`spawn_council_at`], with a pause point armed and the stdout protocol
/// kept alive on a channel — the reader thread forwards every line, and the
/// child's death arrives as `EXITED` on the same channel, so no wait can hang.
fn spawn_council_paused(
    dir: &std::path::Path,
    clock_ms: i64,
    pause_at: &str,
) -> (World, std::sync::mpsc::Receiver<String>) {
    let status = Command::new(env!("CARGO"))
        .args(["build", "-p", "mock-council", "--features", "test-faults"])
        .status()
        .expect("cargo build mock-council");
    assert!(status.success(), "the council must build");

    let binary =
        std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("../../target/debug/mock-council");
    let council_db = dir.join("council.sqlite");
    let mut child = Command::new(binary)
        .arg("--db")
        .arg(&council_db)
        .args(["--key-hex", KEY_HEX, "--port", "0"])
        .args(["--clock", &clock_ms.to_string()])
        .args(["--pause-at", pause_at])
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .spawn()
        .expect("spawn the council");

    let stdout = child.stdout.take().expect("piped stdout");
    let (sender, lines) = std::sync::mpsc::channel();
    std::thread::spawn(move || {
        use std::io::BufRead as _;
        for line in std::io::BufReader::new(stdout).lines() {
            let Ok(line) = line else { break };
            if sender.send(line).is_err() {
                break;
            }
        }
        let _ = sender.send("EXITED".to_owned());
    });

    let ready = lines
        .recv_timeout(std::time::Duration::from_secs(60))
        .expect("a first line");
    let port: u16 = ready
        .strip_prefix("READY ")
        .unwrap_or_else(|| panic!("expected READY, got {ready:?}"))
        .parse()
        .expect("a port");

    (
        World {
            _dir: tempfile::TempDir::new().expect("unused"),
            council_url: format!("http://127.0.0.1:{port}"),
            bld_db: dir.join("townhall.sqlite"),
            council_db,
            council: child,
        },
        lines,
    )
}

/// Test 20's BLD-side leg (map audit): the council dies mid-tombstone-write
/// DURING OUR OWN LOOKUP — the uncommitted absence must reach us as no answer
/// at all, the workflow stays `Unknown` and retries, and the absence that
/// finally settles it is a fresh determination by a live council, never the
/// phantom of the one that died.
#[tokio::test]
async fn a_lookup_killed_mid_tombstone_leaves_the_workflow_unknown_and_retrying() {
    let dir = tempfile::tempdir().expect("tempdir");
    {
        let world = spawn_council(dir.path());
        let status = run_driver(&world, "BKG-K20", "before-call");
        assert!(!status.success(), "the create never left the process");
    }
    let id = BookingId::new("BKG-K20");
    let effect =
        townhall_service::effect_identity_for(&id, townhall_domain::OperationKind::Book, 2);

    // A council past the deadline, pausing INSIDE the tombstone settlement our
    // own lookup is about to trigger.
    let (mut world, lines) = spawn_council_paused(
        dir.path(),
        MovableClock::now() + 600_000,
        "before_settle_commit",
    );
    let (reconciliation, repo, clock) = reconciler_over(&world).await;
    clock.advance(60_000);
    let attend = {
        let effect = effect.clone();
        tokio::spawn(async move { reconciliation.attend(&effect).await })
    };

    // The council announces the pause — our lookup is being answered by a
    // tombstone whose write has NOT committed. Kill it there.
    let paused = tokio::task::spawn_blocking(move || {
        loop {
            let line = lines
                .recv_timeout(std::time::Duration::from_secs(60))
                .expect("a line or EXITED");
            if line.starts_with("PAUSED before_settle_commit") || line == "EXITED" {
                return line;
            }
        }
    })
    .await
    .expect("the watcher ran");
    assert!(
        paused.starts_with("PAUSED before_settle_commit"),
        "the lookup reached the settlement: {paused}"
    );
    let _ = world.council.kill();
    let _ = world.council.wait();

    // Our side observed NO absence answer: still unknown, still in flight —
    // an implementation mapping the dead lookup to local absence dies here.
    let attended = attend.await.expect("the task ran").expect("the turn ran");
    assert!(
        matches!(attended, Attended::StillUnknown { .. }),
        "an uncommitted tombstone is no answer: {attended:?}"
    );
    assert_eq!(
        repo.load(&id).await.expect("load").state.name(),
        "BookingInProgress"
    );

    // And RETRIES: against a live council, the retried lookup obtains a real,
    // freshly decided absence — unobserved absence never became observed
    // absence, and the observed one is the council's own determination.
    drop(world);
    let world = spawn_council_at(dir.path(), Some(MovableClock::now() + 600_000));
    let (reconciliation, repo, clock) = reconciler_over(&world).await;
    // Past the FIRST attempt's real schedule (its finish booked now+5s on the
    // earlier clock — ADR-021's repair made that gate genuine), with margin.
    clock.advance(120_000);
    let attended = reconciliation.attend(&effect).await.expect("attend");
    assert_eq!(attended, Attended::Settled);
    assert_eq!(
        repo.load(&id).await.expect("load").state.name(),
        "AwaitingBooking"
    );
    assert_eq!(council_bookings(&world).await, 0);
}
