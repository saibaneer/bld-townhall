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

/// The RESOLVED dependency names of one workspace package, by kind.
///
/// `resolve.nodes`, not `packages[].dependencies` — the declared list is what
/// a manifest says; the resolved list is what the resolver linked, and only
/// the linked graph shows a forbidden crate arriving transitively.
pub fn resolved_dependencies(package: &str, kinds: &[&str]) -> Vec<String> {
    let output = Command::new(env!("CARGO"))
        .args(["metadata", "--format-version", "1"])
        .output()
        .expect("cargo metadata");
    assert!(output.status.success());
    let metadata: serde_json::Value =
        serde_json::from_slice(&output.stdout).expect("metadata json");

    // `resolve.nodes`, not `packages[].dependencies` — the review's point: the
    // latter is what the manifest DECLARED, the former is what the resolver
    // actually LINKED, and only the linked graph shows a forbidden crate
    // arriving through a re-export or a transitive edge.
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
    let node = nodes
        .iter()
        .find(|n| n["id"] == subject_id.as_str())
        .expect("a resolve node");
    node["deps"]
        .as_array()
        .expect("deps")
        .iter()
        .filter(|dep| {
            dep["dep_kinds"].as_array().is_none_or(|dep_kinds| {
                dep_kinds.iter().any(|dk| {
                    let kind = dk["kind"].as_str().unwrap_or("normal");
                    kinds.contains(&kind)
                })
            })
        })
        .map(|dep| dep["name"].as_str().expect("name").replace('_', "-"))
        .collect()
}
