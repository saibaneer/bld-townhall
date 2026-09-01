#![forbid(unsafe_code)]

//! The composition root (ADR-021): the one place the concrete world is wired —
//! the SQLite store, the denial log, the council client, the pursuit
//! configuration, the authority resolver — and handed to `townhall-http` as
//! trait objects. No handler logic lives here; no store type is nameable
//! there. That split IS the M5 gate's "handlers do not mutate directly",
//! enforced by the crate graph.
//!
//! ```text
//! townhall-server --db townhall.sqlite --denials-db denials.sqlite \
//!                 --council-url http://127.0.0.1:4010 --key-hex <64 hex> \
//!                 --port 0 --dev-authority
//!                 [--retry-cadence-ms 5000] [--reconcile-interval-ms 1000]
//! ```
//!
//! Prints `READY <port>` once bound — the same no-sleep discipline as the
//! council's binary.

use std::io::Write as _;
use std::process::ExitCode;
use std::sync::Arc;

use council_client::{CouncilClient, CouncilVerifier};
use council_wire::CouncilKey;
use townhall_http::{AuthorityResolver, ServerState};
use townhall_service::{BookingApi, Coordinator, PursuitConfig, Reconciliation};
use townhall_store::{SqliteBookingRepository, StoreClock, SystemStoreClock};

struct Args {
    db: String,
    denials_db: String,
    council_url: String,
    key_hex: String,
    port: u16,
    dev_authority: bool,
    retry_cadence_ms: i64,
    reconcile_interval_ms: u64,
    /// The deterministic-429 test seam (ADR-021): the one config zero
    /// `PursuitConfig` sanctions.
    reclassify_attempts: Option<u32>,
}

fn parse_args() -> Result<Args, String> {
    let mut args = std::env::args().skip(1);
    let (mut db, mut denials, mut council, mut key, mut port) = (None, None, None, None, None);
    let mut dev_authority = false;
    let mut retry_cadence_ms = 5_000;
    let mut reconcile_interval_ms = 1_000;
    let mut reclassify_attempts = None;
    while let Some(flag) = args.next() {
        let mut value = || args.next().ok_or_else(|| format!("{flag} needs a value"));
        match flag.as_str() {
            "--db" => db = Some(value()?),
            "--denials-db" => denials = Some(value()?),
            "--council-url" => council = Some(value()?),
            "--key-hex" => key = Some(value()?),
            "--port" => {
                port = Some(
                    value()?
                        .parse::<u16>()
                        .map_err(|_| "--port needs a number".to_owned())?,
                );
            }
            "--dev-authority" => dev_authority = true,
            "--retry-cadence-ms" => {
                retry_cadence_ms = value()?
                    .parse::<i64>()
                    .map_err(|_| "--retry-cadence-ms needs milliseconds".to_owned())?;
            }
            "--reconcile-interval-ms" => {
                reconcile_interval_ms = value()?
                    .parse::<u64>()
                    .map_err(|_| "--reconcile-interval-ms needs milliseconds".to_owned())?;
            }
            "--reclassify-attempts" => {
                reclassify_attempts = Some(
                    value()?
                        .parse::<u32>()
                        .map_err(|_| "--reclassify-attempts needs a count".to_owned())?,
                );
            }
            other => return Err(format!("unknown flag {other:?}")),
        }
    }
    Ok(Args {
        db: db.ok_or("--db is required")?,
        denials_db: denials.ok_or("--denials-db is required")?,
        council_url: council.ok_or("--council-url is required")?,
        key_hex: key.ok_or("--key-hex is required")?,
        port: port.ok_or("--port is required")?,
        dev_authority,
        retry_cadence_ms,
        reconcile_interval_ms,
        reclassify_attempts,
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

/// The M5 stand-in resolver (ADR-021): a FIXED two-token allowlist — nothing
/// pattern-derived, so "unknown bearer" is a real 401. Compiled only with the
/// `dev-authority` feature AND armed only by the `--dev-authority` flag;
/// without both, the server refuses to start, because no other resolver
/// exists until M7 replaces this one here in the composition root.
#[cfg(feature = "dev-authority")]
struct DevAuthority;

#[cfg(feature = "dev-authority")]
impl AuthorityResolver for DevAuthority {
    fn resolve(&self, bearer: &str) -> Option<townhall_domain::VerifiedAuthority> {
        use bld_types::{ActorId, Money, PrincipalId};
        match bearer {
            "dev-lucy" => Some(townhall_domain::VerifiedAuthority {
                principal: PrincipalId::new("lucy"),
                actor: ActorId::new("dev-terminal"),
                max_fee: Money::from_pence(5_000),
                may_book: true,
                may_cancel: true,
            }),
            "dev-marco-restricted" => Some(townhall_domain::VerifiedAuthority {
                principal: PrincipalId::new("marco"),
                actor: ActorId::new("dev-terminal"),
                max_fee: Money::from_pence(1_000),
                may_book: false,
                may_cancel: false,
            }),
            _ => None,
        }
    }
}

fn resolver(args: &Args) -> Result<Arc<dyn AuthorityResolver>, String> {
    #[cfg(feature = "dev-authority")]
    {
        if args.dev_authority {
            eprintln!(
                "==============================================================\n\
                 townhall-server: DEV AUTHORITY IS ACTIVE (ADR-021).\n\
                 Two fixed tokens exist: dev-lucy, dev-marco-restricted.\n\
                 This resolver is a stand-in until M7 and must never ship.\n\
                 =============================================================="
            );
            return Ok(Arc::new(DevAuthority));
        }
    }
    if args.dev_authority {
        return Err("--dev-authority requires building with the dev-authority feature".to_owned());
    }
    Err(
        "no authority resolver: until M7, start with the dev-authority feature AND \
         --dev-authority"
            .to_owned(),
    )
}

// One long function, deliberately: the composition root's whole job is this
// wiring, in order, in one readable place — splitting it into helpers would
// scatter the only sequence that matters here.
#[allow(clippy::too_many_lines)]
#[tokio::main]
async fn main() -> ExitCode {
    let args = match parse_args() {
        Ok(args) => args,
        Err(problem) => {
            eprintln!("townhall-server: {problem}");
            return ExitCode::from(2);
        }
    };
    let Some(key_bytes) = parse_key(&args.key_hex) else {
        eprintln!("townhall-server: --key-hex must be exactly 64 hex characters");
        return ExitCode::from(2);
    };
    let authority = match resolver(&args) {
        Ok(authority) => authority,
        Err(problem) => {
            eprintln!("townhall-server: {problem}");
            return ExitCode::from(2);
        }
    };
    let mut config = PursuitConfig {
        retry_cadence_ms: args.retry_cadence_ms,
        ..PursuitConfig::default()
    };
    if let Some(attempts) = args.reclassify_attempts {
        config.reclassify_attempts = attempts;
    }
    let config = match config.validated() {
        Ok(config) => config,
        Err(problem) => {
            eprintln!("townhall-server: {problem}");
            return ExitCode::from(2);
        }
    };

    // The concrete world, wired exactly once.
    let clock: Arc<dyn StoreClock> = Arc::new(SystemStoreClock);
    let repository = match SqliteBookingRepository::open_with(
        &args.db,
        townhall_store::DEFAULT_EFFECT_TTL_MS,
        Arc::clone(&clock),
    )
    .await
    {
        Ok(repository) => Arc::new(repository),
        Err(error) => {
            eprintln!("townhall-server: cannot open {}: {error}", args.db);
            return ExitCode::FAILURE;
        }
    };
    let denials = match townhall_store::denials::DenialLog::open(&args.denials_db, clock).await {
        Ok(log) => Arc::new(log),
        Err(error) => {
            eprintln!("townhall-server: cannot open {}: {error}", args.denials_db);
            return ExitCode::FAILURE;
        }
    };
    let key = CouncilKey::new(
        council_wire::CouncilSigner::new(council_wire::CouncilSigningKey::from_bytes(&key_bytes))
            .verifying_key(),
    );
    let client = || Arc::new(CouncilClient::new(&args.council_url, key));

    let coordinator = Arc::new(
        Coordinator::new(
            Arc::clone(&repository),
            client(),
            Arc::new(CouncilVerifier::new(key)),
            client(),
        )
        .with_denial_log(denials)
        .with_config(config),
    );
    let reconciliation = Arc::new(Reconciliation::new(Arc::clone(&coordinator), client()));
    let api = Arc::new(BookingApi::new(
        coordinator,
        Arc::clone(&reconciliation),
        client(),
        client(),
    ));

    // The loop: recovery runs itself, shutdown-aware.
    let (stop, stopped) = tokio::sync::watch::channel(false);
    let loop_task = tokio::spawn(townhall_http::run_reconciler(
        reconciliation,
        std::time::Duration::from_millis(args.reconcile_interval_ms),
        stopped,
    ));

    let router = townhall_http::router(ServerState { api, authority });
    let listener = match tokio::net::TcpListener::bind(("127.0.0.1", args.port)).await {
        Ok(listener) => listener,
        Err(error) => {
            eprintln!("townhall-server: cannot bind port {}: {error}", args.port);
            return ExitCode::FAILURE;
        }
    };
    let port = listener
        .local_addr()
        .map_or(args.port, |address| address.port());
    println!("READY {port}");
    let _ = std::io::stdout().flush();

    let served = axum::serve(listener, router)
        .with_graceful_shutdown(async {
            let _ = tokio::signal::ctrl_c().await;
        })
        .await;

    // The loop dies with the server, deliberately and observably.
    let _ = stop.send(true);
    let _ = loop_task.await;

    if served.is_err() {
        return ExitCode::FAILURE;
    }
    ExitCode::SUCCESS
}
