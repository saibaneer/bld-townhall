//! Both binaries, spawned — the real wire, nothing faked.
//!
//! A real crate rather than a tests/ module, because M6B needs it too and
//! test files cannot be imported across crates — this is the "no third copy"
//! promise, kept.
//!
//! Lifted out of `services/townhall-server/tests/http.rs` so M6A and M6B share
//! one, rather than the third copy this would otherwise become.
//!
//! Two properties carried across deliberately:
//!
//! - **No sleeps for readiness.** Each binary prints `READY <port>` once bound,
//!   and the harness reads that line. A sleep would be a race with a nicer name.
//! - **The SQLite shim panics on a failed query.** It used to end
//!   `.unwrap_or(0)`, which turned any malformed SQL into the answer `0` — and
//!   since most callers compare a count before and after an operation that
//!   should change nothing, a typo in a column name produced `0 == 0` and a
//!   confidently passing test that asserted nothing. One such witness shipped in
//!   M5.1 before it was caught.

// Test support: every panic here IS an assertion about the harness's own
// premises, so per-function `# Panics` sections would document the obvious.
#![allow(clippy::missing_panics_doc)]

use std::io::BufRead as _;
use std::process::{Child, Command, Stdio};

pub const KEY_HEX: &str = "0707070707070707070707070707070707070707070707070707070707070707";

/// The delegation envelope's authentication key, for every spawned world.
///
/// A DIFFERENT value from `KEY_HEX` on purpose: that one is the council's
/// signing key, and a test that accidentally passed one where the other
/// belonged would still work if they matched — and would keep working until the
/// day the two keys legitimately differed.
pub const AUTHORITY_KEY_HEX: &str =
    "a7a7a7a7a7a7a7a7a7a7a7a7a7a7a7a7a7a7a7a7a7a7a7a7a7a7a7a7a7a7a7a7";
pub const LUCY: &str = "dev-lucy";
pub const MARCO: &str = "dev-marco-restricted";
pub const PRIYA: &str = "dev-priya-nobook";

/// The publisher signing key a discoverable world serves its manifest under
/// (M9). Its own DISTINCT value — not the council key, not the authority key —
/// so a test that crossed the wires would fail rather than pass by coincidence.
/// A test derives the matching verifying key from this (to pin discovery), and
/// the gate witness re-signs a tampered manifest with it, exactly as the real
/// publisher would.
pub const MANIFEST_KEY_HEX: &str =
    "0d0d0d0d0d0d0d0d0d0d0d0d0d0d0d0d0d0d0d0d0d0d0d0d0d0d0d0d0d0d0d0d";

pub struct World {
    #[allow(dead_code)]
    dir: tempfile::TempDir,
    council: Child,
    pub council_url: String,
    pub council_db: std::path::PathBuf,
    /// The server's SQLite file — the shared pool bookings, authority rows and
    /// the usage ledger all live in. Exposed so a test can seed a row the way the
    /// composition root would (a channel binding, a low usage quota) before the
    /// server is asked. Empty for a `world()` that ran no townhall server.
    pub townhall_db: std::path::PathBuf,
    server: Option<Child>,
    pub server_url: String,
    /// The mock-stripe process, spawned only for a paying world (M10/M11). `None`
    /// for every other world.
    stripe: Option<Child>,
    /// The mock-stripe base URL, when a paying world spawned one.
    pub stripe_url: Option<String>,
}

impl World {
    /// Kill the council mid-test, so "the provider cannot be asked" is a state
    /// a test can put the world into rather than a sentence in a doc comment.
    pub fn kill_council(&mut self) {
        let _ = self.council.kill();
        let _ = self.council.wait();
    }
}

impl Drop for World {
    fn drop(&mut self) {
        if let Some(mut server) = self.server.take() {
            let _ = server.kill();
            let _ = server.wait();
        }
        if let Some(mut stripe) = self.stripe.take() {
            let _ = stripe.kill();
            let _ = stripe.wait();
        }
        let _ = self.council.kill();
        let _ = self.council.wait();
    }
}

/// Wait for `READY <port>`, or fail loudly with everything the child said.
///
/// # Why a deadline exists at all
///
/// It did not, and codex named the consequence before it happened: "any live
/// startup stall will hang indefinitely". It then happened — twice — as CI runs
/// that sat silent for twenty and ninety minutes on a check that takes
/// microseconds locally. A harness that can hang converts an unknown bug into
/// an unknowable one: the step times out with no output and the cause dies with
/// the runner.
///
/// Thirty seconds is generous for a process whose job is to print one line
/// after binding a port, and the panic carries the child's stderr, so the next
/// stall names itself in the CI log instead of being reconstructed from
/// timestamps.
fn spawn_ready(mut command: Command) -> (Child, u16) {
    use std::io::Read as _;
    use std::sync::mpsc;

    // Retry a TRANSIENT spawn failure. A full-workspace run spawns many servers
    // at once, and the OS can refuse a fork with EAGAIN ("resource temporarily
    // unavailable") when it does — a load symptom, not a broken binary, so a
    // short backoff clears it. A genuinely missing binary fails every attempt.
    command.stdout(Stdio::piped()).stderr(Stdio::piped());
    let mut child = {
        let mut attempt = 0;
        loop {
            match command.spawn() {
                Ok(child) => break child,
                Err(error) if attempt < 10 => {
                    attempt += 1;
                    std::thread::sleep(std::time::Duration::from_millis(100 * attempt));
                    let _ = error;
                }
                Err(error) => panic!("the binary spawns: {error}"),
            }
        }
    };
    let stdout = child.stdout.take().expect("piped stdout");
    let mut stderr = child.stderr.take().expect("piped stderr");

    // The read happens on its own thread so the deadline is real: a blocked
    // read on the main thread IS the hang this function exists to prevent.
    let (sender, receiver) = mpsc::channel();
    std::thread::spawn(move || {
        let mut lines = std::io::BufReader::new(stdout).lines();
        let first = lines.next().map(|line| line.expect("readable stdout"));
        let _ = sender.send(first);
        // Keep draining so the child never blocks on a full stdout pipe.
        for _ in lines {}
    });

    match receiver.recv_timeout(std::time::Duration::from_secs(30)) {
        Ok(Some(line)) => {
            let port = line
                .strip_prefix("READY ")
                .unwrap_or_else(|| panic!("expected READY, got {line:?}"))
                .parse()
                .expect("a port");
            (child, port)
        }
        Ok(None) => {
            // The child exited without a READY line: stdout hit EOF. Its
            // stderr says why, and that is the error worth reading.
            let _ = child.wait();
            let mut said = String::new();
            let _ = stderr.read_to_string(&mut said);
            panic!("the child exited before READY. stderr:\n{said}");
        }
        Err(_) => {
            // Still alive, still silent. Kill it, then report everything it
            // wrote — the difference between this panic and a hung CI step is
            // the entire reason this arm exists.
            let _ = child.kill();
            let _ = child.wait();
            let mut said = String::new();
            let _ = stderr.read_to_string(&mut said);
            panic!("no READY within 30s from a live child. stderr so far:\n{said}");
        }
    }
}

fn build_binaries() {
    for (package, features) in [
        ("mock-council", "test-faults"),
        ("townhall-server", "dev-authority"),
    ] {
        let status = Command::new(env!("CARGO"))
            .args(["build", "-p", package, "--features", features])
            .status()
            .expect("build");
        assert!(status.success(), "building {package} failed");
    }
}

fn build_mock_stripe() {
    let status = Command::new(env!("CARGO"))
        .args(["build", "-p", "mock-stripe"])
        .status()
        .expect("build mock-stripe");
    assert!(status.success(), "building mock-stripe failed");
}

fn target_dir() -> std::path::PathBuf {
    std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("../../target/debug")
}

/// The one workload credential the REAL resolver knows (main.rs:513) — a
/// workload, not a person. Authenticates the caller and authorizes nothing.
pub const WORKLOAD: &str = "agent-townhall";
/// The number Lucy's channel binds to in a real-authority world.
pub const LUCY_ADDR: &str = "+447700900123";

pub fn world() -> World {
    world_with(&[])
}

/// A dev-lane world whose server also serves a signed discovery manifest at
/// `GET /.well-known/bld`, signed with [`MANIFEST_KEY_HEX`] (M9). Discovery is
/// opt-in on `--manifest-key`, so the other worlds carry no manifest route.
pub fn world_discoverable() -> World {
    world_with(&["--manifest-key", MANIFEST_KEY_HEX])
}

/// A world whose server takes extra flags — the deterministic-429 seam
/// (`--reclassify-attempts`) is the caller that needs this.
pub fn world_with(extra: &[&str]) -> World {
    spawn_world(true, extra)
}

/// A world on the REAL authority resolver (no dev lane), with Lucy's channel
/// bound — for the M7C approve-first journey, where a booking needs a delegation
/// a person's `YES` issued, and the fixed workload token authorizes nothing.
///
/// The binding is written straight into the server's store the same way
/// `authority_lane.rs` binds it, but through the `sqlite3` CLI this crate already
/// shells out to — after the server has run its migrations, so the table exists.
pub fn world_real() -> World {
    spawn_world(false, &[])
}

/// As [`world_real`], with extra server flags — e.g. a tight `--global-budget-max`
/// so an M8-2 rate/budget ceiling is reachable in a few real turns.
#[must_use]
pub fn world_real_with(extra: &[&str]) -> World {
    spawn_world(false, extra)
}

fn spawn_world(dev_authority: bool, extra: &[&str]) -> World {
    build_binaries();
    let dir = tempfile::tempdir().expect("tempdir");
    let council_db = dir.path().join("council.sqlite");

    let mut command = Command::new(target_dir().join("mock-council"));
    command
        .arg("--db")
        .arg(&council_db)
        .args(["--key-hex", KEY_HEX, "--port", "0"]);
    let (council, council_port) = spawn_ready(command);
    let council_url = format!("http://127.0.0.1:{council_port}");

    let townhall_db = dir.path().join("townhall.sqlite");
    let mut command = Command::new(target_dir().join("townhall-server"));
    command
        .arg("--db")
        .arg(&townhall_db)
        .arg("--denials-db")
        .arg(dir.path().join("denials.sqlite"))
        .args([
            "--council-url",
            &council_url,
            "--key-hex",
            KEY_HEX,
            "--port",
            "0",
            // The envelope key. Fixed, because these worlds are restarted
            // mid-test (the chase-survives-a-restart cases) and a grant issued
            // before the restart has to still verify after it — which is the
            // whole reason the key is configuration and not a per-process
            // accident.
            "--authority-key",
            AUTHORITY_KEY_HEX,
            "--retry-cadence-ms",
            "200",
            "--reconcile-interval-ms",
            "100",
        ]);
    if dev_authority {
        command.arg("--dev-authority");
    }
    command.args(extra);
    let (server, server_port) = spawn_ready(command);

    if !dev_authority {
        bind_channels(&townhall_db);
    }

    World {
        dir,
        council,
        council_url,
        council_db,
        townhall_db,
        server: Some(server),
        server_url: format!("http://127.0.0.1:{server_port}"),
        stripe: None,
        stripe_url: None,
    }
}

/// A dev-lane world that is BOTH discoverable (a signed `/.well-known/bld`
/// manifest, [`MANIFEST_KEY_HEX`]) AND payments-enabled (M11): council +
/// mock-stripe + a server with `--enable-payments` and `--payment-threshold-pence`
/// so a below-authority booking still routes through the human-payment handoff.
/// The two Stripe secrets go through the child's ENVIRONMENT, exactly as the
/// composition root reads them.
#[must_use]
pub fn world_paying_discoverable(threshold_pence: u64) -> World {
    build_binaries();
    build_mock_stripe();
    let dir = tempfile::tempdir().expect("tempdir");

    let council_db = dir.path().join("council.sqlite");
    let mut command = Command::new(target_dir().join("mock-council"));
    command
        .arg("--db")
        .arg(&council_db)
        .args(["--key-hex", KEY_HEX, "--port", "0"]);
    let (council, council_port) = spawn_ready(command);
    let council_url = format!("http://127.0.0.1:{council_port}");

    let mut command = Command::new(target_dir().join("mock-stripe"));
    command.args(["--port", "0"]);
    let (stripe, stripe_port) = spawn_ready(command);
    let stripe_url = format!("http://127.0.0.1:{stripe_port}");

    let townhall_db = dir.path().join("townhall.sqlite");
    let threshold = threshold_pence.to_string();
    let mut command = Command::new(target_dir().join("townhall-server"));
    command
        .arg("--db")
        .arg(&townhall_db)
        .arg("--denials-db")
        .arg(dir.path().join("denials.sqlite"))
        .args([
            "--council-url",
            &council_url,
            "--key-hex",
            KEY_HEX,
            "--port",
            "0",
            "--authority-key",
            AUTHORITY_KEY_HEX,
            "--retry-cadence-ms",
            "200",
            "--reconcile-interval-ms",
            "100",
            "--dev-authority",
            "--manifest-key",
            MANIFEST_KEY_HEX,
            "--enable-payments",
            "--payment-threshold-pence",
            &threshold,
        ])
        .env("STRIPE_SECRET_KEY", "sk_test_townhall_testkit")
        .env("STRIPE_WEBHOOK_SECRET", "whsec_townhall_testkit")
        .env("STRIPE_BASE_URL", &stripe_url)
        .env("STRIPE_SUCCESS_URL", "https://townhall.test/paid")
        .env("STRIPE_CANCEL_URL", "https://townhall.test/cancelled");
    let (server, server_port) = spawn_ready(command);

    World {
        dir,
        council,
        council_url,
        council_db,
        townhall_db,
        server: Some(server),
        server_url: format!("http://127.0.0.1:{server_port}"),
        stripe: Some(stripe),
        stripe_url: Some(stripe_url),
    }
}

/// Bind Lucy's and Priya's channels in a real-authority world's store, so a
/// challenge raised against either number can be answered. `sms-reply` is
/// `AssuranceLevel::SmsReply.name()`, and the row shape is migration 0006's.
///
/// Both rows go in ONE `sqlite3` invocation — the CLI this crate already shells
/// out to — after the server has run its migrations (so the table exists) and in
/// a single process, because a full-workspace run already spawns many.
fn bind_channels(townhall_db: &std::path::Path) {
    let row = |id: &str, address: &str, principal: &str| {
        format!(
            "INSERT INTO channel_bindings \
             (id, address, principal, version, status, assurance, evidence, \
              verified_at_ms, created_at_ms, updated_at_ms) \
             VALUES ('{id}', '{address}', '{principal}', 1, 'active', 'sms-reply', \
              NULL, 1700000000000, 1700000000000, 1700000000000);"
        )
    };
    let sql = format!(
        "{}{}",
        row("binding-lucy", "+447700900123", "lucy"),
        row("binding-priya", "+447700900456", "priya"),
    );
    let output = Command::new("sqlite3")
        .arg(townhall_db)
        .arg(&sql)
        .output()
        .expect("sqlite3 runs");
    assert!(
        output.status.success(),
        "binding channels failed: {}",
        String::from_utf8_lossy(&output.stderr)
    );
}

/// Seed a usage account with a LOW quota, so an exhausted-quota state is reachable
/// in a test without driving the default (generous) ceiling. The id matches what
/// `UsageService` derives from the principal (`usage-<principal>`), and the
/// service's `open_account` is idempotent, so this pre-seeded low limit stands.
/// Run after `world_real()` (the migrations have applied), before the first turn.
pub fn seed_usage_quota(townhall_db: &std::path::Path, principal: &str, limit_units: i64) {
    let sql = format!(
        "INSERT INTO usage_accounts \
         (id, principal, status, limit_units, reserved_units, debited_units, \
          created_at_ms, updated_at_ms) \
         VALUES ('usage-{principal}', '{principal}', 'active', {limit_units}, 0, 0, \
          1700000000000, 1700000000000);"
    );
    let output = Command::new("sqlite3")
        .arg(townhall_db)
        .arg(&sql)
        .output()
        .expect("sqlite3 runs");
    assert!(
        output.status.success(),
        "seeding usage quota failed: {}",
        String::from_utf8_lossy(&output.stderr)
    );
}

/// The units debited to a principal's usage account — the authoritative witness
/// that a turn metered (or that a cancellation did NOT). `0` if no account row
/// exists yet.
#[must_use]
pub fn usage_debited(townhall_db: &std::path::Path, principal: &str) -> i64 {
    let sql = format!("SELECT debited_units FROM usage_accounts WHERE principal = '{principal}';");
    let output = Command::new("sqlite3")
        .arg(townhall_db)
        .arg(&sql)
        .output()
        .expect("sqlite3 runs");
    assert!(
        output.status.success(),
        "reading usage_debited failed: {}",
        String::from_utf8_lossy(&output.stderr)
    );
    String::from_utf8_lossy(&output.stdout)
        .trim()
        .parse()
        .unwrap_or(0)
}

/// A count from the council's own database.
///
/// Strict on purpose: see the module note. A witness that cannot fail is worse
/// than no witness, because it occupies the space where a real one would go.
pub fn council_count(world: &World, sql: &str) -> i64 {
    let output = Command::new("sqlite3")
        .arg(&world.council_db)
        .arg(sql)
        .output()
        .expect("sqlite3 available");
    assert!(
        output.status.success(),
        "sqlite3 rejected {sql:?}: {}",
        String::from_utf8_lossy(&output.stderr)
    );
    let text = String::from_utf8_lossy(&output.stdout);
    text.trim()
        .parse::<i64>()
        .unwrap_or_else(|_| panic!("{sql:?} did not return a count, got {text:?}"))
}

/// Arm one of the council's injected faults for one effect identity.
///
/// Returns the council's own fault id — an index, so it is legitimately `0` for
/// the first fault armed and cannot itself witness anything. The witness is
/// [`fault_fired`], asked afterwards: a test that expects a 202 and whose fault
/// never fired is asserting against a system that does not exist, and would
/// otherwise fail somewhere far from the cause.
pub async fn arm_fault(world: &World, effect: &str, route: &str, fault: &str) -> u64 {
    let response: serde_json::Value = reqwest::Client::new()
        .post(format!("{}/test/faults", world.council_url))
        .json(&serde_json::json!({
            "effect_intent_id": effect,
            "route": route,
            "fault": fault,
        }))
        .send()
        .await
        .expect("arm")
        .json()
        .await
        .expect("json");
    response["fault_id"]
        .as_u64()
        .unwrap_or_else(|| panic!("the council did not arm {fault} on {effect}: {response}"))
}

/// How many times an armed fault was actually consumed.
///
/// The one that matters. An armed-but-never-fired fault leaves the council
/// answering normally, so the turn settles synchronously and a test expecting
/// acceptance fails with no hint that its premise never held.
pub async fn fault_fired(world: &World, fault_id: u64) -> u64 {
    let response: serde_json::Value = reqwest::Client::new()
        .get(format!("{}/test/faults/{fault_id}", world.council_url))
        .send()
        .await
        .expect("status")
        .json()
        .await
        .expect("json");
    response["consumed"].as_u64().unwrap_or_default()
}

/// A recording proxy in front of the server: every request line, in order.
///
/// This exists because counting COUNCIL ROWS cannot witness "never re-POSTs" —
/// the council is idempotent on effect identity, so an erroneous second POST
/// leaves exactly one row and the wrong implementation passes. The witness has
/// to be the requests themselves.
///
/// One request per connection, enforced by injecting `Connection: close` both
/// ways — which trades keep-alive for a proxy simple enough to be obviously
/// correct in a test harness.
pub struct RecordingProxy {
    pub url: String,
    requests: std::sync::Arc<std::sync::Mutex<Vec<String>>>,
}

impl RecordingProxy {
    pub fn in_front_of(upstream_url: &str) -> Self {
        let upstream = upstream_url
            .strip_prefix("http://")
            .expect("an http upstream")
            .to_owned();
        let listener = std::net::TcpListener::bind("127.0.0.1:0").expect("bind");
        let port = listener.local_addr().expect("addr").port();
        let requests = std::sync::Arc::new(std::sync::Mutex::new(Vec::new()));

        let log = std::sync::Arc::clone(&requests);
        std::thread::spawn(move || {
            for client in listener.incoming().flatten() {
                let upstream = upstream.clone();
                let log = std::sync::Arc::clone(&log);
                std::thread::spawn(move || forward_once(client, &upstream, &log));
            }
        });

        Self {
            url: format!("http://127.0.0.1:{port}"),
            requests,
        }
    }

    /// Every request line seen so far, e.g. `POST /booking-intents/x/behaviours/book`.
    pub fn requests(&self) -> Vec<String> {
        self.requests
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .clone()
    }

    /// How many requests matched a `METHOD path-substring` pair.
    pub fn count(&self, method: &str, path_fragment: &str) -> usize {
        self.requests()
            .iter()
            .filter(|line| line.starts_with(method) && line.contains(path_fragment))
            .count()
    }
}

fn forward_once(
    mut client: std::net::TcpStream,
    upstream: &str,
    log: &std::sync::Mutex<Vec<String>>,
) {
    use std::io::{Read as _, Write as _};

    // Read the request head, then exactly Content-Length of body.
    let mut buffer = Vec::new();
    let mut chunk = [0_u8; 4096];
    let head_end = loop {
        let n = client.read(&mut chunk).unwrap_or(0);
        if n == 0 {
            return;
        }
        buffer.extend_from_slice(&chunk[..n]);
        if let Some(pos) = buffer.windows(4).position(|w| w == b"\r\n\r\n") {
            break pos + 4;
        }
    };
    let head = String::from_utf8_lossy(&buffer[..head_end]).to_string();
    let request_line = head.lines().next().unwrap_or_default();
    let (method_path, _) = request_line.rsplit_once(' ').unwrap_or((request_line, ""));
    log.lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner)
        .push(method_path.to_owned());

    let content_length: usize = head
        .lines()
        .find_map(|line| {
            let (name, value) = line.split_once(':')?;
            name.eq_ignore_ascii_case("content-length")
                .then(|| value.trim().parse().ok())?
        })
        .unwrap_or(0);
    while buffer.len() < head_end + content_length {
        let n = client.read(&mut chunk).unwrap_or(0);
        if n == 0 {
            break;
        }
        buffer.extend_from_slice(&chunk[..n]);
    }

    // Rewrite the head: force Connection: close so both sides speak one
    // request per connection, which is what lets this proxy stream the
    // response until EOF instead of parsing framing.
    let mut rewritten = String::new();
    for line in head[..head.len() - 2].lines() {
        if line.is_empty() {
            continue;
        }
        let lowered = line.to_ascii_lowercase();
        if lowered.starts_with("connection:") {
            continue;
        }
        rewritten.push_str(line);
        rewritten.push_str("\r\n");
    }
    rewritten.push_str("connection: close\r\n\r\n");

    let Ok(mut server) = std::net::TcpStream::connect(upstream) else {
        let _ = client.write_all(b"HTTP/1.1 502 Bad Gateway\r\ncontent-length: 0\r\n\r\n");
        return;
    };
    let _ = server.write_all(rewritten.as_bytes());
    let _ =
        server.write_all(&buffer[head_end..head_end + content_length.min(buffer.len() - head_end)]);

    // Stream the whole response back until the server closes.
    loop {
        match server.read(&mut chunk) {
            Ok(0) | Err(_) => break,
            Ok(n) => {
                if client.write_all(&chunk[..n]).is_err() {
                    break;
                }
            }
        }
    }
}

/// The RESOLVED, TRANSITIVE dependency names of one workspace package.
///
/// `resolve.nodes`, not `packages[].dependencies` — the declared list is what a
/// manifest says; the resolved list is what the resolver linked. And it is the
/// LINKED CLOSURE, not the direct edges: a forbidden crate that arrives through a
/// dependency's re-export (`bld-manifest` gaining `pub use bld_types::Behaviour`)
/// links into the subject just the same, and a direct-deps-only check would miss
/// it while its own docstring claimed otherwise. This walks the graph so the
/// promise the boundary tests rest on — "cannot NAME the forbidden crate" — is the
/// one actually enforced.
///
/// `kinds` selects which of the SUBJECT's own edges to enter by (callers pass
/// `["normal"]` for the shipping graph, so a dev-only dependency like this testkit
/// never counts). Past that first hop the walk follows NORMAL edges only: a
/// dependency's normal deps are linked into the subject's build; its dev-deps are
/// not, and its build-deps are compiled but their types are not nameable from the
/// subject's code — which is exactly what "cannot name" is about.
pub fn resolved_dependencies(package: &str, kinds: &[&str]) -> Vec<String> {
    let output = Command::new(env!("CARGO"))
        .args(["metadata", "--format-version", "1"])
        .output()
        .expect("cargo metadata");
    assert!(output.status.success());
    let metadata: serde_json::Value =
        serde_json::from_slice(&output.stdout).expect("metadata json");

    let subject_id = metadata["packages"]
        .as_array()
        .expect("packages")
        .iter()
        .find(|p| p["name"] == package)
        .unwrap_or_else(|| panic!("{package} not in the workspace"))["id"]
        .as_str()
        .expect("id")
        .to_owned();

    let nodes = metadata["resolve"]["nodes"].as_array().expect("nodes");
    let node_of = |id: &str| -> Option<&serde_json::Value> {
        nodes.iter().find(|n| n["id"].as_str() == Some(id))
    };
    // A node carries its own package id, not its name; names come from `packages`.
    let name_of = |id: &str| -> Option<String> {
        metadata["packages"]
            .as_array()
            .expect("packages")
            .iter()
            .find(|p| p["id"].as_str() == Some(id))
            .and_then(|p| p["name"].as_str())
            .map(|name| name.replace('_', "-"))
    };
    // An edge is entered if any of its kinds is in `want` (a missing `dep_kinds`,
    // from an older cargo, is a normal edge).
    let edge_matches = |dep: &serde_json::Value, want: &[&str]| -> bool {
        dep["dep_kinds"].as_array().is_none_or(|dep_kinds| {
            dep_kinds.iter().any(|dk| {
                let kind = dk["kind"].as_str().unwrap_or("normal");
                want.contains(&kind)
            })
        })
    };

    // BFS the linked closure: the subject's edges by `kinds`, everything deeper by
    // normal edges. `reached` holds package ids, never the subject itself.
    let mut reached: std::collections::BTreeSet<String> = std::collections::BTreeSet::new();
    let mut queue: std::collections::VecDeque<(String, bool)> = std::collections::VecDeque::new();
    queue.push_back((subject_id.clone(), true));
    let mut visited: std::collections::BTreeSet<String> = std::collections::BTreeSet::new();
    visited.insert(subject_id);

    while let Some((id, is_subject)) = queue.pop_front() {
        let Some(node) = node_of(&id) else { continue };
        let want: &[&str] = if is_subject { kinds } else { &["normal"] };
        for dep in node["deps"].as_array().into_iter().flatten() {
            if !edge_matches(dep, want) {
                continue;
            }
            let Some(pkg) = dep["pkg"].as_str() else {
                continue;
            };
            if visited.insert(pkg.to_owned()) {
                queue.push_back((pkg.to_owned(), false));
            }
            if let Some(name) = name_of(pkg) {
                reached.insert(name);
            }
        }
    }
    reached.into_iter().collect()
}

/// How a test obtains authority.
///
/// # Why this exists rather than a constructor
///
/// `VerifiedAuthority` has private fields and no public constructor, and
/// ADR-025 refused to add a `test-support` one: a cargo feature that reveals a
/// minting path leaks through feature unification, so it would close the
/// backdoor only on paper. The rule that replaced it is that **tests obtain
/// authority the way production does** — by answering a real challenge.
///
/// This module is the whole cost of that rule, paid once. It runs the genuine
/// `begin → submit` path against an in-memory store, so a test's grant is
/// exactly as real as Lucy's, and a change that breaks issuance breaks every
/// test that depends on authority rather than silently keeping them green.
pub mod issuer {
    use bld_types::{
        ActorId, Behaviour, BookingId, BookingRequirements, Money, PrincipalId, ServiceId,
        TimeWindow,
    };
    use townhall_authority::{
        ApprovalCode, ApprovalRequest, AssuranceLevel, AuthorityPolicy, AuthorityService,
        BehaviourSet, BindingRef, Entropy, EnvelopeKey, InboundEvidenceRecord, MemoryApprovalStore,
        PendingScope, VerifiedAuthority,
    };

    /// The instant every issued test grant is stamped with.
    ///
    /// Fixed rather than `now()`, so a test that asserts on expiry has
    /// something to assert against and nothing here drifts with the wall clock.
    pub const ISSUED_AT_MS: u64 = 1_700_000_000_000;

    /// How long a test grant lasts. An hour: long enough that no test has to
    /// think about it, short enough to be a real deadline.
    pub const GRANT_TTL_MS: u64 = 3_600_000;

    /// What to ask for.
    #[derive(Clone, Debug)]
    pub struct GrantSpec {
        /// On whose behalf — the booking's owner, and the visibility scope.
        pub grantor: PrincipalId,
        /// Who the action is attributed to. Equal to `grantor` unless the test
        /// is about delegation.
        pub subject: PrincipalId,
        /// The exact resource. A grant reaches this booking and no other.
        pub booking: BookingId,
        pub behaviours: Vec<Behaviour>,
        pub max_fee: Money,
        /// The headcount ceiling the grant approves.
        ///
        /// Defaults to the fixture requirements' own 20, which is what a real
        /// approval would carry. A test whose subject is CHANGING the headcount
        /// raises it deliberately — see [`GrantSpec::seating`].
        pub max_attendees: u16,
    }

    /// Everything a booking walk needs, end to end.
    ///
    /// # Why this is the default and not `[Book, Cancel]`
    ///
    /// Getting a booking to `Book` takes `SelectVenue`, `VerifySlot` and
    /// sometimes `RevalidateVenue` first. Until M7B those were ungated, so a
    /// grant naming only `Book` walked the whole way regardless — a fixture
    /// that looked narrow and was not. Now the grant is consulted for every
    /// proposal, so a fixture has to name what it actually does.
    ///
    /// `UpdateRequirements` and `ChangeVenue` are deliberately ABSENT: they
    /// change what a person approved, so a test that wants them says so.
    pub const WALK: &[Behaviour] = &[
        Behaviour::SelectVenue,
        Behaviour::VerifySlot,
        Behaviour::RevalidateVenue,
        Behaviour::Book,
        Behaviour::Cancel,
    ];

    /// Every behaviour there is.
    ///
    /// For fixtures whose subject is something OTHER than authority — the
    /// topology matrix, the characterization suite — where a narrow grant would
    /// make a test fail for a reason it is not about. A test that IS about
    /// authority names its behaviours explicitly instead.
    pub const ALL: &[Behaviour] = &[
        Behaviour::SelectVenue,
        Behaviour::VerifySlot,
        Behaviour::ChangeVenue,
        Behaviour::UpdateRequirements,
        Behaviour::RevalidateVenue,
        Behaviour::Book,
        Behaviour::Cancel,
    ];

    impl GrantSpec {
        /// The ordinary case: one person, their own booking, the whole walk.
        #[must_use]
        pub fn own(principal: &str, booking: &str, max_fee_pence: u64) -> Self {
            Self {
                grantor: PrincipalId::new(principal),
                subject: PrincipalId::new(principal),
                booking: BookingId::new(booking),
                behaviours: WALK.to_vec(),
                max_fee: Money::from_pence(max_fee_pence),
                max_attendees: 20,
            }
        }

        /// Someone acting on another person's booking (ADR-020, ADR-025).
        #[must_use]
        pub fn delegated(grantor: &str, subject: &str, booking: &str, max_fee_pence: u64) -> Self {
            Self {
                grantor: PrincipalId::new(grantor),
                subject: PrincipalId::new(subject),
                booking: BookingId::new(booking),
                behaviours: vec![Behaviour::Cancel],
                max_fee: Money::from_pence(max_fee_pence),
                max_attendees: 20,
            }
        }

        /// Narrow the grant to exactly these behaviours.
        #[must_use]
        pub fn permitting(mut self, behaviours: &[Behaviour]) -> Self {
            self.behaviours = behaviours.to_vec();
            self
        }

        /// Approve a different headcount ceiling.
        ///
        /// For fixtures whose subject IS changing the headcount — the domain's
        /// characterization suite raises attendees to prove revalidation
        /// happens, and would otherwise be refused by the approval ceiling for a
        /// reason it is not about.
        #[must_use]
        pub fn seating(mut self, attendees: u16) -> Self {
            self.max_attendees = attendees;
            self
        }
    }

    struct OneCode;

    impl Entropy for OneCode {
        fn code(&self) -> ApprovalCode {
            ApprovalCode::new("7312").expect("four digits")
        }

        fn identifier(&self) -> String {
            // Every call builds its own store, so a fixed identifier cannot
            // collide with anything.
            "testkit-issued".to_owned()
        }
    }

    /// Issue a grant by answering a real challenge.
    ///
    /// # Panics
    /// If issuance fails — which would mean the approval path itself is broken,
    /// and every test resting on it should say so loudly rather than proceed.
    #[must_use]
    pub async fn issue(spec: &GrantSpec) -> VerifiedAuthority {
        let store = std::sync::Arc::new(MemoryApprovalStore::new());
        // The grantor's channel is BOUND before the challenge is answered.
        //
        // Not scaffolding. Review found the verifier comparing the caller's
        // claimed binding against the caller's own earlier claim, so it checks
        // against a row now — which means a test grant needs a binding to
        // exist, exactly as a real approval does. Every grant in the workspace
        // therefore travels the path a person's approval travels.
        store.bind(&spec.grantor, 1);
        let service = AuthorityService::new(
            std::sync::Arc::clone(&store),
            OneCode,
            AuthorityPolicy {
                reply_window_ms: 600_000,
                grant_ttl_ms: GRANT_TTL_MS,
                assurance: AssuranceLevel::SmsReply,
            },
            // A fixed key: every grant a test holds was signed by the same
            // issuer, which is the only property tests need from it.
            EnvelopeKey::new(vec![0xA7; 32]).expect("32 bytes"),
        );
        let binding = BindingRef {
            principal: spec.grantor.clone(),
            version: 1,
        };
        let actor = ActorId::new("agent:townhall");
        let (_, raised) = service
            .begin(
                &ApprovalRequest {
                    scope: PendingScope {
                        service: ServiceId::new("demo-council-town-hall"),
                        agent: "townhall-agent".to_owned(),
                        booking: spec.booking.clone(),
                        behaviours: BehaviourSet::new(spec.behaviours.clone()),
                        requirements: BookingRequirements {
                            purpose: "meeting".to_owned(),
                            requested_date: "2026-08-20".to_owned(),
                            time_window: TimeWindow {
                                from: "13:00".to_owned(),
                                to: "17:00".to_owned(),
                            },
                            attendees: spec.max_attendees,
                            wheelchair_accessible: true,
                            max_fee: spec.max_fee,
                        },
                    },
                    binding: binding.clone(),
                    grantor: spec.grantor.clone(),
                    subject: spec.subject.clone(),
                    // Every test grant names one workload, so a test that cares
                    // about the actor check has a value to disagree with.
                    actor: actor.clone(),
                },
                ISSUED_AT_MS,
            )
            .await
            .expect("a challenge can always be raised against an empty store");
        // The grant travels the receipt seam a person's approval travels: the
        // trusted ingress deposits the reply's evidence under a one-use receipt,
        // and the issuer forwards the receipt — never fabricated evidence.
        let address = format!("+{}", spec.grantor.as_str());
        let (_challenge, receipt) = service
            .deposit_evidence(
                &address,
                &InboundEvidenceRecord {
                    provider: "sim".to_owned(),
                    provider_account: "townhall".to_owned(),
                    provider_message_id: format!("msg-{}", raised.id.as_str()),
                    claimed_sender: address.clone(),
                    verified: true,
                    signature: None,
                },
                ISSUED_AT_MS + 500,
                600_000,
            )
            .await
            .expect("the bound channel is awaiting this challenge");
        service
            .submit(&raised.id, "7312", &actor, &receipt, ISSUED_AT_MS + 1_000)
            .await
            .expect("the right code, from the bound channel, inside the window")
    }

    /// [`issue`], callable from a synchronous test.
    ///
    /// # Why a thread
    ///
    /// These helpers are called from both `#[test]` and `#[tokio::test]`
    /// bodies, and `block_on` inside a running runtime panics. A thread with
    /// its own runtime works from either, and the cost is paid once per grant.
    ///
    /// # Panics
    /// As [`issue`], or if the issuing thread itself panics.
    #[must_use]
    pub fn issue_blocking(spec: &GrantSpec) -> VerifiedAuthority {
        let spec = spec.clone();
        std::thread::spawn(move || {
            tokio::runtime::Builder::new_current_thread()
                .build()
                .expect("a test runtime")
                .block_on(async move { issue(&spec).await })
        })
        .join()
        .expect("the issuing thread did not panic")
    }
}
