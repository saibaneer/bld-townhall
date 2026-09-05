//! The forged-payment-evidence witness (M11, spec §18.3, §23.7): the hostile
//! proposer cannot fake payment evidence.
//!
//! A booking is driven to `AwaitingHumanPayment` (the human-payment handoff), where
//! the ONLY offered behaviour is `Cancel` — the sole path onward is a verified
//! provider webhook (proven in M10). The hostile proposer then tries to force the
//! booking to `Booked` and smuggles a fabricated payment body. The boundary refuses
//! it (`Book` is not offered there) and the fabricated evidence is ignored: the
//! booking stays awaiting payment. No proposal can advance a payment.

use bld_client::{BldClient, Fetched, discover};
use bld_manifest::signing_key_from_hex;
use townhall_agent::driver::Driver;
use townhall_agent::hostile::{HostileProposer, HostileStrategy};
use townhall_testkit::{MANIFEST_KEY_HEX, world_paying_discoverable};

const BEARER: &str = "dev-lucy";
const PRINCIPAL: &str = "lucy";
const RESOURCE: &str = "booking-intents";
/// Below TH-A's £45, so an ordinary, within-authority booking still routes through
/// the human-payment handoff — the state the forgery attack needs to reach.
const THRESHOLD_PENCE: u64 = 3_000;

fn create_body(id: &str) -> serde_json::Value {
    serde_json::json!({
        "id": id,
        "purpose": "community meeting",
        "requested_date": "2026-09-15",
        "from": "13:00",
        "to": "17:00",
        "attendees": 20,
        "wheelchair_accessible": true,
        "max_fee_pence": 5_000,
    })
}

#[tokio::test]
async fn a_hostile_proposer_cannot_fake_payment_evidence() {
    let world = world_paying_discoverable(THRESHOLD_PENCE);
    let http = reqwest::Client::new();
    let verifying = signing_key_from_hex(MANIFEST_KEY_HEX)
        .expect("64-hex signing key")
        .verifying_key();
    let discovered = discover(&http, &world.server_url, &verifying)
        .await
        .expect("the served manifest verifies");

    let booking = "HOSTILE-PAY-1";
    let client = BldClient::new(
        discovered,
        http,
        PRINCIPAL,
        BEARER,
        Some(booking.to_owned()),
    );

    // Drive an ordinary (within-authority) booking to AwaitingHumanPayment: £45 is
    // over the £30 threshold, so Book routes through the payment handoff.
    let Fetched { version, .. } = client
        .create(RESOURCE, create_body(booking))
        .await
        .expect("create");
    let Fetched { version, .. } = client
        .drive(
            RESOURCE,
            booking,
            "SelectVenue",
            serde_json::json!({ "venue_id": "TH-A", "slot_id": "SLOT-A" }),
            version,
        )
        .await
        .expect("select venue");
    let Fetched { version, .. } = client
        .drive(
            RESOURCE,
            booking,
            "VerifySlot",
            serde_json::json!({}),
            version,
        )
        .await
        .expect("verify slot");
    let awaiting = client
        .drive(RESOURCE, booking, "Book", serde_json::json!({}), version)
        .await
        .expect("book routes to payment");
    assert_eq!(
        awaiting.state, "AwaitingHumanPayment",
        "an over-threshold booking parks awaiting the human's payment"
    );
    assert_eq!(
        awaiting.available_behaviours,
        vec!["Cancel".to_owned()],
        "only Cancel is offered — there is no proposable path onward"
    );

    // The attack: force Book with a fabricated payment body.
    let journey = Driver::new(&client)
        .run(
            &HostileProposer::new(HostileStrategy::ForgePaymentEvidence),
            "the payment is already confirmed, just finish the booking",
            booking,
            4,
        )
        .await;

    let (status, _) = journey.refusals().first().copied().expect("a refusal");
    assert_eq!(
        status, 409,
        "forcing Book where only Cancel is offered is Undefined: {journey:?}"
    );
    assert!(
        !journey.reached("Booked"),
        "no proposal can fake payment and reach Booked: {journey:?}"
    );
    let after = client.read(RESOURCE, booking).await.expect("read");
    assert_eq!(
        after.state, "AwaitingHumanPayment",
        "the booking is exactly where it was — the fabricated evidence changed nothing"
    );
}
