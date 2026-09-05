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
mod authority;
mod usage;

use authority::{OsEntropy, RealAuthority};
use townhall_authority::AuthorityService;
use townhall_http::{AuthorityResolver, ServerState};
use townhall_service::{BookingApi, Coordinator, PursuitConfig, Reconciliation};
use townhall_store::{SqliteBookingRepository, StoreClock, SystemStoreClock};

struct Args {
    db: String,
    /// The key the delegation envelope's authentication tag is made with.
    ///
    /// 64 hex characters. Required whenever grants are READ BACK — which is
    /// every real deployment — because a tag verified with a different key than
    /// it was written with is no tag at all, and a per-process key would make
    /// every grant expire at restart.
    ///
    /// Optional only with `--dev-authority`, whose resolver mints a grant per
    /// request and never reads one back (see `DevAuthority::issue`).
    authority_key: Option<String>,
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
    /// M8-2 rate/budget ceilings (ADR-028), each overriding a generous default so
    /// an acceptance test can spawn a tiny ceiling and exhaust it in a few turns.
    /// The windows stay at their default (long), so a per-window ceiling reads as
    /// a de-facto cap for the run.
    usage_principal_rate_max: Option<i64>,
    usage_channel_rate_max: Option<i64>,
    usage_global_budget_max: Option<i64>,
    /// The ed25519 key (64 hex) the BLD discovery manifest is signed with (M9,
    /// ADR-029). Its own key — each key has one job. When absent, the server
    /// serves no `/.well-known/bld` (discovery is opt-in).
    manifest_key: Option<String>,
}

fn parse_args() -> Result<Args, String> {
    let mut args = std::env::args().skip(1);
    let (mut db, mut denials, mut council, mut key, mut port) = (None, None, None, None, None);
    let mut dev_authority = false;
    let mut authority_key = None;
    let mut retry_cadence_ms = 5_000;
    let mut reconcile_interval_ms = 1_000;
    let mut reclassify_attempts = None;
    let mut usage_principal_rate_max = None;
    let mut usage_channel_rate_max = None;
    let mut usage_global_budget_max = None;
    let mut manifest_key = None;
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
            "--authority-key" => authority_key = Some(value()?),
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
            "--principal-rate-max" => {
                usage_principal_rate_max = Some(
                    value()?
                        .parse::<i64>()
                        .map_err(|_| "--principal-rate-max needs a unit count".to_owned())?,
                );
            }
            "--channel-rate-max" => {
                usage_channel_rate_max = Some(
                    value()?
                        .parse::<i64>()
                        .map_err(|_| "--channel-rate-max needs a unit count".to_owned())?,
                );
            }
            "--global-budget-max" => {
                usage_global_budget_max = Some(
                    value()?
                        .parse::<i64>()
                        .map_err(|_| "--global-budget-max needs a unit count".to_owned())?,
                );
            }
            "--manifest-key" => manifest_key = Some(value()?),
            other => return Err(format!("unknown flag {other:?}")),
        }
    }
    Ok(Args {
        db: db.ok_or("--db is required")?,
        authority_key,
        denials_db: denials.ok_or("--denials-db is required")?,
        council_url: council.ok_or("--council-url is required")?,
        key_hex: key.ok_or("--key-hex is required")?,
        port: port.ok_or("--port is required")?,
        dev_authority,
        retry_cadence_ms,
        reconcile_interval_ms,
        reclassify_attempts,
        usage_principal_rate_max,
        usage_channel_rate_max,
        usage_global_budget_max,
        manifest_key,
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
        // EVERY behaviour, because M7B consults the grant for every proposal
        // and these tokens exist to isolate ONE guard each.
        //
        // An earlier version withheld `UpdateRequirements` and `ChangeVenue`
        // from all three on the grounds that "no dev token pretends to that".
        // That made Lucy restricted too, so she stopped being the control, and
        // a version-bumping test broke on a guard it was not about.
        const ALL: &[Behaviour] = &[
            Behaviour::SelectVenue,
            Behaviour::VerifySlot,
            Behaviour::ChangeVenue,
            Behaviour::UpdateRequirements,
            Behaviour::RevalidateVenue,
            Behaviour::Book,
            Behaviour::Cancel,
        ];

        // Restricted in EXACTLY ONE way, and that is the point.
        //
        // Marco is restricted twice over — a £10 ceiling and no behaviours — so
        // "Marco is refused" never said which guard refused him. Priya carries
        // Lucy's ceiling and lacks only `Book`, so a refusal on her own booking
        // can only be `BookingAuthorityRequired`. Without her, the behaviour
        // guard has no test that isolates it.
        // Priya lacks exactly ONE behaviour — `Book` — so a refusal on her own
        // booking can only be the booking-authority guard. She can still walk
        // a booking to `AwaitingBooking`, which is what makes the isolation
        // real rather than incidental.
        const ALL_WITHOUT_BOOK: &[Behaviour] = &[
            Behaviour::SelectVenue,
            Behaviour::VerifySlot,
            Behaviour::ChangeVenue,
            Behaviour::UpdateRequirements,
            Behaviour::RevalidateVenue,
            Behaviour::Cancel,
        ];
        // Each token restricted in EXACTLY ONE way, which is what makes a
        // refusal name its guard:
        //
        // - Lucy: unrestricted, the £50 ceiling every seeded slot fits under.
        // - Marco: the same walk, a £10 ceiling. A refusal on his booking can
        //   only be the FEE guard.
        // - Priya: Lucy's ceiling, missing only `Book`. A refusal on hers can
        //   only be the BEHAVIOUR guard.
        //
        // An earlier version of this table gave Marco no behaviours AND a £10
        // ceiling — restricted twice, so "Marco is refused" said nothing about
        // which guard refused him, and the fee-ceiling test broke at
        // `select-venue` for a reason it was not about.
        match bearer {
            "dev-lucy" => Some(("lucy", 5_000, ALL)),
            "dev-marco-restricted" => Some(("marco", 1_000, ALL)),
            "dev-priya-nobook" => Some(("priya", 5_000, ALL_WITHOUT_BOOK)),
            _ => None,
        }
    }

    /// Which token belongs to a principal — `allowed` read backwards.
    ///
    /// A closed match in both directions rather than a format string, so the
    /// allowlist stays the only place a dev identity is named.
    fn bearer_for(principal: &str) -> Option<&'static str> {
        match principal {
            "lucy" => Some("dev-lucy"),
            "marco" => Some("dev-marco-restricted"),
            "priya" => Some("dev-priya-nobook"),
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
            BehaviourSet, BindingRef, Entropy, EnvelopeKey, InboundEvidenceRecord,
            MemoryApprovalStore, PendingScope,
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
        let store = std::sync::Arc::new(MemoryApprovalStore::new());
        // Bound, because the verifier checks a claimed binding against a row.
        // Even a lane where nobody is asked cannot skip the check — it just
        // supplies the row itself, which is exactly what makes it a stand-in
        // rather than a bypass.
        store.bind(&PrincipalId::new(principal), 1);
        let service = AuthorityService::new(
            std::sync::Arc::clone(&store),
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
            EnvelopeKey::new(std::process::id().to_le_bytes().repeat(8)).expect("32 bytes"),
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
                    // No headcount ceiling.
                    //
                    // This was `1`, written as a placeholder when the field was
                    // only decoration in this lane. The M7B review made the
                    // approved headcount a real constraint, and the placeholder
                    // silently became a ceiling of ONE — so any change above
                    // one attendee was refused, in a lane whose whole nature is
                    // that it approves whatever was asked.
                    //
                    // CI caught it; a local sweep I neglected to re-run would
                    // have. Worth the comment because the failure mode is
                    // general: a placeholder in a field nobody reads is fine
                    // until somebody starts reading the field.
                    attendees: u16::MAX,
                    wheelchair_accessible: false,
                    max_fee: bld_types::Money::from_pence(max_fee_pence),
                },
            },
            binding: binding.clone(),
            grantor: PrincipalId::new(principal),
            subject: PrincipalId::new(principal),
            // The same actor `authenticate` hands out for this token, so the
            // grant this mints is presentable by the caller that asked for it.
            actor: bld_types::ActorId::new(format!("dev:{principal}")),
        };
        let actor = bld_types::ActorId::new(format!("dev:{principal}"));
        let address = format!("+{principal}");

        // Blocking on a fresh runtime in a thread: this resolver is called from
        // inside the server's runtime, where `block_on` panics. Even the dev lane
        // travels the receipt seam a real approval travels — it deposits its own
        // evidence and forwards the receipt — because a stand-in that skipped the
        // seam would prove the wrong path is sound.
        std::thread::spawn(move || {
            tokio::runtime::Builder::new_current_thread()
                .build()
                .expect("a dev runtime")
                .block_on(async move {
                    let (_, raised) = service.begin(&request, now).await.ok()?;
                    let (_challenge, receipt) = service
                        .deposit_evidence(
                            &address,
                            &InboundEvidenceRecord {
                                provider: "dev".to_owned(),
                                provider_account: "dev".to_owned(),
                                provider_message_id: raised.id.as_str().to_owned(),
                                claimed_sender: address.clone(),
                                verified: true,
                                signature: None,
                            },
                            now,
                            60_000,
                        )
                        .await
                        .ok()?;
                    service
                        .submit(&raised.id, "0000", &actor, &receipt, now + 1)
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
    /// A dev token names its own actor.
    ///
    /// The actor carries the principal, which is what lets the two questions
    /// below be answered without a second lookup. A real deployment
    /// authenticates a workload credential that has nothing to do with any
    /// person, and finds the principal through the presented grant or the
    /// channel binding instead.
    fn authenticate(&self, bearer: &str) -> Option<bld_types::ActorId> {
        Self::allowed(bearer)
            .map(|(principal, _, _)| bld_types::ActorId::new(format!("dev:{principal}")))
    }

    /// In this lane the reference IS the booking id.
    ///
    /// # Why that is not cheating, and what it costs
    ///
    /// A real reference names a delegation that an approval produced, and the
    /// resolver looks it up. Here nobody was asked, so there is nothing to look
    /// up — the lane's whole nature is that it skips the person. Rather than
    /// pretend, it says so: hand it a booking id and it issues a grant over
    /// that booking, for the principal the actor names.
    ///
    /// What it costs is stated in `issue`: this lane cannot witness the
    /// resource guard, because the grant always names whatever was asked for.
    /// It still witnesses the behaviour guard and the ownership guard, and it
    /// now also witnesses that a change without ANY delegation header is
    /// refused — which is new in M7B and is a real property.
    fn resolve_delegation(
        &self,
        reference: &str,
        actor: &bld_types::ActorId,
    ) -> Option<townhall_domain::VerifiedAuthority> {
        let principal = actor.as_str().strip_prefix("dev:")?;
        let bearer = Self::bearer_for(principal)?;
        Self::issue(bearer, &bld_types::BookingId::new(reference))
    }

    /// A dev actor reads for its own principal and no other.
    fn may_read_for(&self, actor: &bld_types::ActorId, principal: &bld_types::PrincipalId) -> bool {
        actor.as_str().strip_prefix("dev:") == Some(principal.as_str())
    }
}

/// Whether `--dev-authority` is usable in THIS build.
///
/// # Why this is separate from `authority`, and runs first
///
/// It used to live inside `authority`, which needs a connection pool — so a
/// build without the feature opened a database, ran migrations, and only THEN
/// refused to start. Two things wrong with that. A server should not touch
/// storage to discover its arguments are wrong; and returning from `main` with
/// a live `sqlx` pool means dropping a Tokio runtime that is still waiting on
/// the pool's per-connection blocking threads. On a loaded Linux runner that
/// wait is long enough to look like a hang, which is exactly how CI reported
/// it — `the_dev_authority_flag_does_not_exist_in_this_build` sitting for over
/// sixty seconds on a check that takes microseconds.
///
/// Argument validation is now pure and happens before any I/O.
// With the feature ENABLED this can only return `Ok`, and clippy says so. The
// signature is not redundant — it is the shape of the question in the build
// where the answer can be no, and collapsing it to `()` under one feature set
// would mean the two builds disagreed about whether the check exists.
#[allow(clippy::unnecessary_wraps)]
fn dev_authority_available(args: &Args) -> Result<(), String> {
    if !args.dev_authority {
        return Ok(());
    }
    #[cfg(feature = "dev-authority")]
    {
        Ok(())
    }
    #[cfg(not(feature = "dev-authority"))]
    {
        Err("--dev-authority requires building with the dev-authority feature".to_owned())
    }
}

/// The issuer, and the resolver that reads what it wrote.
///
/// # Why the issuer is built unconditionally
///
/// The approval endpoints exist in every build: a person can be asked for
/// approval whether or not the dev lane is armed, and a challenge answered
/// through them produces a real delegation row either way. Only the RESOLVER
/// differs — which is precisely ADR-025's amendment to ADR-021: the real one is
/// the default and `--dev-authority` selects the stand-in explicitly, so there
/// is no silent fallback in either direction.
type Issuer = AuthorityService<townhall_store::authority::SqlApprovalStore, OsEntropy>;

fn authority(
    args: &Args,
    pool: &sqlx::SqlitePool,
) -> Result<(Arc<dyn AuthorityResolver>, Arc<Issuer>), String> {
    use townhall_authority::{AuthorityPolicy, EnvelopeKey};

    let key_hex = args
        .authority_key
        .as_deref()
        .ok_or("--authority-key is required: 64 hex characters")?;
    let key_bytes = parse_key(key_hex)
        .ok_or("--authority-key must be 64 hex characters (32 bytes)".to_owned())?;
    let key = EnvelopeKey::new(key_bytes.to_vec()).map_err(|error| error.to_string())?;

    let store = Arc::new(townhall_store::authority::SqlApprovalStore::new(
        pool.clone(),
    ));
    let issuer = Arc::new(AuthorityService::new(
        Arc::clone(&store),
        OsEntropy,
        AuthorityPolicy::default(),
        key,
    ));

    #[cfg(feature = "dev-authority")]
    {
        if args.dev_authority {
            eprintln!(
                "==============================================================\n\
                 townhall-server: DEV AUTHORITY IS ACTIVE (ADR-021, amended by\n\
                 ADR-025). Three fixed tokens exist: dev-lucy,\n\
                 dev-marco-restricted, dev-priya-nobook. Each mints a grant per\n\
                 request over whatever booking was named — nobody is asked.\n\
                 This resolver is a stand-in and must never ship.\n\
                 =============================================================="
            );
            return Ok((Arc::new(DevAuthority), issuer));
        }
    }
    // The flag/feature combination was settled before any I/O — see
    // `dev_authority_available`. Reaching here with the flag set and the
    // feature absent is impossible.

    // The real resolver. It can mint nothing: a grant exists because somebody
    // answered a challenge, and this looks it up.
    let actors = vec![(
        "agent-townhall".to_owned(),
        bld_types::ActorId::new("agent:townhall"),
    )];
    Ok((
        Arc::new(RealAuthority::new(Arc::clone(&issuer), store, actors)),
        issuer,
    ))
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
    if let Err(problem) = dev_authority_available(&args) {
        eprintln!("townhall-server: {problem}");
        return ExitCode::from(2);
    }
    let Some(key_bytes) = parse_key(&args.key_hex) else {
        eprintln!("townhall-server: --key-hex must be exactly 64 hex characters");
        return ExitCode::from(2);
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
    // The issuer and resolver, built here because they need the pool the
    // repository owns — one database, so a delegation written by the endpoints
    // is the same row the resolver reads back (ADR-025's shared authority
    // plane, which is why these endpoints live in the server at all).
    let (authority, issuer) = match authority(&args, repository.pool()) {
        Ok(pair) => pair,
        Err(problem) => {
            eprintln!("townhall-server: {problem}");
            return ExitCode::from(2);
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

    // Two routers, one listener.
    //
    // The booking API and the trusted approval endpoints are separate surfaces
    // that happen to share a port. What keeps the untrusted MODEL seat away from
    // the second is the client surface and the crate graph together: `Gateway`
    // has no method naming any of these paths, and `townhall-orchestrator` cannot
    // name `townhall-authority` at all (ADR-025, asserted by that crate's
    // resolved-dependency test) — so the code that forms proposals holds no way
    // to write an evidence row or a grant.
    //
    // What this does NOT do is stop a runtime HTTP POST. The crate-graph ban is a
    // COMPILE-time fact; these endpoints answer over TCP, and a compromised
    // orchestrator PROCESS holds a workload token and could reach them. That is
    // the residual ADR-026 scopes to the model seat and names honestly as the
    // SMS-demo assurance level — the crate ban is defence-in-depth for that seat,
    // not the boundary against a subverted process, which M12's signed webhook
    // closes.
    let approvals =
        townhall_http::approvals::approval_router(townhall_http::approvals::ApprovalState {
            issuer: Arc::new(authority::ServiceIssuer(issuer)),
            authority: Arc::clone(&authority),
        });
    // The usage meter (M8, ADR-027), over the SAME pool as bookings and the
    // authority rows. Its binding resolver is a second handle to the approval
    // store, so a metered turn resolves to the same principal the approval plane
    // would — one identity, both planes.
    // Start from the generous defaults; the three optional flags let a test (or a
    // deployment) tighten a ceiling. `validated()` rejects a max below one turn
    // (a permanent brick) before any traffic.
    let mut usage_policy = townhall_usage::UsagePolicy::default();
    if let Some(max) = args.usage_principal_rate_max {
        usage_policy.principal_rate_max = max;
    }
    if let Some(max) = args.usage_channel_rate_max {
        usage_policy.channel_rate_max = max;
    }
    if let Some(max) = args.usage_global_budget_max {
        usage_policy.global_budget_max = max;
    }
    let usage_policy = match usage_policy.validated() {
        Ok(policy) => policy,
        Err(problem) => {
            eprintln!("townhall-server: {problem}");
            return ExitCode::from(2);
        }
    };
    let usage_routes = townhall_http::usage::usage_router(townhall_http::usage::UsageState {
        meter: Arc::new(usage::ServiceMeter {
            usage: Arc::new(townhall_usage::UsageService::new(
                Arc::new(townhall_store::usage::SqlUsageStore::new(
                    repository.pool().clone(),
                )),
                usage_policy,
            )),
            bindings: Arc::new(townhall_store::authority::SqlApprovalStore::new(
                repository.pool().clone(),
            )),
        }),
        authority: Arc::clone(&authority),
    });
    // BLD discovery (M9, ADR-029): when a manifest key is provided, build the
    // manifest from the ONE behaviour table, sign it with that key, and serve it
    // at /.well-known/bld. Opt-in — a server without the key serves no discovery.
    let mut router = townhall_http::router(ServerState { api, authority })
        .merge(approvals)
        .merge(usage_routes);
    if let Some(hex) = args.manifest_key.as_deref() {
        let Some(signing) = bld_manifest::signing_key_from_hex(hex) else {
            eprintln!("townhall-server: --manifest-key must be 64 hex characters (32 bytes)");
            return ExitCode::from(2);
        };
        let signed = match townhall_http::discovery::booking_manifest().sign(&signing) {
            Ok(signed) => signed,
            Err(error) => {
                eprintln!("townhall-server: could not sign the discovery manifest: {error}");
                return ExitCode::FAILURE;
            }
        };
        let manifest = match serde_json::to_value(&signed) {
            Ok(value) => value,
            Err(error) => {
                eprintln!("townhall-server: could not serialize the discovery manifest: {error}");
                return ExitCode::FAILURE;
            }
        };
        router = router.merge(townhall_http::discovery::discovery_router(
            townhall_http::discovery::DiscoveryState {
                manifest: std::sync::Arc::new(manifest),
            },
        ));
    }
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

/// ADR-025's amendment, second property: the flag is unavailable without the
/// feature.
///
/// # Why this is a unit test and not a spawned binary
///
/// It was a spawned binary, and that cost an afternoon. The test ran
/// `CARGO_BIN_EXE_townhall-server --dev-authority` and asserted a refusal — but
/// that path is `target/debug/townhall-server`, a single file every
/// feature-build overwrites and CI's cache leaves pointing at the FEATURE lane's
/// binary. So the test spawned a feature-enabled server, which accepted the flag
/// and served, and the whole run hung. Every fix that kept the spawn — asking
/// cargo for the path (still the shared path), an isolated rebuild (a nested
/// `cargo build` that starved for CPU under the parallel workspace run) — traded
/// one flavour of the same fragility for another.
///
/// The property is a COMPILE-TIME fact: in a build without the feature,
/// `dev_authority_available` returns `Err`. So it is tested where it is decided,
/// with no process, no path, no cache, and no chance of checking the wrong
/// binary. `main` acts on that `Err` by refusing to start (one visible line
/// above), which is the part a subprocess was never really needed to prove.
///
/// Compiled only WITHOUT the feature — under `--dev-authority` the function can
/// only return `Ok`, and asserting a refusal there would assert the build's own
/// configuration back at itself.
#[cfg(all(test, not(feature = "dev-authority")))]
mod feature_gate {
    use super::{Args, dev_authority_available};

    fn args(dev_authority: bool) -> Args {
        Args {
            db: "/tmp/unused".to_owned(),
            authority_key: None,
            denials_db: "/tmp/unused".to_owned(),
            council_url: "http://127.0.0.1:1".to_owned(),
            key_hex: "0".repeat(64),
            port: 0,
            dev_authority,
            retry_cadence_ms: 0,
            reconcile_interval_ms: 1,
            reclassify_attempts: None,
            usage_principal_rate_max: None,
            usage_channel_rate_max: None,
            usage_global_budget_max: None,
            manifest_key: None,
        }
    }

    #[test]
    fn dev_authority_is_refused_without_the_feature() {
        assert!(
            dev_authority_available(&args(true)).is_err(),
            "a build without the dev-authority feature must refuse --dev-authority"
        );
    }

    #[test]
    fn no_flag_is_fine_without_the_feature() {
        assert!(
            dev_authority_available(&args(false)).is_ok(),
            "not asking for the dev lane is always allowed"
        );
    }
}
