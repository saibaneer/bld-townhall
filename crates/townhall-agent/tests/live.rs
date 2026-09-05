#![cfg(feature = "agent-live")]

//! The `agent-live` lane (M11, ADR-031; spec §18.2): the REAL model completes the
//! booking journey through the same public surface, behind the same driver the
//! hermetic test uses. This is the model-independence acceptance — a locally
//! hosted / open-source model drives a booking to `Booked` without ever touching
//! a council capability, a fee, a version, or a payment.
//!
//! Opt-in and never in CI (it needs a running model endpoint):
//!
//! ```text
//! AGENT_MODEL=glm-5.3:cloud cargo test -p townhall-agent --features agent-live -- --nocapture
//! ```
//!
//! The model/provider is pure config (`AGENT_BASE_URL` / `AGENT_MODEL`), so the
//! SAME test certifies a capable open-weight model (`glm-5.3:cloud` completes the
//! journey in ~21s), a local `qwen3:4b`, or the from-scratch model — the swap
//! changes no invariant.
//!
//! And it demonstrates §18.2 directly: a smaller model (`qwen3:4b`) is less
//! consistent — it may stop early or re-propose Create — but that only REDUCES
//! proposal quality; it never enlarges authority or corrupts state. The boundary
//! refuses every over-reach, and the driver's stall guard ends a stuck run. Safety
//! is the boundary's, not the model's.

use bld_client::{BldClient, discover};
use bld_manifest::signing_key_from_hex;
use townhall_agent::driver::Driver;
use townhall_agent::llm::LlmProposer;
use townhall_agent::openai::{AgentConfig, OpenAiChat};
use townhall_testkit::{MANIFEST_KEY_HEX, world_discoverable};

const BEARER: &str = "dev-lucy";
const PRINCIPAL: &str = "lucy";

#[tokio::test]
async fn a_real_model_drives_a_booking_to_booked() {
    let config = AgentConfig::from_env(|name| std::env::var(name).ok());
    eprintln!(
        "agent-live: model={} endpoint={}",
        config.model, config.base_url
    );
    let proposer = LlmProposer::new(OpenAiChat::new(config));

    let world = world_discoverable();
    let http = reqwest::Client::new();
    let verifying = signing_key_from_hex(MANIFEST_KEY_HEX)
        .expect("64-hex signing key")
        .verifying_key();
    let discovered = discover(&http, &world.server_url, &verifying)
        .await
        .expect("the served manifest verifies");

    let booking = "AGENT-LIVE-1";
    let client = BldClient::new(
        discovered,
        http,
        PRINCIPAL,
        BEARER,
        Some(booking.to_owned()),
    );

    let journey = Driver::new(&client)
        .run(
            &proposer,
            "I'd like to book the town hall for a community meeting of about 20 people, \
             wheelchair accessible, on 2026-09-15 from 13:00 to 17:00, budget up to £50.",
            booking,
            12,
        )
        .await;

    eprintln!("agent-live journey: {journey:#?}");
    // The acceptance (spec §18.2): a locally hosted / open-source model completed
    // the booking journey — it drove the boundary all the way to Booked, choosing
    // only offered behaviours and deciding no authoritative fact.
    assert!(
        journey.reached("Booked") || journey.final_state() == Some("BookingInProgress"),
        "the reference model must drive the booking to Booked: {journey:#?}"
    );

    // Confirm against the authoritative projection (the reconciler converges
    // BookingInProgress -> Booked).
    let mut final_state = journey.final_state().unwrap_or("").to_owned();
    let deadline = std::time::Instant::now() + std::time::Duration::from_secs(15);
    while final_state != "Booked" && std::time::Instant::now() < deadline {
        std::thread::sleep(std::time::Duration::from_millis(150));
        if let Ok(read) = client.read("booking-intents", booking).await {
            final_state = read.state;
        }
    }
    assert_eq!(
        final_state, "Booked",
        "the reference model completed the booking journey end to end"
    );
}
