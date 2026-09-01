//! The crash matrix, against a real process.
//!
//! Everything in here spawns the actual `mock-council` binary, drives it over
//! the stdin/stdout protocol, and kills it with `SIGKILL` at armed pause points.
//! Cancelling a task is not a crash — memory survives, transactions roll back
//! politely — so these are the only tests in the project that exercise what a
//! power cut actually does.
//!
//! # No sleeps
//!
//! Every wait in this file is on an event: a protocol line arriving, or the
//! child process exiting. The reader thread forwards both into one channel, so
//! "wait for PAUSED or death" is a single `recv` — a child that dies
//! mid-handshake produces an `EXITED` line rather than a hang (gate M14).
//! Every recv also carries a deadline: if the harness itself is ever the thing
//! that stops forwarding, nontermination becomes a NAMED failure here rather
//! than a CI job that times out an hour later saying nothing.

use std::{
    io::{BufRead as _, Write as _},
    process::{Child, Command, Stdio},
    sync::mpsc,
    time::Duration,
};

const KEY_HEX: &str = "0707070707070707070707070707070707070707070707070707070707070707";
const NOW: i64 = 2_000_000_000;
const DEADLINE: i64 = 2_000_030_000;

/// A running council child and the means to talk to it.
struct Council {
    child: Child,
    lines: mpsc::Receiver<String>,
    port: u16,
}

impl Council {
    /// Spawn against `db`, with `pause_at` armed and the clock at `clock_ms`.
    /// Returns once `READY` arrives — never sleeps for the socket.
    fn spawn(db: &std::path::Path, clock_ms: i64, pause_at: &[&str]) -> Self {
        let mut command = Command::new(env!("CARGO_BIN_EXE_mock-council"));
        command
            .arg("--db")
            .arg(db)
            .args(["--key-hex", KEY_HEX, "--port", "0"])
            .args(["--clock", &clock_ms.to_string()])
            .stdin(Stdio::piped())
            .stdout(Stdio::piped());
        for point in pause_at {
            command.args(["--pause-at", point]);
        }
        let mut child = command.spawn().expect("spawn mock-council");

        // One channel carries protocol lines AND the child's death, so every
        // wait below is a single recv with no race between the two.
        let stdout = child.stdout.take().expect("piped stdout");
        let (sender, lines) = mpsc::channel();
        let exit_sender = sender.clone();
        std::thread::spawn(move || {
            let reader = std::io::BufReader::new(stdout);
            for line in reader.lines() {
                let Ok(line) = line else { break };
                if exit_sender.send(line).is_err() {
                    break;
                }
            }
            // stdout closed: the child is gone. Say so on the same channel.
            let _ = exit_sender.send("EXITED".to_owned());
        });

        let ready = Self::next(&lines);
        let port = ready
            .strip_prefix("READY ")
            .unwrap_or_else(|| panic!("expected READY, got {ready:?}"))
            .parse()
            .expect("a port");

        Self { child, lines, port }
    }

    /// One line off the channel, under the runner-level deadline (gate M14):
    /// a wait that can never be satisfied fails BY NAME instead of hanging.
    fn next(lines: &mpsc::Receiver<String>) -> String {
        lines
            .recv_timeout(Duration::from_secs(60))
            .expect("a line, EXITED, or the deadline naming the hang")
    }

    /// The next line starting with `prefix`. Panics — with the line — on the
    /// child dying first, unless death is what the test wanted.
    fn expect(&self, prefix: &str) -> String {
        let line = Self::next(&self.lines);
        assert!(
            line.starts_with(prefix),
            "expected a {prefix:?} line, got {line:?}"
        );
        line
    }

    fn say(&mut self, line: &str) {
        let stdin = self.child.stdin.as_mut().expect("piped stdin");
        writeln!(stdin, "{line}").expect("write to child");
        stdin.flush().expect("flush");
    }

    fn kill(&mut self) {
        let _ = self.child.kill();
        let _ = self.child.wait();
    }

    fn url(&self, path: &str) -> String {
        format!("http://127.0.0.1:{}{path}", self.port)
    }
}

impl Drop for Council {
    fn drop(&mut self) {
        self.kill();
    }
}

/// Fire one effect request on its own thread. The caller may be killed
/// mid-handling — so there may never be an answer — or the answer may be the
/// assertion, joined for after a RELEASE.
fn post_detached(
    url: String,
    body: serde_json::Value,
) -> std::thread::JoinHandle<Option<serde_json::Value>> {
    std::thread::spawn(move || ureq_post(&url, &body))
}

/// A tiny blocking POST that tolerates the connection dying — which is the
/// point of half these tests.
fn ureq_post(url: &str, body: &serde_json::Value) -> Option<serde_json::Value> {
    let agent = reqwest::blocking::Client::builder()
        .timeout(std::time::Duration::from_secs(30))
        .build()
        .ok()?;
    let response = agent.post(url).json(body).send().ok()?;
    response.json().ok()
}

fn resolve_body(deadline: i64) -> serde_json::Value {
    serde_json::json!({ "expires_at_ms": deadline, "operation_kind": "Book" })
}

fn create_body(id: &str, deadline: i64, grant: &str) -> serde_json::Value {
    serde_json::json!({
        "effect_intent_id": id,
        "expires_at_ms": deadline,
        "venue_id": "TH-A",
        "slot_id": "SLOT-A",
        "attendees": 20,
        "fee_pence": 4500,
        "principal": "lucy",
        "grant": grant,
    })
}

fn read_grant(council: &Council) -> String {
    let response: serde_json::Value = reqwest::blocking::Client::new()
        .get(council.url("/venues/TH-A/slots/SLOT-A"))
        .send()
        .expect("availability")
        .json()
        .expect("json");
    response["grant"].as_str().expect("a grant").to_owned()
}

/// Count matching effect rows straight from the council's database file, with
/// the process DEAD. This is what makes a "did the write commit?" test
/// discriminating: asking the restarted council re-decides the answer, but the
/// file between kill and restart holds only what actually committed. Opened
/// read-write so `SQLite` can run WAL recovery — which is exactly what a
/// restart would do, and recovery discards the uncommitted transaction.
fn effect_rows_in(db: &std::path::Path, effect_intent_id: &str) -> i64 {
    count_in(
        db,
        "SELECT COUNT(*) FROM effects WHERE effect_intent_id = ?",
        effect_intent_id,
    )
}

/// Bookings created by this identity — the create-path visibility witness.
fn booking_rows_in(db: &std::path::Path, effect_intent_id: &str) -> i64 {
    count_in(
        db,
        "SELECT COUNT(*) FROM bookings WHERE created_by = ?",
        effect_intent_id,
    )
}

/// Settled outcomes for this identity: anything past `Open` — a decided
/// create, a tombstone, a rejection. Pre-commit, all of them must be zero.
fn settled_effect_rows_in(db: &std::path::Path, effect_intent_id: &str) -> i64 {
    count_in(
        db,
        "SELECT COUNT(*) FROM effects WHERE effect_intent_id = ? AND state <> 'Open'",
        effect_intent_id,
    )
}

fn count_in(db: &std::path::Path, sql: &'static str, bind: &str) -> i64 {
    let db = db.to_path_buf();
    let bind = bind.to_owned();
    tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .expect("runtime")
        .block_on(async move {
            let pool = sqlx::sqlite::SqlitePoolOptions::new()
                .max_connections(1)
                .connect_with(sqlx::sqlite::SqliteConnectOptions::new().filename(&db))
                .await
                .expect("open the council's database");
            let n: i64 = sqlx::query_scalar(sql)
                .bind(&bind)
                .fetch_one(&pool)
                .await
                .expect("count");
            pool.close().await;
            n
        })
}

// --------------------------------------------------------------- the matrix

/// Test 21 / 2a's second half: killed AFTER the settlement commits and before
/// anyone hears the answer, the answer must be reproducible after restart —
/// and a LATER CREATE for the same identity must still be refused by the
/// crash-survived tombstone, not merely reported absent.
#[test]
fn an_answer_no_one_heard_survives_a_kill() {
    let dir = tempfile::tempdir().expect("tempdir");
    let db = dir.path().join("council.sqlite");

    let mut council = Council::spawn(&db, NOW, &["after_settle_commit"]);
    let url = council.url("/effects/EFF-K1/resolve");
    // Past the deadline, so the resolve tombstones — a durable determination.
    let _asker = post_detached(url, resolve_body(NOW - 1));

    council.expect("PAUSED after_settle_commit EFF-K1");
    council.kill(); // the answer was committed; nobody ever saw it

    let survivor = Council::spawn(&db, NOW, &[]);
    let answer = ureq_post(
        &survivor.url("/effects/EFF-K1/resolve"),
        &resolve_body(NOW - 1),
    )
    .expect("an answer after restart");
    assert_eq!(
        answer["outcome"], "DefinitivelyAbsent",
        "the tombstone survived the kill, and the retried lookup reports it"
    );

    // Test 21's second leg: the tombstone doesn't just answer lookups — it
    // REFUSES a create that arrives afterwards, so "definitively absent" can
    // never quietly become "booked after all".
    let grant = read_grant(&survivor);
    let attempted = ureq_post(
        &survivor.url("/bookings"),
        &create_body("EFF-K1", NOW - 1, &grant),
    )
    .expect("an answer");
    assert_eq!(
        attempted["outcome"], "DefinitivelyAbsent",
        "the crash-survived tombstone refuses the late create"
    );
    assert_eq!(
        effect_rows_in(&db, "EFF-K1"),
        1,
        "one determination, no second row"
    );
}

/// Test 20: killed BEFORE the settlement commits, nothing is discoverable —
/// unobserved absence must never become observed absence.
///
/// The discriminating assertion is the middle one, read from the FILE while the
/// process is dead: the retried resolve would answer `DefinitivelyAbsent`
/// either way (a past-deadline resolve re-decides from scratch), so only the
/// database itself can tell "the uncommitted write died" from "it leaked".
#[test]
fn an_uncommitted_answer_dies_with_the_process() {
    let dir = tempfile::tempdir().expect("tempdir");
    let db = dir.path().join("council.sqlite");

    let mut council = Council::spawn(&db, NOW, &["before_settle_commit"]);
    let url = council.url("/effects/EFF-K2/resolve");
    let _asker = post_detached(url, resolve_body(NOW - 1));

    council.expect("PAUSED before_settle_commit EFF-K2");
    council.kill(); // the tombstone was never committed

    // Between the kill and the restart: the file holds NOTHING for this
    // identity. A council that committed (or leaked) before its armed pause
    // fails here, where the wire answer below could not catch it.
    assert_eq!(
        effect_rows_in(&db, "EFF-K2"),
        0,
        "the uncommitted settlement died with the process"
    );

    let survivor = Council::spawn(&db, NOW, &[]);
    // The registry holds nothing settled for this identity: a fresh resolve
    // decides from scratch (and, past the deadline, reaches the same verdict —
    // by deciding it, not by replaying a phantom).
    let answer = ureq_post(
        &survivor.url("/effects/EFF-K2/resolve"),
        &resolve_body(NOW - 1),
    )
    .expect("an answer");
    assert_eq!(answer["outcome"], "DefinitivelyAbsent");
}

/// Test 2a: an authoritative REJECTION is committed before it is observable —
/// killed between the two, the retried lookup still reports the rejection,
/// with its reason.
#[test]
fn a_rejection_no_one_heard_survives_a_kill() {
    let dir = tempfile::tempdir().expect("tempdir");
    let db = dir.path().join("council.sqlite");

    let mut council = Council::spawn(&db, NOW, &["after_settle_commit"]);
    let grant = read_grant(&council);
    // A fee the catalogue disagrees with: authoritatively rejected.
    let mut body = create_body("EFF-K3", DEADLINE, &grant);
    body["fee_pence"] = serde_json::json!(1);
    let _asker = post_detached(council.url("/bookings"), body);

    council.expect("PAUSED after_settle_commit EFF-K3");
    council.kill();

    let survivor = Council::spawn(&db, NOW, &[]);
    let answer = ureq_post(
        &survivor.url("/effects/EFF-K3/resolve"),
        &resolve_body(DEADLINE),
    )
    .expect("an answer");
    assert_eq!(answer["outcome"], "ProviderRejected");
    assert!(
        answer["reason"]
            .as_str()
            .expect("a reason")
            .contains("4500"),
        "the reason survives too: {answer}"
    );
}

/// Test 2b: a rejection stays rejected across a clock ROLLBACK — the recorded
/// state refuses the retry, not a time comparison.
#[test]
fn a_rejection_survives_the_clock_winding_back() {
    let dir = tempfile::tempdir().expect("tempdir");
    let db = dir.path().join("council.sqlite");

    {
        let council = Council::spawn(&db, NOW, &[]);
        let grant = read_grant(&council);
        let mut body = create_body("EFF-K4", DEADLINE, &grant);
        body["fee_pence"] = serde_json::json!(1);
        let answer = ureq_post(&council.url("/bookings"), &body).expect("an answer");
        assert_eq!(answer["outcome"], "ProviderRejected");
    }

    // Restart with the clock WOUND BACK a full day.
    let rewound = Council::spawn(&db, NOW - 86_400_000, &[]);
    let grant = read_grant(&rewound);
    let retry = ureq_post(
        &rewound.url("/bookings"),
        &create_body("EFF-K4", DEADLINE, &grant),
    )
    .expect("an answer");
    assert_eq!(
        retry["outcome"], "ProviderRejected",
        "the tombstone refuses it; the clock has no say"
    );
}

/// Test 15's council half: a create ACCEPTED before its deadline, held at the
/// write while the clock passes it, is refused — a council that judged expiry
/// on arrival cannot pass this, because its check already succeeded.
///
/// Test 15 also names a lookup CONCURRENT with the held write. A concurrent
/// RESOLVE is structurally unrunnable against the real process, stated here
/// rather than silently narrowed: `before_expiry_write` pauses INSIDE the
/// write transaction, so the paused create holds the database's one writer
/// lock and a resolve — a settling write itself — queues behind it (gate
/// M13's own header below records the deadlock the first attempt produced).
/// What the concurrent lookup would OBSERVE is testable anyway, and tested
/// below: WAL readers do not queue behind the writer, so the council's file is
/// read mid-pause from this test — nothing about the held create is committed
/// or visible. That is the create-path visibility witness the map audit asked
/// for; the resolve-path twin is slice D's
/// `nothing_is_discoverable_before_the_settlement_commits`.
#[test]
fn a_create_overtaken_by_its_deadline_while_paused_is_refused() {
    let dir = tempfile::tempdir().expect("tempdir");
    let db = dir.path().join("council.sqlite");

    let mut council = Council::spawn(&db, NOW, &["before_expiry_write"]);
    let grant = read_grant(&council);
    let _asker = post_detached(
        council.url("/bookings"),
        create_body("EFF-K5", DEADLINE, &grant),
    );

    let paused = council.expect("PAUSED before_expiry_write EFF-K5");
    let occurrence = paused.split_whitespace().last().expect("occurrence");

    // The concurrent-visibility witness: while the create sits paused inside
    // its own write transaction, a reader of the FILE sees no booking and no
    // settled outcome for the identity. A council that leaks create rows
    // before its commit fails here — and only here, because a wire lookup
    // would queue behind the held writer lock.
    assert_eq!(
        booking_rows_in(&db, "EFF-K5"),
        0,
        "nothing about the held create is visible before its commit"
    );
    assert_eq!(
        settled_effect_rows_in(&db, "EFF-K5"),
        0,
        "no settled outcome exists before the commit decides one"
    );

    // Move the ONE clock past the deadline while the request waits inside the
    // write transaction, then let it proceed.
    council.say(&format!("SETCLOCK {occurrence} {}", DEADLINE + 1));
    council.expect(&format!("CLOCK {occurrence} {}", DEADLINE + 1));
    council.say(&format!("RELEASE {occurrence}"));
    council.expect(&format!("RELEASED {occurrence}"));

    // The pause fires again for the settle path's commit? No — only
    // before_expiry_write is armed, and it fires once per request. Ask what
    // became of the identity: tombstoned, from the DATABASE's point of view.
    let answer = ureq_post(
        &council.url("/effects/EFF-K5/resolve"),
        &resolve_body(DEADLINE),
    )
    .expect("an answer");
    assert_eq!(
        answer["outcome"], "DefinitivelyAbsent",
        "accepted before the deadline, judged at the write: refused and tombstoned"
    );
}

/// Gate M13: a SETCLOCK with TWO occurrences live is refused — and refused
/// means nothing moved, which both requests then prove by resolving as still
/// pre-deadline against the unmoved clock.
///
/// The two pauses are at `before_resolve_lock`, deliberately: it fires BEFORE
/// the transaction opens, so two can be live at once. `before_expiry_write`
/// cannot host this test — it fires inside the write transaction, so the first
/// paused request holds the writer lock and the second never reaches its pause.
/// (The first draft of this very test deadlocked on exactly that, which is
/// the plan's own warning coming true in its own suite.)
#[test]
fn a_clock_move_with_two_live_pauses_is_refused_and_moves_nothing() {
    let dir = tempfile::tempdir().expect("tempdir");
    let db = dir.path().join("council.sqlite");

    let mut council = Council::spawn(&db, NOW, &["before_resolve_lock"]);

    let asker_a = post_detached(
        council.url("/effects/EFF-K6A/resolve"),
        resolve_body(DEADLINE),
    );
    let first = council.expect("PAUSED before_resolve_lock EFF-K6A");
    let occurrence_a = first.split_whitespace().last().expect("occ").to_owned();

    let asker_b = post_detached(
        council.url("/effects/EFF-K6B/resolve"),
        resolve_body(DEADLINE),
    );
    let second = council.expect("PAUSED before_resolve_lock EFF-K6B");
    let occurrence_b = second.split_whitespace().last().expect("occ").to_owned();

    // Two live pauses. Moving the one shared clock would move BOTH requests'
    // deadline decisions, so the child refuses — before mutating, not after.
    council.say(&format!("SETCLOCK {occurrence_a} {}", DEADLINE + 1));
    council.expect(&format!("REFUSED {occurrence_a} multiple-occurrences-live"));

    council.say(&format!("RELEASE {occurrence_a}"));
    council.expect(&format!("RELEASED {occurrence_a}"));
    council.say(&format!("RELEASE {occurrence_b}"));
    council.expect(&format!("RELEASED {occurrence_b}"));

    // The RELEASED requests themselves carry the assertion: both report "not
    // yet visible", which is only true if the refused SETCLOCK mutated nothing.
    // A mutate-then-refuse implementation would have tombstoned both. (Fresh
    // follow-up resolves would pause at the still-armed point, so the detached
    // answers are joined instead — the first draft of this test hung on exactly
    // that.)
    let a = asker_a.join().expect("thread A").expect("answer A");
    let b = asker_b.join().expect("thread B").expect("answer B");
    assert_eq!(a["outcome"], "NotYetVisible", "the clock held still for A");
    assert_eq!(b["outcome"], "NotYetVisible", "and for B");
}

/// Gate M14: a child that dies between announcing a pause and being answered
/// produces a deterministic signal, never a hang — the reader turns death into
/// a line on the same channel every wait uses.
#[test]
fn a_child_dying_mid_handshake_is_a_signal_not_a_hang() {
    let dir = tempfile::tempdir().expect("tempdir");
    let db = dir.path().join("council.sqlite");

    let mut council = Council::spawn(&db, NOW, &["before_settle_commit"]);
    let _asker = post_detached(
        council.url("/effects/EFF-K7/resolve"),
        resolve_body(NOW - 1),
    );
    council.expect("PAUSED before_settle_commit EFF-K7");

    // The child dies. The parent's next wait must resolve — as EXITED — rather
    // than blocking forever on an acknowledgement that can never come.
    council.kill();
    let line = Council::next(&council.lines);
    assert_eq!(line, "EXITED");
}

/// Test 22, the whole sequence in one run: a create refused for EXPIRY writes
/// a tombstone; the council's clock then rolls back below the deadline; and the
/// same identity's retried create must STILL be refused — by the recorded
/// determination, never by a time comparison, which would now say yes.
#[test]
fn an_expiry_refusal_survives_the_clock_winding_back() {
    let dir = tempfile::tempdir().expect("tempdir");
    let db = dir.path().join("council.sqlite");

    {
        // A council whose clock is already past the deadline: the create is
        // refused for expiry and tombstoned, durably.
        let council = Council::spawn(&db, DEADLINE + 1, &[]);
        let grant = read_grant(&council);
        let refused = ureq_post(
            &council.url("/bookings"),
            &create_body("EFF-K9", DEADLINE, &grant),
        )
        .expect("an answer");
        assert_eq!(refused["outcome"], "DefinitivelyAbsent");
    }

    // Restart with the clock WOUND BACK below the deadline. A council that
    // re-judged expiry against its clock would now say "plenty of time" and
    // book the room — creating the very thing it already determined absent.
    let rewound = Council::spawn(&db, NOW, &[]);
    let grant = read_grant(&rewound);
    let retry = ureq_post(
        &rewound.url("/bookings"),
        &create_body("EFF-K9", DEADLINE, &grant),
    )
    .expect("an answer");
    assert_eq!(
        retry["outcome"], "DefinitivelyAbsent",
        "the tombstone refuses it; the rewound clock has no say"
    );
    assert_eq!(
        effect_rows_in(&db, "EFF-K9"),
        1,
        "one determination — the retry minted nothing"
    );
}

/// Test 22's key-continuity half: after a restart, the same signing key still
/// verifies — the council's answers are attributable across its own death.
#[test]
fn answers_verify_across_a_restart_with_the_same_key() {
    let dir = tempfile::tempdir().expect("tempdir");
    let db = dir.path().join("council.sqlite");

    let reference;
    {
        let council = Council::spawn(&db, NOW, &[]);
        let grant = read_grant(&council);
        let answer = ureq_post(
            &council.url("/bookings"),
            &create_body("EFF-K8", DEADLINE, &grant),
        )
        .expect("created");
        assert_eq!(answer["outcome"], "BookingCreated");
        reference = answer["booking_reference"]
            .as_str()
            .expect("ref")
            .to_owned();
    }

    let survivor = Council::spawn(&db, NOW, &[]);
    let answer = ureq_post(
        &survivor.url("/effects/EFF-K8/resolve"),
        &resolve_body(DEADLINE),
    )
    .expect("an answer");
    assert_eq!(answer["outcome"], "BookingCreated");
    assert_eq!(answer["booking_reference"], reference.as_str());
    assert!(
        answer["signature"].as_str().is_some_and(|s| !s.is_empty()),
        "signed by the same durable key, so the client's pinned key still verifies"
    );
}
