//! The classifier against responses the real server will not send.
//!
//! An untrusted driver polices the wire, and the malformed shapes it must
//! refuse are exactly the ones a well-behaved test server cannot be made to
//! produce — so they are constructed here, and the classifier is pure so that
//! constructing them is enough.

use townhall_gateway::dto::RawResponse;
use townhall_gateway::{GatewayError, Turn, classify_error, classify_turn};

fn response(status: u16, etag: Option<u64>, retry_after: Option<u64>, body: &str) -> RawResponse {
    RawResponse {
        status,
        etag,
        retry_after,
        request_id: None,
        body: serde_json::from_str(body).expect("test json"),
    }
}

/// A 202 without its schedule is not an acceptance.
///
/// The first classifier defaulted the missing wait to one second — inventing a
/// schedule for a server that stopped keeping its contract, and turning a
/// malformed `202 {}` into a legitimate turn.
#[test]
fn a_naked_202_is_refused_not_defaulted() {
    let malformed = response(202, None, None, "{}");
    assert!(matches!(
        classify_turn(&malformed),
        Err(GatewayError::Unrecognized(_))
    ));
    // Missing either half alone is refused too.
    assert!(matches!(
        classify_turn(&response(202, Some(3), None, "{}")),
        Err(GatewayError::Unrecognized(_))
    ));
    assert!(matches!(
        classify_turn(&response(202, None, Some(1), "{}")),
        Err(GatewayError::Unrecognized(_))
    ));
    // The genuine shape still classifies.
    let genuine = response(202, Some(3), Some(2), r#"{"status":"accepted"}"#);
    assert!(matches!(
        classify_turn(&genuine),
        Ok(Turn::Accepted { retry_after }) if retry_after.as_secs() == 2
    ));
}

/// A denial without the domain's NAME is not a denial.
///
/// `422 + ETag + {"detail":"x"}` used to become `Denied("(no error field)")` —
/// a refusal quoting an error nobody sent.
#[test]
fn a_denial_needs_its_name() {
    let nameless = response(422, Some(4), None, r#"{"detail":"x"}"#);
    assert!(
        matches!(classify_turn(&nameless), Err(GatewayError::Malformed(_))),
        "no name + no denial shape = the malformed-body 422"
    );
    let named = response(
        422,
        Some(4),
        None,
        r#"{"error":"CapacityInsufficient","detail":"30 < 999"}"#,
    );
    assert!(matches!(
        classify_turn(&named),
        Ok(Turn::Denied { reason }) if reason == "CapacityInsufficient"
    ));
    // A bare 403 with no denial shape is refused rather than guessed at.
    assert!(matches!(
        classify_turn(&response(403, None, None, "{}")),
        Err(GatewayError::Unrecognized(_))
    ));
}

/// The two 409 shapes, and the hybrid that is neither.
#[test]
fn the_409_shapes_are_owner_generic_or_refused() {
    let owners = response(409, Some(0), None, r#"{"error":"exists","version":0}"#);
    assert!(matches!(
        classify_error(&owners),
        GatewayError::Existing { current: 0 }
    ));
    let strangers = response(409, None, None, r#"{"error":"identifier unavailable"}"#);
    assert!(matches!(
        classify_error(&strangers),
        GatewayError::IdentifierUnavailable
    ));
    // An ETag with no version field is neither contract — refuse to guess
    // which half the server meant.
    let hybrid = response(409, Some(3), None, r#"{"error":"exists"}"#);
    assert!(matches!(
        classify_error(&hybrid),
        GatewayError::Unrecognized(_)
    ));
}

/// The two 503 shapes, keyed on the distinguisher.
#[test]
fn the_503_shapes_are_silence_or_absence() {
    let denial = response(
        503,
        Some(2),
        None,
        r#"{"error":"FactsUnavailable","detail":"could not ask"}"#,
    );
    assert!(matches!(
        classify_error(&denial),
        GatewayError::ProviderSilent(_)
    ));
    let plain = response(
        503,
        None,
        None,
        r#"{"error":"the catalogue could not be asked"}"#,
    );
    assert!(matches!(
        classify_error(&plain),
        GatewayError::Unavailable(_)
    ));
}

/// A committed turn without its state or tag is refused.
#[test]
fn a_committed_turn_needs_its_state_and_tag() {
    assert!(matches!(
        classify_turn(&response(200, None, None, r#"{"state":"Booked"}"#)),
        Err(GatewayError::Unrecognized(_))
    ));
    assert!(matches!(
        classify_turn(&response(200, Some(3), None, r#"{"version":3}"#)),
        Err(GatewayError::Unrecognized(_))
    ));
    assert!(matches!(
        classify_turn(&response(
            200,
            Some(3),
            None,
            r#"{"state":"Booked","version":3}"#
        )),
        Ok(Turn::Committed { version: 3, .. })
    ));
    // A 412 whose current version is missing cannot say what to retry with.
    assert!(matches!(
        classify_error(&response(412, None, None, r#"{"error":"stale"}"#)),
        GatewayError::Unrecognized(_)
    ));
}
