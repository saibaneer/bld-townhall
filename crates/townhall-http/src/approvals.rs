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
//! that names any of these paths, and `townhall-orchestrator` may not depend on
//! `townhall-authority` — forbidden by its resolved-dependency test
//! (`crates/townhall-orchestrator/tests/boundary.rs`). A crate that cannot name
//! an issuer cannot call one, whatever it can reach over TCP.
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
    ActorId, Behaviour, BookingId, BookingRequirements, DelegationId, Money, PrincipalId,
    ServiceId, TimeWindow,
};
use serde::Deserialize;
use std::sync::Arc;
use townhall_authority::{ApprovalDenied, ApprovalRequest, BehaviourSet, BindingRef, PendingScope};

/// What the endpoints need, without naming a concrete issuer.
///
/// A trait rather than the generic `AuthorityService<S, E>` so this crate does
/// not have to know which store or which entropy the composition root chose —
/// and so a test can answer these three questions without a database.
#[async_trait::async_trait]
pub trait ApprovalIssuer: Send + Sync {
    /// Raise a challenge, or return the one this inbound intent already has.
    ///
    /// `created` is false when a redelivered `BOOK` reused an existing
    /// challenge — same id, same code — so the caller can answer `200` rather
    /// than `201` without raising a second, contradictory prompt.
    async fn begin(&self, request: &ApprovalRequest) -> Result<(bool, String, String), String>;

    /// Deposit an inbound reply's transport evidence under a one-use receipt.
    ///
    /// The trusted ingress calls this; it returns the challenge the reply
    /// answers and an opaque receipt. The orchestrator then forwards
    /// `challenge + code + receipt` to [`Self::reply`] — never the evidence
    /// itself, which it cannot forge into a row it did not write (ADR-026).
    async fn deposit_evidence(
        &self,
        inbound: &InboundEvidence,
    ) -> Result<(String, String), ApprovalDenied>;

    /// Answer it with a forwarded receipt. `approve` false is a rejection,
    /// which is terminal. `actor` is the authenticated caller, used only to
    /// return the SAME reference to a replayed `YES`, never to name the grant.
    async fn reply(
        &self,
        challenge: &str,
        code: &str,
        receipt: &str,
        actor: &ActorId,
        approve: bool,
    ) -> Result<Option<String>, ApprovalDenied>;

    /// Revoke a grant. `false` means it was already revoked or never existed —
    /// not an error, because REVOKE is a safety exit (spec §2).
    async fn revoke(&self, delegation: &str) -> Result<bool, String>;

    /// Revoke EVERY live grant for the principal whose bound channel sent this
    /// control inbound. Deposits the transport evidence and sweeps in one server
    /// operation, so the receipt never leaves the server. Returns the count
    /// revoked; idempotent (a re-sent REVOKE returns the count still stopped).
    async fn revoke_via_receipt(&self, inbound: &InboundEvidence) -> Result<u64, ApprovalDenied>;
}

/// One inbound reply's transport evidence, as the trusted ingress presents it.
///
/// The identity triple is transport-set: the ingress fills it from the carrier,
/// and it is what stops a caller naming a row into being by choosing a sender.
#[derive(Deserialize)]
pub struct InboundEvidence {
    pub provider: String,
    pub account: String,
    pub message_id: String,
    /// The number the reply came from, normalized. Both the routing key and the
    /// evidence's `claimed_sender`.
    pub address: String,
    pub verified: bool,
    pub signature: Option<String>,
}

/// Everything the approval router can reach.
#[derive(Clone)]
pub struct ApprovalState {
    pub issuer: Arc<dyn ApprovalIssuer>,
    /// Reused so these endpoints authenticate exactly as the booking API does:
    /// one notion of "which workload is calling", not two.
    pub authority: Arc<dyn crate::AuthorityResolver>,
}

/// The routes.
pub fn approval_router(state: ApprovalState) -> Router {
    Router::new()
        .route("/approvals", post(begin_approval))
        .route("/inbound-evidence", post(deposit_evidence))
        .route("/approvals/{id}/reply", post(reply_to_approval))
        .route("/delegations/{id}/revoke", post(revoke_delegation))
        .route("/revocations", post(revoke_via_receipt))
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
    /// The opaque receipt for the deposited evidence — the ONLY thing the caller
    /// says about the reply. No binding fields: the verifier reads the sender
    /// from the receipt's row, not from anything the caller claims.
    receipt: String,
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
        // Created is 201; a redelivered BOOK that reused an existing challenge is
        // 200 — the same id and code, not a fresh prompt.
        Ok((created, id, preview)) => mapping::json_response(
            if created {
                StatusCode::CREATED
            } else {
                StatusCode::OK
            },
            &serde_json::json!({ "challenge": id, "preview": preview }),
        ),
        Err(problem) => mapping::plain_error(StatusCode::SERVICE_UNAVAILABLE, &problem),
    }
}

/// The trusted ingress deposits an inbound reply's evidence and gets a receipt.
///
/// This is the seam that makes the receipt honest: the evidence row is written
/// HERE, in the server that owns the store, and the untrusted orchestrator only
/// ever forwards the receipt this returns. It authenticates exactly as the other
/// endpoints do — a compromised process holding a workload token can reach it,
/// which is the SMS-demo assurance level ADR-026 concedes; the MODEL seat, which
/// holds no token and no client, cannot.
async fn deposit_evidence(
    State(state): State<ApprovalState>,
    headers: HeaderMap,
    axum::Json(body): axum::Json<InboundEvidence>,
) -> Response {
    if let Err(refused) = crate::authenticated(state.authority.as_ref(), &headers) {
        return refused;
    }
    match state.issuer.deposit_evidence(&body).await {
        Ok((challenge, receipt)) => mapping::json_response(
            StatusCode::CREATED,
            &serde_json::json!({ "challenge": challenge, "receipt": receipt }),
        ),
        Err(denied) => denial_response(&denied),
    }
}

async fn reply_to_approval(
    State(state): State<ApprovalState>,
    headers: HeaderMap,
    Path(id): Path<String>,
    axum::Json(body): axum::Json<ReplyBody>,
) -> Response {
    // The authenticated caller is KEPT (M7B fixed the discard): it is the actor a
    // replayed `YES` must match to recover its reference, never the grant's actor.
    let actor = match crate::authenticated(state.authority.as_ref(), &headers) {
        Ok(actor) => actor,
        Err(refused) => return refused,
    };

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

    match state
        .issuer
        .reply(&id, &body.code, &body.receipt, &actor, approve)
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
    let actor = match crate::authenticated(state.authority.as_ref(), &headers) {
        Ok(actor) => actor,
        Err(refused) => return refused,
    };

    // Only the actor a delegation NAMES may revoke it.
    //
    // Without this, any workload the resolver knows could revoke any
    // delegation whose id it could guess or had seen — no widening, but an
    // authorization-free way to break other people's bookings. Found in review.
    //
    // "Not yours" and "no such delegation" answer identically, for the reason
    // ADR-022 recorded about 404-not-403: distinguishing them would tell a
    // caller which references exist.
    if state.authority.resolve_delegation(&id, &actor).is_none() {
        return mapping::plain_error(
            StatusCode::UNAUTHORIZED,
            "no live delegation for that reference",
        );
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

/// A texted REVOKE: stop EVERY live grant the sender authorized.
///
/// # Why this has no per-grant actor gate, unlike `revoke_delegation`
///
/// The single-revoke route above gates on `resolve_delegation(actor)` — only the
/// workload a delegation names may revoke it. This route deliberately does NOT.
/// A REVOKE names no delegation; its authority is the transport-set control
/// inbound itself, which only the trusted ingress can write and only for a sender
/// that resolves to a live binding. That resolution — sender → binding →
/// principal → sweep by grantor — IS the authorization, checked in the service.
/// The bearer authentication here is the same compromised-workload assurance the
/// deposit endpoint concedes (ADR-026); the anti-forgery property is that the
/// identity triple is transport-set, never caller-chosen.
async fn revoke_via_receipt(
    State(state): State<ApprovalState>,
    headers: HeaderMap,
    axum::Json(body): axum::Json<InboundEvidence>,
) -> Response {
    if let Err(refused) = crate::authenticated(state.authority.as_ref(), &headers) {
        return refused;
    }
    match state.issuer.revoke_via_receipt(&body).await {
        // Idempotent by contract: a re-sent REVOKE returns the count still
        // stopped (0 once everything is), never a 410. `revoked` is the count,
        // not the single-route's bool — a distinct route, a distinct shape.
        Ok(count) => {
            mapping::json_response(StatusCode::OK, &serde_json::json!({ "revoked": count }))
        }
        Err(denied) => denial_response(&denied),
    }
}

/// Each denial keeps its own status, because they are different facts.
fn denial_response(denied: &ApprovalDenied) -> Response {
    let status = match denied {
        // 404 rather than 400: a caller who guesses challenge ids — or receipt
        // ids — learns nothing about which exist (ADR-022's reasoning, one
        // layer up).
        ApprovalDenied::UnknownChallenge | ApprovalDenied::UnknownReceipt => StatusCode::NOT_FOUND,
        // 410 Gone: it existed, its moment has passed, and retrying the same
        // thing cannot help — which is exactly what Gone means.
        ApprovalDenied::ChallengeExpired | ApprovalDenied::Replay(_) => StatusCode::GONE,
        // 403: the answer was heard and refused — including a receipt that
        // answers a different challenge than the one named.
        ApprovalDenied::WrongCode { .. }
        | ApprovalDenied::AttemptsExceeded
        | ApprovalDenied::WrongChannel
        | ApprovalDenied::ReceiptChallengeMismatch => StatusCode::FORBIDDEN,
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
