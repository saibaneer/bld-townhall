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
        Coordinator::new(
            Arc::clone(&repo),
            Arc::new(client()),
            Arc::new(CouncilVerifier::new(key())),
            Arc::new(client()),
        ),
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

/// Test 1: our process dies AFTER the intent commits and BEFORE one byte
/// reaches the council. The intent is durable; the provider has nothing; and
/// recovery converges without ever double-booking.
#[tokio::test]
async fn a_crash_before_the_call_leaves_a_durable_intent_and_an_ignorant_council() {
    let dir = tempfile::tempdir().expect("tempdir");
    let world = spawn_council(dir.path());

    let status = run_driver(&world, "BKG-DIES-EARLY", "before-call");
    assert!(!status.success(), "the driver aborted, as armed");

    let id = BookingId::new("BKG-DIES-EARLY");
    let effect =
        townhall_service::effect_identity_for(&id, townhall_domain::OperationKind::Book, 2);

    // The two halves of test 1, asserted from the two databases.
    let (reconciliation, repo, clock) = reconciler_over(&world).await;
    let intent = repo
        .load_effect(&effect)
        .await
        .expect("the intent is durable");
    assert_eq!(intent.effect_intent_id, effect);
    assert!(
        !council_knows(&world, effect.as_str()).await,
        "the provider has nothing — read from its database, not through an \
         endpoint that would bind the identity on first sight"
    );

    // Recovery, ask-only: before the deadline the council honestly says
    // "not yet visible", which is Unknown, which drives nothing.
    clock.advance(60_000);
    let attended = reconciliation.attend(&effect).await.expect("attend");
    assert!(
        matches!(attended, Attended::StillUnknown { .. }),
        "pre-deadline, an undelivered create is genuinely unknown: {attended:?}"
    );

    // Past the effect's deadline the council tombstones it: definitive absence,
    // and the booking returns to re-proposable. The room was NEVER booked, so
    // "it failed, try again" is now literally true (ADR-019 / the owner's rule).
    //
    // The deadline is the COUNCIL's to judge, on the COUNCIL's clock (ADR-016)
    // — advancing our store clock moves only our cadence, never its verdict. So
    // "later" is a council restart with its clock ahead, which is what actually
    // happens when real time passes. (The first draft of this test advanced our
    // clock and expected absence: the council rightly kept answering "not yet",
    // which is ADR-016 §2 refusing to let OUR clock manufacture absence.)
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
    let (reconciliation, repo, clock) = reconciler_over(&world).await;
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

    // Reconcile the cancellation: the armed answer arrives — signed, wrong
    // kind — passes the wire, and the DOMAIN refuses it. Nothing commits, the
    // intent stays live, and the refusal is in the logbook.
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
        let (reconciliation, _repo, clock) = reconciler_over(&world).await;
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
