#![forbid(unsafe_code)]

//! Our side of the boundary, as a process that can die on cue.
//!
//! The crash tests need to kill *us*, not just the council — and "kill between
//! Phase A's commit and the provider call" cannot be positioned from outside a
//! test-runner process, because a test cannot abort itself and keep asserting.
//! So this driver runs one booking turn and, depending on `--die`, aborts at
//! exactly the moment under test:
//!
//! ```text
//! --die before-call   Phase A committed, the capability invoked — and the
//!                     process aborts at its entry, before any byte reaches the
//!                     council. Test 1: intent durable, provider has nothing.
//! --die after-call    the council answered — the booking EXISTS out there —
//!                     and the process aborts before the evidence lands
//!                     locally. Test 5: the two records disagree, and only
//!                     reconciliation under the same identity heals it.
//! --die never         run the turn to whatever end it reaches, print the
//!                     outcome, exit cleanly.
//! ```
//!
//! `abort()` rather than `panic!`: no unwinding, no destructors, no polite
//! rollback — the closest a process gets to a power cut on demand.

use bld_kernel::{Capability, Unknown};
use bld_types::{
    BookingId, BookingRequirements, EffectAttempt, Money, PrincipalId, SlotId, TimeWindow, VenueId,
};
use council_client::{CouncilClient, CouncilVerifier};
use council_wire::CouncilKey;
use std::{process::ExitCode, sync::Arc};
use townhall_domain::{BookingEffect, BookingProposal, VerifiedAuthority};
use townhall_service::{Coordinator, Reconciliation};
use townhall_store::{
    BookingRepository as _, NewBooking, SqliteBookingRepository, StoreClock, SystemStoreClock,
};

/// The system clock, shifted — so a reconcile run can be "later" than the
/// cadence the dying process wrote, without a harness ever sleeping.
#[derive(Debug)]
struct OffsetClock(i64);

impl StoreClock for OffsetClock {
    fn now_ms(&self) -> i64 {
        SystemStoreClock.now_ms() + self.0
    }
}

enum Die {
    BeforeCall,
    AfterCall,
    Never,
}

/// The capability with a death wish. Wraps the real client and aborts the whole
/// process at the armed moment.
struct DiesOnCue {
    inner: CouncilClient,
    die: Die,
}

#[async_trait::async_trait]
impl Capability<BookingEffect> for DiesOnCue {
    type Raw = <CouncilClient as Capability<BookingEffect>>::Raw;

    async fn execute(
        &self,
        effect: &BookingEffect,
        attempt: &EffectAttempt,
    ) -> Result<Self::Raw, Unknown> {
        if matches!(self.die, Die::BeforeCall) {
            // Phase A is committed and the attempt is durably recorded; not one
            // byte has reached the council. Lights out.
            std::process::abort();
        }
        let raw = self.inner.execute(effect, attempt).await;
        if matches!(self.die, Die::AfterCall) {
            // The council has answered — its record exists — and ours never
            // will. Lights out.
            std::process::abort();
        }
        raw
    }
}

/// The driver's own grant, issued the way production issues one.
///
/// # Why a demo binary now runs the whole approval flow
///
/// It used to write a five-field struct literal. ADR-025 sealed the envelope —
/// private fields, and no constructor to reach for — precisely so that nothing
/// can assert its own authority, and a demo is not an exception to that. So
/// this is a small composition root: it raises a challenge over the booking it
/// is about to drive, answers it, and carries the resulting grant.
///
/// The store is in-memory because this driver is a single run of a single
/// booking; a deployment binds `SqlApprovalStore` here instead, which is M7B's
/// work.
async fn authority(id: &BookingId) -> VerifiedAuthority {
    use bld_types::Behaviour;
    use townhall_authority::{
        ApprovalCode, ApprovalRequest, AssuranceLevel, AuthorityPolicy, AuthorityService,
        BehaviourSet, BindingRef, Entropy, MemoryApprovalStore, PendingScope,
    };

    /// The demo's fixed code. A real one comes from the OS (M7B).
    struct DemoCode;
    impl Entropy for DemoCode {
        fn code(&self) -> ApprovalCode {
            ApprovalCode::new("7312").expect("four digits")
        }
        fn identifier(&self) -> String {
            format!("driver-{}", std::process::id())
        }
    }

    let now = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map_or(0, |since| {
            u64::try_from(since.as_millis()).unwrap_or(u64::MAX)
        });
    let service = AuthorityService::new(
        MemoryApprovalStore::new(),
        DemoCode,
        AuthorityPolicy::default(),
    );
    let binding = BindingRef {
        principal: PrincipalId::new("lucy"),
        version: 1,
    };
    let raised = service
        .begin(
            &ApprovalRequest {
                scope: PendingScope {
                    service: bld_types::ServiceId::new("demo-council-town-hall"),
                    agent: "bld-driver".to_owned(),
                    booking: id.clone(),
                    behaviours: BehaviourSet::new([Behaviour::Book, Behaviour::Cancel]),
                    requirements: requirements(),
                },
                binding: binding.clone(),
                grantor: PrincipalId::new("lucy"),
                subject: PrincipalId::new("lucy"),
            },
            now,
        )
        .await
        .expect("a challenge over an empty store");
    println!("APPROVAL PREVIEW\n{}", raised.preview);
    service
        .submit(
            &raised.id,
            raised.code.revealed(),
            &binding,
            AssuranceLevel::SmsReply,
            now + 1,
        )
        .await
        .expect("the driver answers its own challenge")
}

fn requirements() -> BookingRequirements {
    BookingRequirements {
        purpose: "community meeting".to_owned(),
        requested_date: "2026-09-01".to_owned(),
        time_window: TimeWindow {
            from: "13:00".to_owned(),
            to: "17:00".to_owned(),
        },
        attendees: 20,
        wheelchair_accessible: true,
        max_fee: Money::from_pence(5_000),
    }
}

struct Args {
    db: String,
    council_url: String,
    key_hex: String,
    /// `Some` runs one booking turn for that id; `None` with `reconcile` runs
    /// recovery only.
    booking_id: Option<String>,
    die: Die,
    /// Milliseconds our store clock runs AHEAD of the system clock. Cadences
    /// are real times; this is how a reconcile run is "later" without sleeping.
    clock_ahead_ms: i64,
    /// Run `due`/`attend` rounds until quiescent instead of a booking turn —
    /// the recovery process, as a process (test 12). With `--die before-call`
    /// the abort lands at the first CAPABILITY entry of the run, which in a
    /// reconcile run is the first SEND recovery decides on: exactly the window
    /// between a handoff's commit and the cancellation call.
    reconcile: bool,
}

fn parse_args() -> Result<Args, String> {
    let mut args = std::env::args().skip(1);
    let (mut db, mut council, mut key, mut booking, mut die) = (None, None, None, None, None);
    let mut reconcile = false;
    let mut clock_ahead_ms = 0;
    while let Some(flag) = args.next() {
        let mut value = || args.next().ok_or_else(|| format!("{flag} needs a value"));
        match flag.as_str() {
            "--db" => db = Some(value()?),
            "--council-url" => council = Some(value()?),
            "--key-hex" => key = Some(value()?),
            "--booking-id" => booking = Some(value()?),
            "--reconcile" => reconcile = true,
            "--clock-ahead-ms" => {
                clock_ahead_ms = value()?
                    .parse::<i64>()
                    .map_err(|_| "--clock-ahead-ms needs milliseconds".to_owned())?;
            }
            "--die" => {
                die = Some(match value()?.as_str() {
                    "before-call" => Die::BeforeCall,
                    "after-call" => Die::AfterCall,
                    "never" => Die::Never,
                    other => return Err(format!("unknown --die {other:?}")),
                });
            }
            other => return Err(format!("unknown flag {other:?}")),
        }
    }
    if booking.is_none() && !reconcile {
        return Err("--booking-id required unless --reconcile".to_owned());
    }
    Ok(Args {
        db: db.ok_or("--db required")?,
        council_url: council.ok_or("--council-url required")?,
        key_hex: key.ok_or("--key-hex required")?,
        booking_id: booking,
        die: die.ok_or("--die required")?,
        clock_ahead_ms,
        reconcile,
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
        Ok(parsed) => parsed,
        Err(problem) => {
            eprintln!("bld-driver: {problem}");
            return ExitCode::from(2);
        }
    };
    let Some(key_bytes) = parse_key(&args.key_hex) else {
        eprintln!("bld-driver: --key-hex must be 64 hex characters");
        return ExitCode::from(2);
    };
    let public =
        council_wire::CouncilSigner::new(council_wire::CouncilSigningKey::from_bytes(&key_bytes))
            .verifying_key();

    let repo = Arc::new(
        SqliteBookingRepository::open_with(
            &args.db,
            townhall_store::DEFAULT_EFFECT_TTL_MS,
            Arc::new(OffsetClock(args.clock_ahead_ms)),
        )
        .await
        .expect("open the repository"),
    );
    let client = CouncilClient::new(&args.council_url, CouncilKey::new(public));
    let capability = Arc::new(DiesOnCue {
        inner: client,
        die: args.die,
    });
    let availability = Arc::new(CouncilClient::new(
        &args.council_url,
        CouncilKey::new(public),
    ));
    let coordinator = Coordinator::new(
        Arc::clone(&repo),
        capability,
        Arc::new(CouncilVerifier::new(CouncilKey::new(public))),
        availability,
    );

    if args.reconcile {
        // Recovery, as a process: rounds of due/attend until quiescent —
        // bounded, because a chase that spins is a bug the harness should see.
        let reconciliation = Reconciliation::new(
            Arc::new(coordinator),
            Arc::new(CouncilClient::new(
                &args.council_url,
                CouncilKey::new(public),
            )),
        );
        for _round in 0..5 {
            let due = reconciliation.due(10).await.expect("due");
            if due.is_empty() {
                break;
            }
            for effect in due {
                let attended = reconciliation.attend(&effect).await.expect("attend");
                println!("ATTENDED {} {attended:?}", effect.as_str());
            }
        }
        return ExitCode::SUCCESS;
    }

    let id = BookingId::new(args.booking_id.expect("checked in parse_args"));
    // Approval FIRST, then the booking (spec §23.1). The grant names this
    // booking, so it has to be minted before anything is created — which is
    // exactly the ordering ADR-025 recorded and M7C wires into the SMS path.
    let authority = authority(&id).await;
    repo.create(NewBooking {
        id: id.clone(),
        requirements: requirements(),
        owner: authority.grantor().clone(),
    })
    .await
    .expect("create");

    for proposal in [
        BookingProposal::SelectVenue {
            venue_id: VenueId::new("TH-A"),
            slot_id: SlotId::new("SLOT-A"),
        },
        BookingProposal::VerifySlot,
        BookingProposal::Book,
    ] {
        let name = proposal.name();
        let outcome = coordinator
            .propose(&id, proposal, &authority)
            .await
            .expect("a turn must not fail at the transport level");
        println!("TURN {name} {outcome:?}");
    }

    ExitCode::SUCCESS
}
