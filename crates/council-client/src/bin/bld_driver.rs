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
    ActorId, BookingId, BookingRequirements, EffectAttempt, Money, PrincipalId, SlotId, TimeWindow,
    VenueId,
};
use council_client::{CouncilClient, CouncilVerifier};
use council_wire::CouncilKey;
use std::{process::ExitCode, sync::Arc};
use townhall_domain::{BookingEffect, BookingProposal, VerifiedAuthority};
use townhall_service::Coordinator;
use townhall_store::{BookingRepository as _, NewBooking, SqliteBookingRepository};

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

fn authority() -> VerifiedAuthority {
    VerifiedAuthority {
        principal: PrincipalId::new("lucy"),
        actor: ActorId::new("agent-1"),
        max_fee: Money::from_pence(5_000),
        may_book: true,
        may_cancel: true,
    }
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

fn parse_args() -> Result<(String, String, String, String, Die), String> {
    let mut args = std::env::args().skip(1);
    let (mut db, mut council, mut key, mut booking, mut die) = (None, None, None, None, None);
    while let Some(flag) = args.next() {
        let mut value = || args.next().ok_or_else(|| format!("{flag} needs a value"));
        match flag.as_str() {
            "--db" => db = Some(value()?),
            "--council-url" => council = Some(value()?),
            "--key-hex" => key = Some(value()?),
            "--booking-id" => booking = Some(value()?),
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
    Ok((
        db.ok_or("--db required")?,
        council.ok_or("--council-url required")?,
        key.ok_or("--key-hex required")?,
        booking.ok_or("--booking-id required")?,
        die.ok_or("--die required")?,
    ))
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
    let (db, council_url, key_hex, booking_id, die) = match parse_args() {
        Ok(parsed) => parsed,
        Err(problem) => {
            eprintln!("bld-driver: {problem}");
            return ExitCode::from(2);
        }
    };
    let Some(key_bytes) = parse_key(&key_hex) else {
        eprintln!("bld-driver: --key-hex must be 64 hex characters");
        return ExitCode::from(2);
    };
    let public =
        council_wire::CouncilSigner::new(council_wire::CouncilSigningKey::from_bytes(&key_bytes))
            .verifying_key();

    let repo = Arc::new(
        SqliteBookingRepository::open(&db)
            .await
            .expect("open the repository"),
    );
    let client = CouncilClient::new(&council_url, CouncilKey::new(public));
    let capability = Arc::new(DiesOnCue { inner: client, die });
    let availability = Arc::new(CouncilClient::new(&council_url, CouncilKey::new(public)));
    let coordinator = Coordinator::new(
        Arc::clone(&repo),
        capability,
        Arc::new(CouncilVerifier::new(CouncilKey::new(public))),
        availability,
    );

    let id = BookingId::new(booking_id);
    repo.create(NewBooking {
        id: id.clone(),
        requirements: requirements(),
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
            .propose(&id, proposal, &authority())
            .await
            .expect("a turn must not fail at the transport level");
        println!("TURN {name} {outcome:?}");
    }

    ExitCode::SUCCESS
}
