//! The driver over the real wire (M11, ADR-031): a proposer drives a booking to
//! `Booked` through a discovered service — the same public surface the M9 client
//! test uses, now behind the [`Proposer`] seam.
//!
//! This layer proves the DRIVER with a deterministic helpful proposer, so the
//! result is stable and CI stays hermetic (no model, no network beyond the
//! spawned server). The real LLM completing the journey is the opt-in `agent-live`
//! lane; the hostile proposer's refusals are the M11 adversarial suite. All three
//! ride this same driver.

use async_trait::async_trait;
use bld_client::{BldClient, discover};
use bld_manifest::signing_key_from_hex;
use townhall_agent::driver::Driver;
use townhall_agent::{ProjectedContext, ProposedAction, Proposer};
use townhall_testkit::{MANIFEST_KEY_HEX, world_discoverable};

const BEARER: &str = "dev-lucy";
const PRINCIPAL: &str = "lucy";

/// A deterministic HELPFUL proposer: given the state, it proposes the obvious next
/// step of the happy booking journey. It stands in for the LLM so the driver is
/// tested without model non-determinism — the LLM proposer rides the exact same
/// seam in the live lane.
struct Scripted;

#[async_trait]
impl Proposer for Scripted {
    async fn propose(&self, ctx: &ProjectedContext) -> ProposedAction {
        match ctx.state.as_deref() {
            // No booking yet — create one with the requirements.
            None => ProposedAction::Create {
                body: serde_json::json!({
                    "purpose": "community meeting",
                    "requested_date": "2026-09-15",
                    "from": "13:00",
                    "to": "17:00",
                    "attendees": 20,
                    "wheelchair_accessible": true,
                    "max_fee_pence": 5_000,
                }),
            },
            Some("Draft") => drive(ctx, "SelectVenue", venue_body(ctx)),
            Some("VenueSelected") => drive(ctx, "VerifySlot", serde_json::json!({})),
            Some("AwaitingBooking") => drive(ctx, "Book", serde_json::json!({})),
            // BookingInProgress / Booked / anything else — the agent is done; the
            // reconciler owns the convergence to Booked.
            _ => ProposedAction::Done,
        }
    }
}

/// Pick the first affordable, accessible, large-enough venue from the browse — a
/// helpful proposer chooses from what the projection surfaced, it does not invent.
fn venue_body(ctx: &ProjectedContext) -> serde_json::Value {
    let choice = ctx
        .venues
        .iter()
        .find(|v| v.accessible && v.capacity >= 20 && v.fee_pence <= 5_000)
        .or_else(|| ctx.venues.first());
    match choice {
        Some(v) => serde_json::json!({ "venue_id": v.venue_id, "slot_id": v.slot_id }),
        None => serde_json::json!({ "venue_id": "TH-A", "slot_id": "SLOT-A" }),
    }
}

fn drive(ctx: &ProjectedContext, behaviour: &str, body: serde_json::Value) -> ProposedAction {
    ProposedAction::Drive {
        behaviour: behaviour.to_owned(),
        body,
        if_match: ctx.version,
    }
}

#[tokio::test]
async fn a_proposer_drives_a_booking_to_booked_through_the_public_surface() {
    let world = world_discoverable();
    let http = reqwest::Client::new();
    let verifying = signing_key_from_hex(MANIFEST_KEY_HEX)
        .expect("64-hex signing key")
        .verifying_key();

    let discovered = discover(&http, &world.server_url, &verifying)
        .await
        .expect("the served manifest verifies");

    let booking = "AGENT-HAPPY-1";
    // Dev lane: the delegation reference IS the booking id (DevAuthority).
    let client = BldClient::new(
        discovered,
        http,
        PRINCIPAL,
        BEARER,
        Some(booking.to_owned()),
    );

    let journey = Driver::new(&client)
        .run(
            &Scripted,
            "book the hall for a community meeting",
            booking,
            8,
        )
        .await;

    // The agent chose a venue from the browse (not a hard-coded id) and every
    // proposal it made was committed — no refusals on the happy path.
    assert!(
        journey.refusals().is_empty(),
        "the happy path takes no refusals: {:?}",
        journey.refusals()
    );
    assert!(
        journey.reached("AwaitingBooking"),
        "verification put a below-threshold booking in AwaitingBooking: {journey:?}"
    );

    // Book commits BookingInProgress; the reconciler converges it to Booked. Poll
    // the authoritative projection until it does (the stated no-sleep exception).
    let mut final_state = journey.final_state().unwrap_or("").to_owned();
    let deadline = std::time::Instant::now() + std::time::Duration::from_secs(10);
    while final_state != "Booked" && std::time::Instant::now() < deadline {
        std::thread::sleep(std::time::Duration::from_millis(100));
        if let Ok(read) = client.read("booking-intents", booking).await {
            final_state = read.state;
        }
    }
    assert_eq!(
        final_state, "Booked",
        "the proposer's booking converges to Booked at the council"
    );
}
