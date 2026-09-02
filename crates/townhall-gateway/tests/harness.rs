//! Both binaries, spawned — the real wire, nothing faked.
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
pub const LUCY: &str = "dev-lucy";
pub const MARCO: &str = "dev-marco-restricted";
pub const PRIYA: &str = "dev-priya-nobook";

pub struct World {
    #[allow(dead_code)]
    dir: tempfile::TempDir,
    council: Child,
    pub council_url: String,
    pub council_db: std::path::PathBuf,
    server: Option<Child>,
    pub server_url: String,
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
        let _ = self.council.kill();
        let _ = self.council.wait();
    }
}

fn spawn_ready(mut command: Command) -> (Child, u16) {
    let mut child = command
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .spawn()
        .expect("spawn");
    let stdout = child.stdout.take().expect("piped stdout");
    let mut lines = std::io::BufReader::new(stdout).lines();
    let ready = lines.next().expect("a line").expect("readable");
    let port: u16 = ready
        .strip_prefix("READY ")
        .unwrap_or_else(|| panic!("expected READY, got {ready:?}"))
        .parse()
        .expect("a port");
    // Keep draining, or the child blocks on a full pipe.
    std::thread::spawn(move || for _ in lines {});
    (child, port)
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

fn target_dir() -> std::path::PathBuf {
    std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("../../target/debug")
}

pub fn world() -> World {
    world_with(&[])
}

/// A world whose server takes extra flags — the deterministic-429 seam
/// (`--reclassify-attempts`) is the caller that needs this.
pub fn world_with(extra: &[&str]) -> World {
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

    let mut command = Command::new(target_dir().join("townhall-server"));
    command
        .arg("--db")
        .arg(dir.path().join("townhall.sqlite"))
        .arg("--denials-db")
        .arg(dir.path().join("denials.sqlite"))
        .args([
            "--council-url",
            &council_url,
            "--key-hex",
            KEY_HEX,
            "--port",
            "0",
            "--dev-authority",
            "--retry-cadence-ms",
            "200",
            "--reconcile-interval-ms",
            "100",
        ])
        .args(extra);
    let (server, server_port) = spawn_ready(command);

    World {
        dir,
        council,
        council_url,
        council_db,
        server: Some(server),
        server_url: format!("http://127.0.0.1:{server_port}"),
    }
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
