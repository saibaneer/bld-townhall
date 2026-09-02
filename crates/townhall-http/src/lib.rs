#![forbid(unsafe_code)]

//! The boundary's wire: Axum handlers over [`BookingFacade`], and nothing else.
//!
//! This crate is the M5 gate's third clause made structural — *"handlers do
//! not mutate directly"* is a fact about what this code CAN express, because
//! its whole world is two object-safe traits from `townhall-service`
//! ([`BookingFacade`], [`ReconcilerHandle`]) plus the domain's vocabulary. No
//! store type, no provider client, no SQL is nameable here: the Cargo manifest
//! is the enforcement (ADR-021), and a source-scan test in the server remains
//! as a secondary tripwire.
//!
//! # The mapping is one function
//!
//! Every HTTP status is derived from a typed outcome in [`mapping`], whose
//! matches are exhaustive — a new outcome or error variant fails to compile
//! until someone classifies it. Handlers assemble requests and bodies; they
//! never decide a status.

use std::sync::Arc;
use std::sync::atomic::{AtomicU64, Ordering};

use axum::{
    Router,
    extract::{Path, Query, State},
    http::{HeaderMap, StatusCode, header},
    response::{IntoResponse, Response},
    routing::{get, post},
};
use bld_types::{BookingId, BookingRequirements, Money, SlotId, TimeWindow, VenueId};
use townhall_domain::{BookingProposal, VerifiedAuthority};
use townhall_service::{
    BookingFacade, LookupQuery, Mutated, Projection, ReconcilerHandle, VenueFilters,
};

pub mod mapping;

/// Resolves a bearer token to a verified authority over a NAMED RESOURCE — or
/// nothing.
///
/// # Why the booking is a parameter
///
/// It was not, through M5 and M6: a bearer resolved to a capability
/// (`may_book`), which held for any booking its holder could name. ADR-025
/// replaced capabilities with grants, and a grant names its resource — so
/// "resolve this bearer" is no longer a whole question. The answer depends on
/// which booking is being acted upon, and a resolver that could not see it
/// would have to return something permissive.
///
/// `None` is the resource-scoped refusal: this bearer holds no live grant over
/// THIS booking. Whether the bearer is unknown or merely unauthorized here is
/// deliberately not distinguished at this seam — the caller maps both to 401,
/// and the finer story belongs to the audit rather than to the wire (the same
/// 404-not-403 reasoning ADR-022 recorded for ownership).
pub trait AuthorityResolver: Send + Sync {
    fn resolve(&self, bearer: &str, booking: &BookingId) -> Option<VerifiedAuthority>;

    /// Who is calling, for the routes that name no booking.
    ///
    /// # Why this returns an identity and not a grant
    ///
    /// The first version returned a `VerifiedAuthority` and promised it would
    /// "name no resources". It could not keep that promise: a grant's resource
    /// list comes from an approved scope, which always names one booking, so
    /// the reader grant ended up naming a synthetic id — authority over an
    /// imaginary booking, which is still authority. The assertion guarding it
    /// was worse: written over an `Option` that was `None` by construction, it
    /// was vacuously true.
    ///
    /// So the seam says what it means. Listing your own bookings needs to know
    /// WHO you are; touching one needs a grant. A caller on this path receives
    /// something that cannot authorize anything, because it is not the kind of
    /// thing authorization is made of.
    fn resolve_reader(&self, bearer: &str) -> Option<bld_types::PrincipalId>;
}

/// Everything a handler can reach. The completeness of this struct is the
/// completeness of the wire's power.
#[derive(Clone)]
pub struct ServerState {
    pub api: Arc<dyn BookingFacade>,
    pub authority: Arc<dyn AuthorityResolver>,
}

/// The router over one state. The composition root binds it to a listener.
pub fn router(state: ServerState) -> Router {
    Router::new()
        .route(
            "/booking-intents",
            get(lookup_bookings).post(create_booking),
        )
        .route("/booking-intents/{id}", get(read_booking))
        .route("/booking-intents/{id}/audit", get(read_audit))
        .route("/venues", get(browse_venues))
        .route("/venues/{venue}/slots/{slot}", get(read_slot))
        .route(
            "/booking-intents/{id}/behaviours/{behaviour}",
            post(propose_behaviour),
        )
        .layer(axum::middleware::from_fn(request_id))
        .with_state(state)
}

/// The reconciler loop: `due` then `attend`, every `interval`, until the
/// shutdown watch flips. The store is the queue, the cadence is the retry,
/// escalation is the dead-letter — the loop only supplies the heartbeat.
pub async fn run_reconciler(
    reconciler: Arc<dyn ReconcilerHandle>,
    interval: std::time::Duration,
    mut shutdown: tokio::sync::watch::Receiver<bool>,
) {
    let mut ticker = tokio::time::interval(interval);
    ticker.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);
    loop {
        tokio::select! {
            _ = ticker.tick() => {
                let Ok(due) = reconciler.due(16).await else { continue };
                for effect in due {
                    // An error on one identity must not starve the rest; the
                    // row stays due and the next tick returns to it.
                    let _ = reconciler.attend(&effect).await;
                }
            }
            changed = shutdown.changed() => {
                if changed.is_err() || *shutdown.borrow() {
                    return;
                }
            }
        }
    }
}

// ------------------------------------------------------------------ headers

static REQUEST_COUNTER: AtomicU64 = AtomicU64::new(0);

/// Echo `X-Request-ID`, or mint one — one transport attempt, one id.
async fn request_id(request: axum::extract::Request, next: axum::middleware::Next) -> Response {
    let incoming = request
        .headers()
        .get("x-request-id")
        .and_then(|value| value.to_str().ok())
        .map(str::to_owned);
    let id = incoming.unwrap_or_else(|| {
        format!(
            "req-{}-{}",
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .map_or(0, |since| since.as_millis()),
            REQUEST_COUNTER.fetch_add(1, Ordering::Relaxed)
        )
    });
    let mut response = next.run(request).await;
    if let Ok(value) = header::HeaderValue::from_str(&id) {
        response.headers_mut().insert("x-request-id", value);
    }
    response
}

/// The auth gate every route passes: a resolvable bearer, and no reserved
/// headers. `X-BLD-Delegation` is refused loudly until M7's envelope exists
/// (ADR-021) — silently ignoring a header that claims authority would be
/// worse than refusing it.
// The large-Err lint is silenced deliberately on the three header gates: the
// Err IS a finished HTTP response, produced at most once per request, and
// boxing it would trade clarity at every call site for bytes nobody counts.
#[allow(clippy::result_large_err)]
fn authorize(
    state: &ServerState,
    headers: &HeaderMap,
    booking: &BookingId,
) -> Result<VerifiedAuthority, Response> {
    authorized(headers, |bearer| state.authority.resolve(bearer, booking))
}

/// The reader gate, for routes that name no booking.
#[allow(clippy::result_large_err)]
fn authorize_reader(
    state: &ServerState,
    headers: &HeaderMap,
) -> Result<bld_types::PrincipalId, Response> {
    authorized(headers, |bearer| state.authority.resolve_reader(bearer))
}

/// The header checks both gates share, so neither can forget one.
#[allow(clippy::result_large_err)]
fn authorized<T>(
    headers: &HeaderMap,
    resolve: impl FnOnce(&str) -> Option<T>,
) -> Result<T, Response> {
    if headers.contains_key("x-bld-delegation") {
        return Err(mapping::plain_error(
            StatusCode::BAD_REQUEST,
            "delegation envelopes arrive with M7; this header is reserved",
        ));
    }
    let bearer = headers
        .get(header::AUTHORIZATION)
        .and_then(|value| value.to_str().ok())
        .and_then(|value| value.strip_prefix("Bearer "));
    let Some(token) = bearer else {
        return Err(mapping::plain_error(
            StatusCode::UNAUTHORIZED,
            "no verified caller identity",
        ));
    };
    resolve(token).ok_or_else(|| {
        mapping::plain_error(StatusCode::UNAUTHORIZED, "no verified caller identity")
    })
}

/// What an `If-Match` header amounted to, under spec §9.2's contract: exactly
/// one strong, quoted (or bare) numeric version. Wildcards would make
/// staleness unexpressible; weak validators cannot guard a CAS; multiple
/// values are ambiguous about which precondition the caller means.
#[allow(clippy::result_large_err)]
fn expected_version(headers: &HeaderMap) -> Result<u64, Response> {
    let mut values = headers.get_all(header::IF_MATCH).iter();
    let (Some(value), None) = (values.next(), values.next()) else {
        return Err(match headers.get_all(header::IF_MATCH).iter().count() {
            0 => mapping::plain_error(
                StatusCode::PRECONDITION_REQUIRED,
                "mutations require If-Match with the version last observed",
            ),
            _ => mapping::plain_error(
                StatusCode::BAD_REQUEST,
                "exactly one If-Match value is meaningful here",
            ),
        });
    };
    let Ok(text) = value.to_str() else {
        return Err(mapping::plain_error(
            StatusCode::BAD_REQUEST,
            "If-Match must be a version ETag",
        ));
    };
    let candidate = text.trim();
    if candidate == "*" || candidate.starts_with("W/") {
        return Err(mapping::plain_error(
            StatusCode::BAD_REQUEST,
            "If-Match must be one strong version ETag; wildcards and weak validators cannot guard a compare-and-set",
        ));
    }
    candidate.trim_matches('"').parse::<u64>().map_err(|_| {
        mapping::plain_error(StatusCode::BAD_REQUEST, "If-Match must be a version ETag")
    })
}

/// Routes where NO precondition applies refuse a present `If-Match` rather
/// than ignore it (ADR-021): a precondition the server would not honour is a
/// caller's false belief, and 400 says so.
#[allow(clippy::result_large_err)]
fn refuse_precondition(headers: &HeaderMap, route: &str) -> Result<(), Response> {
    if headers.contains_key(header::IF_MATCH) {
        return Err(mapping::plain_error(
            StatusCode::BAD_REQUEST,
            &format!("no precondition applies to {route}"),
        ));
    }
    Ok(())
}

// ------------------------------------------------------------------- bodies

#[derive(serde::Deserialize)]
struct CreateBody {
    id: String,
    purpose: String,
    requested_date: String,
    from: String,
    to: String,
    attendees: u16,
    wheelchair_accessible: bool,
    max_fee_pence: u64,
}

#[derive(serde::Deserialize)]
struct SelectVenueBody {
    venue_id: String,
    slot_id: String,
}

#[derive(serde::Deserialize, Default)]
struct UpdateRequirementsBody {
    attendees: Option<u16>,
}

#[derive(serde::Deserialize)]
struct CancelBody {
    reason: String,
}

#[derive(serde::Deserialize, Default)]
struct VenueQuery {
    attendees: Option<u16>,
    accessible: Option<bool>,
    max_fee_pence: Option<i64>,
}

// ----------------------------------------------------------------- handlers

/// The two supported collection queries, and nothing else.
///
/// # Where each 400 comes from, because they are two different mechanisms
///
/// Axum rejects a *malformed* value itself — `?cancellable=maybe` never reaches
/// this function. What it accepts perfectly happily is no filter, both filters,
/// and `cancellable=false`: all three are well-formed `Option`s. So those are
/// refused here, by hand, because they are unrepresentable in [`LookupQuery`]
/// rather than merely empty results.
///
/// An unfiltered "every booking you own" listing is not a surface this milestone
/// offers, and refusing it in the type is cheaper than remembering to.
#[derive(serde::Deserialize, Default)]
struct LookupParams {
    booking_ref: Option<String>,
    cancellable: Option<bool>,
}

async fn lookup_bookings(
    State(state): State<ServerState>,
    headers: HeaderMap,
    Query(params): Query<LookupParams>,
) -> Response {
    let authority = match authorize_reader(&state, &headers) {
        Ok(authority) => authority,
        Err(refused) => return refused,
    };

    let query = match (params.booking_ref, params.cancellable) {
        (Some(_), Some(_)) => {
            return mapping::plain_error(
                StatusCode::BAD_REQUEST,
                "booking_ref and cancellable are alternatives, not a conjunction",
            );
        }
        (Some(reference), None) => {
            LookupQuery::ByBookingRef(bld_types::CouncilBookingRef::new(reference))
        }
        (None, Some(true)) => LookupQuery::Cancellable,
        (None, Some(false)) => {
            return mapping::plain_error(
                StatusCode::BAD_REQUEST,
                "cancellable=false has no meaning here; omit the filter or ask for true",
            );
        }
        (None, None) => {
            return mapping::plain_error(
                StatusCode::BAD_REQUEST,
                "one of booking_ref or cancellable=true is required",
            );
        }
    };

    match state.api.lookup(&query, &authority).await {
        // No collection ETag: a list has no single version, and emitting one
        // would invite a caller to use it as a precondition for a mutation on
        // one of its members. Each projection still carries its own.
        Ok(rows) => (
            StatusCode::OK,
            axum::Json(serde_json::json!({
                "bookings": rows.iter().map(mapping::projection_body).collect::<Vec<_>>(),
            })),
        )
            .into_response(),
        Err(error) => mapping::api_error(&error),
    }
}

async fn create_booking(
    State(state): State<ServerState>,
    headers: HeaderMap,
    axum::Json(body): axum::Json<CreateBody>,
) -> Response {
    // The resolved authority is KEPT now, not discarded. It names the owner the
    // new row records, which is the whole of M5.1: before this, `authorize` was
    // a turnstile that proved someone was a caller and then threw away which
    // caller they were.
    //
    // The id comes off the BODY here rather than the path, and it is the
    // resource the grant must name — a create authorized for some other
    // booking is not an authorization for this one.
    let id = BookingId::new(body.id);
    let authority = match authorize(&state, &headers, &id) {
        Ok(authority) => authority,
        Err(refused) => return refused,
    };
    if let Err(refused) = refuse_precondition(&headers, "create") {
        return refused;
    }
    let requirements = BookingRequirements {
        purpose: body.purpose,
        requested_date: body.requested_date,
        time_window: TimeWindow {
            from: body.from,
            to: body.to,
        },
        attendees: body.attendees,
        wheelchair_accessible: body.wheelchair_accessible,
        max_fee: Money::from_pence(body.max_fee_pence),
    };
    match state.api.create(id, requirements, &authority).await {
        Ok(projection) => mapping::projection_response(StatusCode::CREATED, &projection),
        Err(error) => mapping::api_error(&error),
    }
}

async fn read_booking(
    State(state): State<ServerState>,
    headers: HeaderMap,
    Path(id): Path<String>,
) -> Response {
    let id = BookingId::new(id);
    let authority = match authorize(&state, &headers, &id) {
        Ok(authority) => authority,
        Err(refused) => return refused,
    };
    match state.api.read(&id, &authority).await {
        Ok(projection) => mapping::projection_response(StatusCode::OK, &projection),
        Err(error) => mapping::api_error(&error),
    }
}

async fn read_audit(
    State(state): State<ServerState>,
    headers: HeaderMap,
    Path(id): Path<String>,
) -> Response {
    let id = BookingId::new(id);
    let authority = match authorize(&state, &headers, &id) {
        Ok(authority) => authority,
        Err(refused) => return refused,
    };
    match state.api.audit(&id, &authority).await {
        Ok(entries) => {
            let rows: Vec<serde_json::Value> = entries
                .iter()
                .map(|entry| {
                    serde_json::json!({
                        "driver_kind": entry.driver_kind,
                        "driver_detail": entry.driver_detail,
                        "outcome": entry.outcome,
                        "from_version": entry.from_version,
                        "to_version": entry.to_version,
                        "at_ms": entry.at_ms,
                    })
                })
                .collect();
            (
                StatusCode::OK,
                axum::Json(serde_json::json!({ "audit": rows })),
            )
                .into_response()
        }
        Err(error) => mapping::api_error(&error),
    }
}

async fn browse_venues(
    State(state): State<ServerState>,
    headers: HeaderMap,
    Query(query): Query<VenueQuery>,
) -> Response {
    if let Err(refused) = authorize_reader(&state, &headers) {
        return refused;
    }
    let filters = VenueFilters {
        attendees: query.attendees,
        accessible: query.accessible,
        max_fee_pence: query.max_fee_pence,
    };
    match state.api.venues(filters).await {
        Ok(rows) => {
            let venues: Vec<serde_json::Value> = rows
                .iter()
                .map(|row| {
                    serde_json::json!({
                        "venue_id": row.venue_id,
                        "slot_id": row.slot_id,
                        "fee_pence": row.fee_pence,
                        "capacity": row.capacity,
                        "accessible": row.accessible,
                        "available": row.available,
                    })
                })
                .collect();
            (
                StatusCode::OK,
                axum::Json(serde_json::json!({ "venues": venues, "browse_only": true })),
            )
                .into_response()
        }
        Err(error) => mapping::api_error(&error),
    }
}

async fn read_slot(
    State(state): State<ServerState>,
    headers: HeaderMap,
    Path((venue, slot)): Path<(String, String)>,
) -> Response {
    if let Err(refused) = authorize_reader(&state, &headers) {
        return refused;
    }
    match state
        .api
        .slot_facts(&VenueId::new(venue), &SlotId::new(slot))
        .await
    {
        Ok(Some(facts)) => (
            StatusCode::OK,
            axum::Json(serde_json::json!({
                "venue_id": facts.venue_id,
                "slot_id": facts.slot_id,
                "capacity": facts.capacity,
                "accessible": facts.accessible,
                "fee_pence": facts.fee_pence,
                "available": facts.available,
            })),
        )
            .into_response(),
        Ok(None) => mapping::plain_error(StatusCode::NOT_FOUND, "no such venue or slot"),
        Err(error) => mapping::api_error(&error),
    }
}

async fn propose_behaviour(
    State(state): State<ServerState>,
    headers: HeaderMap,
    Path((id, behaviour)): Path<(String, String)>,
    body: Option<axum::Json<serde_json::Value>>,
) -> Response {
    let id = BookingId::new(id);
    let authority = match authorize(&state, &headers, &id) {
        Ok(authority) => authority,
        Err(refused) => return refused,
    };
    let payload = body.map_or(serde_json::Value::Null, |axum::Json(value)| value);

    // Admission FIRST — before either header gate, and before the behaviour
    // name is even looked at.
    //
    // The header gates below answer 400 for a malformed `If-Match` and 428 for a
    // missing one. Both are statements about a resource, so a caller who cannot
    // see this booking would learn it exists by being told its precondition was
    // wrong. Whether the caller is entitled to an answer has to be settled
    // before any answer is composed.
    //
    // Side-effect-free: one scoped load, nothing written, nothing chased.
    if let Err(refused) = state.api.ensure_visible(&id, &authority).await {
        return mapping::api_error(&refused);
    }

    // The reconcile trigger: exempt from preconditions by classification
    // (ADR-021) — it asserts no expected state, so a version tag would be a
    // false belief, refused like any other. That exemption covers preconditions
    // only; the visibility check above still applies, or this route would be an
    // authenticated existence oracle.
    if behaviour == "reconcile" {
        if let Err(refused) = refuse_precondition(&headers, "reconcile") {
            return refused;
        }
        return match state.api.attend_booking(&id, &authority).await {
            Ok(outcomes) => (
                StatusCode::OK,
                axum::Json(serde_json::json!({
                    "attended": outcomes
                        .iter()
                        .map(|outcome| format!("{outcome:?}"))
                        .collect::<Vec<_>>(),
                })),
            )
                .into_response(),
            Err(error) => mapping::api_error(&error),
        };
    }

    let expected = match expected_version(&headers) {
        Ok(version) => version,
        Err(refused) => return refused,
    };
    let proposal = match parse_proposal(&behaviour, payload) {
        Ok(proposal) => proposal,
        Err(refused) => return refused,
    };
    match state
        .api
        .propose_at(&id, expected, proposal, &authority)
        .await
    {
        Ok(mutated) => turn_response(&state, &id, mutated, &authority).await,
        Err(error) => mapping::api_error(&error),
    }
}

#[allow(clippy::result_large_err)]
fn parse_proposal(
    behaviour: &str,
    payload: serde_json::Value,
) -> Result<BookingProposal, Response> {
    let bad_body = |detail: &str| mapping::plain_error(StatusCode::UNPROCESSABLE_ENTITY, detail);
    match behaviour {
        "select-venue" => {
            let body: SelectVenueBody = serde_json::from_value(payload)
                .map_err(|_| bad_body("select-venue needs venue_id and slot_id"))?;
            Ok(BookingProposal::SelectVenue {
                venue_id: VenueId::new(body.venue_id),
                slot_id: SlotId::new(body.slot_id),
            })
        }
        "verify-slot" => Ok(BookingProposal::VerifySlot),
        "change-venue" => Ok(BookingProposal::ChangeVenue),
        "update-requirements" => {
            let body: UpdateRequirementsBody = serde_json::from_value(payload)
                .map_err(|_| bad_body("update-requirements carries optional attendees"))?;
            Ok(BookingProposal::UpdateRequirements {
                attendees: body.attendees,
            })
        }
        "revalidate-venue" => Ok(BookingProposal::RevalidateVenue),
        // Deliberately empty-bodied: parameters are boundary-derived (spec §10).
        "book" => Ok(BookingProposal::Book),
        "cancel" => {
            let body: CancelBody =
                serde_json::from_value(payload).map_err(|_| bad_body("cancel needs a reason"))?;
            Ok(BookingProposal::Cancel {
                reason: body.reason,
            })
        }
        _ => Err(mapping::plain_error(
            StatusCode::NOT_FOUND,
            "no such behaviour route",
        )),
    }
}

/// A turn's response body wants the current projection for the states the
/// outcome does not carry — assembled here, mapped in [`mapping`].
async fn turn_response(
    state: &ServerState,
    id: &BookingId,
    mutated: Mutated,
    authority: &VerifiedAuthority,
) -> Response {
    // Scoped like every other external read. The caller has already been
    // admitted to reach here, so this cannot fail for visibility — but reading
    // unscoped would leave a path that does not care who is asking, and those
    // are exactly the paths that get reused later by something that should.
    let menu: Option<Projection> = state.api.read(id, authority).await.ok();
    mapping::turn(&mutated, menu.as_ref())
}
