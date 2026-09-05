//! Spec §10.2, as one set of exhaustive functions. A new outcome or error
//! variant fails to compile until someone classifies it — the mapping can
//! never silently guess (ADR-021). The unit tests call THESE functions; a
//! copied table in a test would be a fake test.

use axum::{
    http::{StatusCode, header},
    response::{IntoResponse, Response},
};
use townhall_domain::{BookingError, FeeCeiling};
use townhall_service::{ApiError, Mutated, Projection};

// The kernel's outcome vocabulary arrives through townhall-service's `Turn`
// alias — this crate deliberately has no bld-kernel dependency of its own,
// and enum variants are nameable through the alias's own paths.

/// Which HTTP class a DENIAL belongs to (spec §10.2): authority-classed errors
/// are 403, could-not-ask is 503, everything else is a 422 guard story.
/// Exhaustive — a new `BookingError` variant fails to compile until classified.
#[must_use]
pub fn denial_status(error: &BookingError) -> StatusCode {
    match error {
        BookingError::BookingAuthorityRequired
        | BookingError::CancellationAuthorityRequired
        | BookingError::FeeExceeded {
            ceiling: FeeCeiling::Authority,
        }
        | BookingError::AttendeesExceedApproval { .. } => StatusCode::FORBIDDEN,
        BookingError::FactsUnavailable => StatusCode::SERVICE_UNAVAILABLE,
        BookingError::VenueFactsMissing
        | BookingError::SlotUnavailable
        | BookingError::CapacityInsufficient { .. }
        | BookingError::AccessibilityRequired
        | BookingError::FeeExceeded {
            ceiling: FeeCeiling::Requirement,
        }
        | BookingError::EffectIdentityMissing
        | BookingError::EffectPlanMissing
        | BookingError::EffectMismatch
        | BookingError::EffectKindMismatch
        | BookingError::EffectPlanMismatch { .. }
        | BookingError::DuplicateProviderEffect
        | BookingError::ContradictoryProviderFact
        | BookingError::InconsistentEffectIdentity
        | BookingError::IncoherentAggregate(_)
        | BookingError::IncoherentIntent(_) => StatusCode::UNPROCESSABLE_ENTITY,
    }
}

/// One turn, one response (spec §10.2's rows for typed outcomes).
#[must_use]
pub fn turn(mutated: &Mutated, projection: Option<&Projection>) -> Response {
    let etag = etag_header(mutated.current_version);
    match &mutated.outcome {
        townhall_service::Turn::Committed(aggregate) => {
            let body = serde_json::json!({
                "id": aggregate.id.to_string(),
                "version": aggregate.version,
                "state": aggregate.state.name(),
                "available_behaviours": aggregate.state.proposal_menu(),
            });
            (StatusCode::OK, [etag], axum::Json(body)).into_response()
        }
        townhall_service::Turn::Converged => {
            let body = serde_json::json!({
                "state": projection.map(|projection| projection.state),
                "converged": true,
            });
            (StatusCode::OK, [etag], axum::Json(body)).into_response()
        }
        townhall_service::Turn::Undefined => {
            let body = serde_json::json!({
                "error": "no such behaviour in this state",
                "available_behaviours": projection.map(|projection| projection.available_behaviours),
            });
            (StatusCode::CONFLICT, [etag], axum::Json(body)).into_response()
        }
        townhall_service::Turn::Denied(error) => {
            let body = serde_json::json!({
                "error": error.name(),
                "detail": error.to_string(),
            });
            (denial_status(error), [etag], axum::Json(body)).into_response()
        }
        // 202: a durable intent exists, its outcome is honestly unknowable
        // right now, and the chase owns it (ADR-019's 202/503 rule). The
        // Retry-After is the STORE's schedule, ceiling-rounded to seconds.
        townhall_service::Turn::Unresolved => {
            let seconds = mutated
                .retry_after_ms
                .map_or(1, |ms| {
                    ms.div_euclid(1000) + i64::from(ms.rem_euclid(1000) > 0)
                })
                .max(1);
            let body = serde_json::json!({
                "status": "accepted",
                "detail": "the outcome is not yet knowable; reconciliation owns it",
            });
            (
                StatusCode::ACCEPTED,
                [etag, (header::RETRY_AFTER, seconds.to_string())],
                axum::Json(body),
            )
                .into_response()
        }
    }
}

/// The facade's closed error vocabulary, mapped (spec §10.2's rows for
/// non-outcome failures). Exhaustive.
#[must_use]
pub fn api_error(error: &ApiError) -> Response {
    match error {
        ApiError::UnknownBooking => plain_error(StatusCode::NOT_FOUND, "no such booking"),
        ApiError::AlreadyExists { current } => (
            StatusCode::CONFLICT,
            [etag_header(*current)],
            axum::Json(serde_json::json!({
                "error": "the booking already exists",
                "version": current,
            })),
        )
            .into_response(),
        // The same 409 number, deliberately without the state.
        //
        // The owner's duplicate ships a version and an `ETag` so a retry can
        // carry a precondition. A stranger's ships neither: they learn the
        // identifier is taken and nothing else — not the version, not the state,
        // not who holds it. The one remaining bit (taken or free) is unavoidable
        // under a caller-chosen primary key, and a 404 here would leak the same
        // bit while misdescribing a POST to a collection that does exist
        // (ADR-022's accepted residual).
        ApiError::IdentifierUnavailable => {
            plain_error(StatusCode::CONFLICT, "that identifier is unavailable")
        }
        ApiError::PreconditionFailed { current } => (
            StatusCode::PRECONDITION_FAILED,
            [etag_header(*current)],
            axum::Json(serde_json::json!({
                "error": "the resource changed since it was observed",
                "version": current,
            })),
        )
            .into_response(),
        // RFC 6585: a 429 tells the caller when to come back.
        ApiError::Contended => (
            StatusCode::TOO_MANY_REQUESTS,
            [(header::RETRY_AFTER, "1".to_owned())],
            axum::Json(serde_json::json!({
                "error": "contention budget exhausted; re-read and retry",
            })),
        )
            .into_response(),
        ApiError::Internal(detail) => plain_error(StatusCode::INTERNAL_SERVER_ERROR, detail),
        ApiError::Unavailable(detail) => plain_error(StatusCode::SERVICE_UNAVAILABLE, detail),
    }
}

/// A projection, as a response: the `ETag` is the version, quoted.
#[must_use]
pub fn projection_response(status: StatusCode, projection: &Projection) -> Response {
    let body = projection_body(projection);
    (status, [etag_header(projection.version)], axum::Json(body)).into_response()
}

/// One booking's JSON, without status or headers.
///
/// Shared by the single read and the collection listing so the two cannot
/// describe the same resource differently — a client that learned a shape from
/// a list should be able to re-read one member and recognise it.
#[must_use]
pub fn projection_body(projection: &Projection) -> serde_json::Value {
    serde_json::json!({
        "id": projection.id.to_string(),
        "version": projection.version,
        "state": projection.state,
        "requirements": {
            "purpose": projection.requirements.purpose,
            "requested_date": projection.requirements.requested_date,
            "from": projection.requirements.time_window.from,
            "to": projection.requirements.time_window.to,
            "attendees": projection.requirements.attendees,
            "wheelchair_accessible": projection.requirements.wheelchair_accessible,
            "max_fee_pence": projection.requirements.max_fee.pence(),
        },
        "selected_venue": projection.selected_venue.as_ref().map(|selection| {
            serde_json::json!({
                "venue_id": selection.venue_id.to_string(),
                "slot_id": selection.slot_id.to_string(),
            })
        }),
        "booking_ref": projection.booking_ref.as_ref().map(ToString::to_string),
        "checkout_url": projection.checkout_url,
        "available_behaviours": projection.available_behaviours,
    })
}

#[must_use]
pub fn plain_error(status: StatusCode, detail: &str) -> Response {
    (status, axum::Json(serde_json::json!({ "error": detail }))).into_response()
}

/// A JSON body with a status and nothing else.
///
/// The approval endpoints carry no resource version, so none of the `ETag`
/// machinery above applies to them: a challenge is not a resource a caller
/// mutates with `If-Match`, it is a question somebody answers once.
pub fn json_response(status: StatusCode, body: &serde_json::Value) -> Response {
    (status, axum::Json(body.clone())).into_response()
}

fn etag_header(version: u64) -> (header::HeaderName, String) {
    (header::ETAG, format!("\"{version}\""))
}
