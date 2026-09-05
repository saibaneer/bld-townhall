//! The trusted usage-metering endpoints (M8, ADR-027).
//!
//! # Why the dispatcher reaches these over HTTP
//!
//! The metered step is the proposer turn, which runs in the sms-simulator
//! process — no database, no pool. The usage ledger is a TRUSTED component, so it
//! lives here, behind the boundary, and the dispatcher consults it over these
//! endpoints exactly as it consults `/approvals`.
//!
//! # Why the caller names nothing load-bearing
//!
//! Each request carries only the inbound's transport evidence (the same triple
//! `/inbound-evidence` and `/revocations` take). The SERVER derives the three
//! things that matter — the principal (by resolving the sender to a live
//! binding), the intent id (from the triple) and the unit cost (from its own
//! `PricingSchedule`). So a compromised dispatcher can meter only turns it can
//! present transport evidence for, and cannot name a victim's account or inflate
//! a debit. A unit is £0 and grants no authority.

use crate::approvals::InboundEvidence;
use crate::mapping;
use axum::{
    Router,
    extract::State,
    http::{HeaderMap, StatusCode},
    response::Response,
    routing::post,
};
use std::sync::Arc;

/// What a balance read returns, in units.
pub struct UsageBalanceView {
    pub remaining: i64,
    pub limit: i64,
}

/// Why a metering call was refused. Every resource denial is HTTP 429; they are
/// told apart by the `denial_code` in the body, not the status (ADR-028).
pub enum UsageMeterError {
    /// The account is out of total quota (M8-1).
    QuotaExhausted,
    /// This principal hit its per-window rate (M8-2).
    PrincipalRateLimited,
    /// This channel hit its per-window rate (M8-2).
    ChannelRateLimited,
    /// The global per-window provider ceiling is spent (M8-2).
    ProviderBudgetExhausted,
    /// The store could not be reached — a 503.
    Unavailable(String),
}

impl UsageMeterError {
    /// The stable body code a client reads to tell the four 429 denials apart.
    #[must_use]
    pub const fn denial_code(&self) -> &'static str {
        match self {
            Self::QuotaExhausted => "quota_exhausted",
            Self::PrincipalRateLimited => "rate_limited_principal",
            Self::ChannelRateLimited => "rate_limited_channel",
            Self::ProviderBudgetExhausted => "provider_budget_exhausted",
            Self::Unavailable(_) => "unavailable",
        }
    }
}

/// What the usage endpoints need, without naming a concrete meter or store. The
/// implementation (in the server) resolves the sender to a binding, derives the
/// intent from the triple, and prices the turn — none of which the caller supplies.
#[async_trait::async_trait]
pub trait UsageMeter: Send + Sync {
    /// Reserve the metered turn's units — the quota gate.
    async fn reserve(&self, inbound: &InboundEvidence) -> Result<(), UsageMeterError>;
    /// Settle the turn — the meter-once op. Best-effort/idempotent.
    async fn debit(&self, inbound: &InboundEvidence);
    /// Rescind the reservation — failure before consumption, or a zero-unit turn.
    async fn release(&self, inbound: &InboundEvidence);
    /// The account's balance, in units — a zero-unit read.
    async fn balance(&self, inbound: &InboundEvidence)
    -> Result<UsageBalanceView, UsageMeterError>;
}

/// Everything the usage router can reach.
#[derive(Clone)]
pub struct UsageState {
    pub meter: Arc<dyn UsageMeter>,
    /// Reused so these endpoints authenticate exactly as the booking API and the
    /// approval endpoints do — one notion of "which workload is calling".
    pub authority: Arc<dyn crate::AuthorityResolver>,
}

/// The routes. All POST, because each carries the transport-evidence body — even
/// the balance read, whose input is the evidence, not a path parameter.
pub fn usage_router(state: UsageState) -> Router {
    Router::new()
        .route("/usage/reserve", post(reserve))
        .route("/usage/debit", post(debit))
        .route("/usage/release", post(release))
        .route("/usage/balance", post(balance))
        .with_state(state)
}

async fn reserve(
    State(state): State<UsageState>,
    headers: HeaderMap,
    axum::Json(body): axum::Json<InboundEvidence>,
) -> Response {
    if let Err(refused) = crate::authenticated(state.authority.as_ref(), &headers) {
        return refused;
    }
    match state.meter.reserve(&body).await {
        Ok(()) => mapping::json_response(StatusCode::OK, &serde_json::json!({ "reserved": true })),
        Err(UsageMeterError::Unavailable(why)) => {
            mapping::plain_error(StatusCode::SERVICE_UNAVAILABLE, &why)
        }
        // Every resource denial is 429, distinguished by the STRUCTURED body's
        // `denial_code` — status alone cannot tell quota from a rate limit from
        // the provider budget (ADR-028). The client reads the code; a client that
        // reads only the status still sees "refused for a resource reason".
        Err(denied) => mapping::json_response(
            StatusCode::TOO_MANY_REQUESTS,
            &serde_json::json!({ "denial_code": denied.denial_code() }),
        ),
    }
}

async fn debit(
    State(state): State<UsageState>,
    headers: HeaderMap,
    axum::Json(body): axum::Json<InboundEvidence>,
) -> Response {
    if let Err(refused) = crate::authenticated(state.authority.as_ref(), &headers) {
        return refused;
    }
    // Best-effort and idempotent — the meter-once guard is in the store, so a
    // replay settles nothing more. Always 200.
    state.meter.debit(&body).await;
    mapping::json_response(StatusCode::OK, &serde_json::json!({ "settled": true }))
}

async fn release(
    State(state): State<UsageState>,
    headers: HeaderMap,
    axum::Json(body): axum::Json<InboundEvidence>,
) -> Response {
    if let Err(refused) = crate::authenticated(state.authority.as_ref(), &headers) {
        return refused;
    }
    state.meter.release(&body).await;
    mapping::json_response(StatusCode::OK, &serde_json::json!({ "released": true }))
}

async fn balance(
    State(state): State<UsageState>,
    headers: HeaderMap,
    axum::Json(body): axum::Json<InboundEvidence>,
) -> Response {
    if let Err(refused) = crate::authenticated(state.authority.as_ref(), &headers) {
        return refused;
    }
    match state.meter.balance(&body).await {
        Ok(view) => mapping::json_response(
            StatusCode::OK,
            &serde_json::json!({ "remaining": view.remaining, "limit": view.limit }),
        ),
        Err(UsageMeterError::Unavailable(why)) => {
            mapping::plain_error(StatusCode::SERVICE_UNAVAILABLE, &why)
        }
        // Balance never exhausts — it is a read that charges nothing — but map any
        // resource denial defensively rather than leak a 429 from a read.
        Err(_) => mapping::plain_error(StatusCode::SERVICE_UNAVAILABLE, "usage read unavailable"),
    }
}
