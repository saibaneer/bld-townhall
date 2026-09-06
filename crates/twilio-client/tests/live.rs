#![cfg(feature = "twilio-live")]

//! The `twilio-live` lane (M12): the SAME `send_sms` adapter, driven against the
//! REAL Twilio API at `api.twilio.com` rather than the hermetic in-process mock.
//!
//! It exists to prove the one thing the mock cannot: that our REQUEST ENCODING
//! and RESPONSE PARSING match real Twilio — the basic auth, the form fields, the
//! JSON shape of a Message resource — not merely the mock we wrote to mirror
//! them. A mock and its adapter can drift together and stay green; real Twilio
//! cannot be talked into agreeing with a wrong request.
//!
//! OPT-IN, never part of a normal `cargo test`: it needs live credentials and
//! network, so it lives behind the `twilio-live` feature. It sends ONE real SMS,
//! so it also needs a recipient — read from `TWILIO_TEST_TO` (your verified
//! number) so no personal number is ever committed.
//!
//! ```text
//! set -a; source .env; set +a          # exports TWILIO_SID / _CLIENT_SECRET / _FROM_NUMBER
//! TWILIO_TEST_TO=+44…  cargo test -p twilio-client --features twilio-live -- --nocapture
//! ```
//!
//! What it does NOT cover: receiving a real inbound webhook (that needs a public
//! tunnel, M12 increment 2). The inbound signature FORMAT is locked hermetically
//! against an independent OpenSSL vector in the crate's `verify_signature` tests.

use twilio_client::{TwilioClient, TwilioConfig};

/// Missing config means FAIL LOUDLY, never a silent skip: a skipped live lane
/// lets "the live lane is green" quietly mean "the live lane never ran".
fn client() -> TwilioClient {
    let config = TwilioConfig::from_env(|name| std::env::var(name).ok()).expect(
        "the twilio-live lane needs TWILIO_SID, TWILIO_CLIENT_SECRET and TWILIO_FROM_NUMBER \
         in the environment (source your .env first)",
    );
    TwilioClient::new(reqwest::Client::new(), config)
}

#[tokio::test]
async fn a_real_sms_sends_and_returns_a_message_sid() {
    let to = std::env::var("TWILIO_TEST_TO").expect(
        "set TWILIO_TEST_TO to your verified recipient number (a trial account can only text \
         verified numbers)",
    );

    let sent = client()
        .send_sms(
            &to,
            "BLD boundary — M12 live SMS adapter is up. (twilio-live lane)",
        )
        .await
        .expect("a real send to a verified number succeeds");

    eprintln!(
        "twilio-live: sent SID {} (status {})",
        sent.sid, sent.status
    );
    assert!(
        sent.sid.starts_with("SM"),
        "a real Twilio message SID starts with SM: got {}",
        sent.sid
    );
    assert!(
        !sent.status.is_empty(),
        "Twilio reports an initial queue status"
    );
}
