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
                if reconcile_interval_ms == 0 {
                    return Err("--reconcile-interval-ms must be positive".to_owned());
                }
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

/// The M5 stand-in resolver (ADR-021, amended by ADR-022): a FIXED three-token
/// allowlist — nothing pattern-derived, so "unknown bearer" is a real 401.
/// Compiled only with the
/// `dev-authority` feature AND armed only by the `--dev-authority` flag;
/// without both, the server refuses to start, because no other resolver
/// exists until M7 replaces this one here in the composition root.
#[cfg(feature = "dev-authority")]
struct DevAuthority;

#[cfg(feature = "dev-authority")]
impl DevAuthority {
    /// What each dev token stands for: who, how much, and what they may do.
    ///
    /// A FIXED allowlist, nothing pattern-derived, so "unknown bearer" stays a
    /// real 401.
    fn allowed(bearer: &str) -> Option<(&'static str, u64, &'static [bld_types::Behaviour])> {
        use bld_types::Behaviour;
        const BOTH: &[Behaviour] = &[Behaviour::Book, Behaviour::Cancel];
        const NEITHER: &[Behaviour] = &[];
        // Restricted in EXACTLY ONE way, and that is the point.
        //
        // Marco is restricted twice over — a £10 ceiling and no behaviours — so
        // "Marco is refused" never said which guard refused him. Priya carries
        // Lucy's ceiling and lacks only `Book`, so a refusal on her own booking
        // can only be `BookingAuthorityRequired`. Without her, the behaviour
        // guard has no test that isolates it.
        const CANCEL_ONLY: &[Behaviour] = &[Behaviour::Cancel];
        match bearer {
            "dev-lucy" => Some(("lucy", 5_000, BOTH)),
            "dev-marco-restricted" => Some(("marco", 1_000, NEITHER)),
            "dev-priya-nobook" => Some(("priya", 5_000, CANCEL_ONLY)),
            _ => None,
        }
    }

    /// Issue a dev grant over one booking, through the real approval path.
    ///
    /// # Why the stand-in now runs the whole flow
    ///
    /// It used to write a struct literal. ADR-025 sealed the envelope — private
    /// fields, and no constructor to reach for — precisely so that nothing can
    /// assert its own authority, and a demo lane is not an exception. So this
    /// raises a challenge and answers it, exactly as the SMS path will. What
    /// makes it a STAND-IN is not a shortcut through the machinery; it is that
    /// nobody was asked.
    ///
    /// # What this lane does NOT prove
    ///
    /// The grant names whichever booking the request named, so its
    /// resource check can never refuse anything here. That is honest for a lane
    /// where nobody was asked, and it means the curl suite witnesses the
    /// BEHAVIOUR guard (`dev-priya-nobook` is refused `Book`) and the OWNERSHIP
    /// guard (M5.1's scoped rows), never the resource guard. The resource guard
    /// is witnessed in `townhall-authority`, where a grant is issued for one
    /// booking and asked about another.
    ///
    /// The assurance is pinned to the floor. Once the envelope carries a level,
    /// a dev token must fabricate one like every other field, and the value a
    /// careless implementation reaches for is the strongest — which would make
    /// a dev token a forged envelope with a straight face (ADR-025's amendment
    /// to ADR-021).
    fn issue(
        bearer: &str,
        booking: &bld_types::BookingId,
    ) -> Option<townhall_domain::VerifiedAuthority> {
        use bld_types::{PrincipalId, ServiceId};
        use townhall_authority::{
            ApprovalCode, ApprovalRequest, AssuranceLevel, AuthorityPolicy, AuthorityService,
            BehaviourSet, BindingRef, EnvelopeKey, Entropy, MemoryApprovalStore, PendingScope,
        };

        struct DevCode;
        impl Entropy for DevCode {
            fn code(&self) -> ApprovalCode {
                ApprovalCode::new("0000").expect("four digits")
            }
            fn identifier(&self) -> String {
                "dev".to_owned()
            }
        }

        let (principal, max_fee_pence, behaviours) = Self::allowed(bearer)?;
        let service = AuthorityService::new(
            std::sync::Arc::new(MemoryApprovalStore::new()),
            DevCode,
            AuthorityPolicy {
                reply_window_ms: 60_000,
                // Short, so a dev grant is never mistaken for a durable one.
                grant_ttl_ms: 5 * 60 * 1_000,
                assurance: AssuranceLevel::Dev,
            },
            // Per-process, and it genuinely does not matter here: this service
            // issues a grant and hands it straight back, and its store is
            // dropped on the next line. Nothing ever reads the envelope it
            // wrote. A deployment binds a configured key at the composition
            // root, where the grant IS read back — see M7B's resolver.
            EnvelopeKey::new(std::process::id().to_le_bytes().repeat(8))
                .expect("32 bytes"),
        );
        let binding = BindingRef {
            principal: PrincipalId::new(principal),
            version: 1,
        };
        let now = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map_or(0, |since| {
                u64::try_from(since.as_millis()).unwrap_or(u64::MAX)
            });

        let request = ApprovalRequest {
            scope: PendingScope {
                service: ServiceId::new("demo-council-town-hall"),
                agent: "dev-terminal".to_owned(),
                booking: booking.clone(),
                behaviours: BehaviourSet::new(behaviours.iter().copied()),
                requirements: bld_types::BookingRequirements {
                    purpose: "dev lane".to_owned(),
                    requested_date: "2026-01-01".to_owned(),
                    time_window: bld_types::TimeWindow {
                        from: "00:00".to_owned(),
                        to: "23:59".to_owned(),
                    },
                    attendees: 1,
                    wheelchair_accessible: false,
                    max_fee: bld_types::Money::from_pence(max_fee_pence),
                },
            },
            binding: binding.clone(),
            grantor: PrincipalId::new(principal),
            subject: PrincipalId::new(principal),
        };

        // Blocking on a fresh runtime in a thread: this resolver is called from
        // inside the server's runtime, where `block_on` panics. The whole dance
        // disappears in M7B, when the resolver reads a delegation the approval
        // path already issued instead of minting one per request.
        std::thread::spawn(move || {
            tokio::runtime::Builder::new_current_thread()
                .build()
                .expect("a dev runtime")
                .block_on(async move {
                    let raised = service.begin(&request, now).await.ok()?;
                    service
                        .submit(&raised.id, "0000", &binding, AssuranceLevel::Dev, now + 1)
                        .await
                        .ok()
                })
        })
        .join()
        .ok()
        .flatten()
    }
}

#[cfg(feature = "dev-authority")]
impl AuthorityResolver for DevAuthority {
    fn resolve(
        &self,
        bearer: &str,
        booking: &bld_types::BookingId,
    ) -> Option<townhall_domain::VerifiedAuthority> {
        Self::issue(bearer, booking)
    }

    /// A reader gets a name, not a grant — nothing here to mint.
    fn resolve_reader(&self, bearer: &str) -> Option<bld_types::PrincipalId> {
        Self::allowed(bearer).map(|(principal, _, _)| bld_types::PrincipalId::new(principal))
    }
}

fn resolver(args: &Args) -> Result<Arc<dyn AuthorityResolver>, String> {
    #[cfg(feature = "dev-authority")]
    {
        if args.dev_authority {
            eprintln!(
                "==============================================================\n\
                 townhall-server: DEV AUTHORITY IS ACTIVE (ADR-021).\n\
                 Three fixed tokens exist: dev-lucy, dev-marco-restricted,\n\
                 dev-priya-nobook.\n\
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
