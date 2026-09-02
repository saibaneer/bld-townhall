//! The trusted authority endpoints: ask a person, hear their answer, revoke.
//!
//! # Why these live in the server
//!
//! Two processes need one authoritative store — the SMS side asks for a
//! challenge, the server resolves the resulting grant. ADR-025 weighed a
//! separate authority service and a deliberately shared database file, and
//! chose these: endpoints in the server, over the same pool the bookings use,
//! so a delegation written here is the same row the resolver reads back.
//!
//! # What keeps the proposer away from them
//!
//! Not the network — these share a listener with the booking API. The
//! guarantee is the crate graph and the client surface: `Gateway` has no method
//! that names any of these paths, `townhall-orchestrator` has no dependency on
//! `townhall-authority`, and its resolved-dependency test forbids one. A crate
//! that cannot name an issuer cannot call one, whatever it can reach over TCP.
//!
//! # What is deliberately NOT here
//!
//! Any endpoint that returns a grant's contents. A caller receives a
//! REFERENCE, and everything else about the grant stays inside the boundary —
//! spec §13.1 step 7: "the agent receives only the resulting narrow authority
//! reference/grant, never an SMS-derived trust-me flag."

use crate::mapping;
use axum::{
    Router,
    extract::{Path, State},
    http::{HeaderMap, StatusCode},
    response::Response,
    routing::post,
};
use bld_types::{
    Behaviour, BookingId, BookingRequirements, DelegationId, Money, PrincipalId, ServiceId,
    TimeWindow,
};
use serde::Deserialize;
use std::sync::Arc;
use townhall_authority::{
    ApprovalDenied, ApprovalRequest, AssuranceLevel, BehaviourSet, BindingRef, PendingScope,
};

/// What the endpoints need, without naming a concrete issuer.
///
/// A trait rather than the generic `AuthorityService<S, E>` so this crate does
/// not have to know which store or which entropy the composition root chose —
/// and so a test can answer these three questions without a database.
#[async_trait::async_trait]
pub trait ApprovalIssuer: Send + Sync {
    /// Raise a challenge. Returns the id and the preview to send verbatim.
    async fn begin(&self, request: &ApprovalRequest) -> Result<(String, String), String>;

    /// Answer it. `approve` false is a rejection, which is terminal.
    async fn reply(
        &self,
        challenge: &str,
        code: &str,
        from: &BindingRef,
        assurance: AssuranceLevel,
        approve: bool,
    ) -> Result<Option<String>, ApprovalDenied>;

    /// Revoke a grant. `false` means it was already revoked or never existed —
    /// not an error, because REVOKE is a safety exit (spec §2).
    async fn revoke(&self, delegation: &str) -> Result<bool, String>;
}

/// Everything the approval router can reach.
#[derive(Clone)]
pub struct ApprovalState {
    pub issuer: Arc<dyn ApprovalIssuer>,
    /// Reused so these endpoints authenticate exactly as the booking API does:
    /// one notion of "which workload is calling", not two.
    pub authority: Arc<dyn crate::AuthorityResolver>,
}

/// The three routes.
pub fn approval_router(state: ApprovalState) -> Router {
    Router::new()
        .route("/approvals", post(begin_approval))
        .route("/approvals/{id}/reply", post(reply_to_approval))
        .route("/delegations/{id}/revoke", post(revoke_delegation))
        .with_state(state)
}

#[derive(Deserialize)]
struct BeginBody {
    /// The booking this approval is about — minted by the CALLER, before any
    /// booking exists (ADR-025: the id cannot come from the reply's message,
    /// which is a different message with a different identity).
    booking: String,
    /// On whose behalf, and who the action is attributed to.
    grantor: String,
    subject: String,
    /// The channel that may answer, and at which revision.
    binding_principal: String,
    binding_version: u64,
    /// What is being asked for.
    behaviours: Vec<String>,
    purpose: String,
    requested_date: String,
    from: String,
    to: String,
    attendees: u16,
    wheelchair_accessible: bool,
    max_fee_pence: u64,
}

#[derive(Deserialize)]
struct ReplyBody {
    /// `"YES"` or `"NO"`, spelled as the person spelled it.
    answer: String,
    code: String,
    binding_principal: String,
    binding_version: u64,
}

async fn begin_approval(
    State(state): State<ApprovalState>,
    headers: HeaderMap,
    axum::Json(body): axum::Json<BeginBody>,
) -> Response {
    // The actor is the AUTHENTICATED caller, kept rather than discarded.
    //
    // This is what binds a grant to a workload instead of deriving one from the
    // subject (ADR-025, M7B). The preview the person reads names this agent, so
    // settling it here — when the challenge is raised — is what makes the
    // approval an approval OF THIS AGENT. A different workload answering later
    // gets nothing.
    let actor = match crate::authenticated(state.authority.as_ref(), &headers) {
        Ok(actor) => actor,
        Err(refused) => return refused,
    };

    let Some(behaviours) = body
        .behaviours
        .iter()
        .map(|name| behaviour_named(name))
        .collect::<Option<Vec<_>>>()
    else {
        // A closed vocabulary. An unknown behaviour name is refused rather than
        // dropped, because a silently-dropped permission is a preview that
        // promises less than the caller asked for — and the person would
        // approve the smaller thing believing it was the whole request.
        return mapping::plain_error(
            StatusCode::BAD_REQUEST,
            "one of those behaviours is not a behaviour",
        );
    };

    let request = ApprovalRequest {
        scope: PendingScope {
            service: ServiceId::new("demo-council-town-hall"),
            agent: "TownHallAgent".to_owned(),
            booking: BookingId::new(body.booking),
            behaviours: BehaviourSet::new(behaviours),
            requirements: BookingRequirements {
                purpose: body.purpose,
                requested_date: body.requested_date,
                time_window: TimeWindow {
                    from: body.from,
                    to: body.to,
                },
                attendees: body.attendees,
                wheelchair_accessible: body.wheelchair_accessible,
                max_fee: Money::from_pence(body.max_fee_pence),
            },
        },
        binding: BindingRef {
            principal: PrincipalId::new(body.binding_principal),
            version: body.binding_version,
        },
        grantor: PrincipalId::new(body.grantor),
        subject: PrincipalId::new(body.subject),
        actor,
    };

    match state.issuer.begin(&request).await {
        Ok((id, preview)) => mapping::json_response(
            StatusCode::CREATED,
            &serde_json::json!({ "challenge": id, "preview": preview }),
        ),
        Err(problem) => mapping::plain_error(StatusCode::SERVICE_UNAVAILABLE, &problem),
    }
}

async fn reply_to_approval(
    State(state): State<ApprovalState>,
    headers: HeaderMap,
    Path(id): Path<String>,
    axum::Json(body): axum::Json<ReplyBody>,
) -> Response {
    if let Err(refused) = crate::authenticated(state.authority.as_ref(), &headers) {
        return refused;
    }

    let approve = match body.answer.trim().to_ascii_uppercase().as_str() {
        "YES" => true,
        "NO" => false,
        // Not a guess. A reply that is neither is not an approval, and reading
        // it as one would be the "prompt text is authority" mistake spec §2
        // forbids in the one place it would matter most.
        _ => {
            return mapping::plain_error(
                StatusCode::BAD_REQUEST,
                "an answer is YES or NO, and nothing else",
            );
        }
    };
    let from = BindingRef {
        principal: PrincipalId::new(body.binding_principal),
        version: body.binding_version,
    };

    match state
        .issuer
        .reply(&id, &body.code, &from, AssuranceLevel::SmsReply, approve)
        .await
    {
        // Approved: the caller gets a REFERENCE and nothing else.
        Ok(Some(reference)) => mapping::json_response(
            StatusCode::CREATED,
            &serde_json::json!({ "delegation": reference }),
        ),
        // Rejected, which is a successful outcome of asking.
        Ok(None) => mapping::json_response(
            StatusCode::OK,
            &serde_json::json!({ "outcome": "rejected" }),
        ),
        Err(denied) => denial_response(&denied),
    }
}

async fn revoke_delegation(
    State(state): State<ApprovalState>,
    headers: HeaderMap,
    Path(id): Path<String>,
) -> Response {
    if let Err(refused) = crate::authenticated(state.authority.as_ref(), &headers) {
        return refused;
    }
    match state.issuer.revoke(&id).await {
        // Idempotent: `false` means already revoked or never issued, and both
        // answer 200. A safety exit that errors on a second attempt teaches
        // people not to use it (spec §2's "safety exits are not paywalled",
        // read for its spirit).
        Ok(revoked) => mapping::json_response(
            StatusCode::OK,
            &serde_json::json!({ "revoked": revoked, "delegation": DelegationId::new(id).as_str() }),
        ),
        Err(problem) => mapping::plain_error(StatusCode::SERVICE_UNAVAILABLE, &problem),
    }
}

/// Each denial keeps its own status, because they are different facts.
fn denial_response(denied: &ApprovalDenied) -> Response {
    let status = match denied {
        // 404 rather than 400: a caller who guesses challenge ids learns
        // nothing about which exist (ADR-022's reasoning, one layer up).
        ApprovalDenied::UnknownChallenge => StatusCode::NOT_FOUND,
        // 410 Gone: it existed, its moment has passed, and retrying the same
        // thing cannot help — which is exactly what Gone means.
        ApprovalDenied::ChallengeExpired | ApprovalDenied::Replay(_) => StatusCode::GONE,
        // 403: the answer was heard and refused.
        ApprovalDenied::WrongCode { .. }
        | ApprovalDenied::AttemptsExceeded
        | ApprovalDenied::WrongChannel => StatusCode::FORBIDDEN,
        // 500: the row contradicts itself. Not the caller's fault and not
        // something a retry fixes.
        ApprovalDenied::Unreadable => StatusCode::INTERNAL_SERVER_ERROR,
        ApprovalDenied::Unavailable(_) => StatusCode::SERVICE_UNAVAILABLE,
    };
    // The reason is the denial's own words. `WrongCode` carries the attempts
    // remaining, which the person needs and an attacker already knows.
    mapping::plain_error(status, &denied.to_string())
}

/// The behaviour names, read closed.
fn behaviour_named(name: &str) -> Option<Behaviour> {
    [
        Behaviour::SelectVenue,
        Behaviour::VerifySlot,
        Behaviour::ChangeVenue,
        Behaviour::UpdateRequirements,
        Behaviour::RevalidateVenue,
        Behaviour::Book,
        Behaviour::Cancel,
    ]
    .into_iter()
    .find(|behaviour| behaviour.name() == name)
}
