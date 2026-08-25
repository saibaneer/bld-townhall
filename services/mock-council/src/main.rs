#![forbid(unsafe_code)]

//! The council as a process — so a harness can kill it.
//!
//! Slice C faked crashes by scripting a struct; slice D proved commit-before-
//! response ordering in-process. What neither could do is die: cancelling a task
//! is not a crash, because memory survives and open transactions roll back
//! politely. This binary exists to be `SIGKILL`ed mid-write and restarted
//! against the same database file, which is the only honest test of ADR-016's
//! durability claims.
//!
//! ```text
//! mock-council --db council.sqlite --key-hex <64 hex chars> --port 0
//!              [--pause-at <point>]...     arm IPC pause points (implies --clock)
//!              [--clock <ms>]              injectable clock, movable via SETCLOCK
//! ```
//!
//! On startup it prints `READY <port>` to stdout — the parent's signal to
//! connect, so no harness ever sleeps to wait for a socket. With any pause
//! point armed, the stdin/stdout protocol in [`mock_council::ipc`] is live.
//!
//! # There is no outage fault, deliberately
//!
//! Unavailability cannot be scoped to a request: refusing a connection happens
//! before any byte of a route is readable, so it cannot live in the fault bank.
//! Rather than build a process-wide fault window here, the suite makes the
//! council unavailable the honest way — it kills this process and talks to the
//! dead socket (test 7 in `council-client/tests/reconciliation.rs`). An armed
//! imitation of an outage would only ever prove the imitation.

use mock_council::{
    Council,
    clock::{Clock, SettableClock, SystemClock, TestClock},
    ipc::IpcPauses,
    pause::{NeverPauses, PausePoint, Pauses},
};
use std::{collections::HashSet, io::Write as _, process::ExitCode, sync::Arc};

fn usage(problem: &str) -> ExitCode {
    eprintln!("mock-council: {problem}");
    eprintln!(
        "usage: mock-council --db <path> --key-hex <64 hex chars> --port <n> \
         [--pause-at <point>]... [--clock <ms>]"
    );
    ExitCode::from(2)
}

struct Args {
    db: String,
    key_hex: String,
    port: u16,
    pause_at: HashSet<PausePoint>,
    clock_ms: Option<i64>,
}

fn parse_args() -> Result<Args, String> {
    let mut args = std::env::args().skip(1);
    let (mut db, mut key_hex, mut port, mut clock_ms) = (None, None, None, None);
    let mut pause_at = HashSet::new();

    while let Some(flag) = args.next() {
        let mut value = || args.next().ok_or_else(|| format!("{flag} needs a value"));
        match flag.as_str() {
            "--db" => db = Some(value()?),
            "--key-hex" => key_hex = Some(value()?),
            "--port" => {
                port = Some(
                    value()?
                        .parse::<u16>()
                        .map_err(|_| "--port needs a number".to_owned())?,
                );
            }
            "--pause-at" => {
                let raw = value()?;
                let point = PausePoint::parse(&raw)
                    .ok_or_else(|| format!("unknown pause point {raw:?}"))?;
                pause_at.insert(point);
            }
            "--clock" => {
                clock_ms = Some(
                    value()?
                        .parse::<i64>()
                        .map_err(|_| "--clock needs milliseconds".to_owned())?,
                );
            }
            other => return Err(format!("unknown flag {other:?}")),
        }
    }

    Ok(Args {
        db: db.ok_or("--db is required")?,
        key_hex: key_hex.ok_or("--key-hex is required")?,
        port: port.ok_or("--port is required")?,
        pause_at,
        clock_ms,
    })
}

fn parse_key(hex: &str) -> Option<[u8; 32]> {
    if hex.len() != 64 {
        return None;
    }
    let mut key = [0u8; 32];
    for (i, byte) in key.iter_mut().enumerate() {
        *byte = u8::from_str_radix(&hex[2 * i..2 * i + 2], 16).ok()?;
    }
    Some(key)
}

#[tokio::main]
async fn main() -> ExitCode {
    let args = match parse_args() {
        Ok(args) => args,
        Err(problem) => return usage(&problem),
    };
    let Some(key) = parse_key(&args.key_hex) else {
        return usage("--key-hex must be exactly 64 hex characters");
    };

    // The clock: injectable the moment anything needs to move it. A pause driver
    // without a settable clock could acknowledge a SETCLOCK it cannot apply,
    // which is the incoherence review found in an earlier design — so arming any
    // pause point forces the test clock.
    let needs_test_clock = args.clock_ms.is_some() || !args.pause_at.is_empty();
    let (clock, settable): (Arc<dyn Clock>, Option<Arc<TestClock>>) = if needs_test_clock {
        let start = args.clock_ms.unwrap_or_else(|| SystemClock.now_ms());
        let test = Arc::new(TestClock::at(start));
        (Arc::clone(&test) as Arc<dyn Clock>, Some(test))
    } else {
        (Arc::new(SystemClock), None)
    };

    let pauses: Arc<dyn Pauses> = match (&settable, args.pause_at.is_empty()) {
        (Some(test), false) => IpcPauses::start(
            args.pause_at.clone(),
            Arc::clone(test) as Arc<dyn SettableClock>,
        ),
        _ => Arc::new(NeverPauses),
    };

    let signer = Arc::new(council_wire::CouncilSigner::new(
        council_wire::CouncilSigningKey::from_bytes(&key),
    ));

    let council = match Council::build(
        &args.db,
        signer,
        clock,
        pauses,
        mock_council::DEFAULT_AVAILABILITY_TTL_MS,
    )
    .await
    {
        Ok(council) => council,
        Err(error) => {
            eprintln!("mock-council: cannot open {}: {error}", args.db);
            return ExitCode::FAILURE;
        }
    };

    let listener = match tokio::net::TcpListener::bind(("127.0.0.1", args.port)).await {
        Ok(listener) => listener,
        Err(error) => {
            eprintln!("mock-council: cannot bind port {}: {error}", args.port);
            return ExitCode::FAILURE;
        }
    };
    let port = listener
        .local_addr()
        .map_or(args.port, |address| address.port());

    // The startup signal. Printed after bind and after migrations, so a parent
    // that reads it can connect immediately — the no-sleep rule starts here.
    println!("READY {port}");
    let _ = std::io::stdout().flush();

    if axum::serve(listener, council.router()).await.is_err() {
        return ExitCode::FAILURE;
    }
    ExitCode::SUCCESS
}
