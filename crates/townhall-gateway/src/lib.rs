#![forbid(unsafe_code)]

//! The town hall's wire, as a client — an untrusted driver.
//!
//! Spec §3.2 grades a BLD client an *"untrusted driver; must not bypass server
//! checks"*, and marks it as unable to mutate authoritative booking state. That
//! is enforced here by the crate graph: this crate cannot name
//! `townhall-service`, `townhall-store`, `townhall-http`, `townhall-domain`,
//! `bld-kernel` or `sqlx`. Its only route to a booking is a socket, so "the
//! channel cannot bypass the boundary" is a fact about the process tree rather
//! than a promise.
//!
//! # Narrow on purpose
//!
//! Every route below is hard-coded. That is not laziness — it is the *pre*-M9
//! state, and M9's gate is precisely *"generic client discovers service and
//! drives API without hard-coded behaviour URLs beyond bootstrap"*. A client
//! that hard-codes them is what M9 replaces; calling this the generic client
//! would have claimed M9's deliverable inside M6.
//!
//! # Its own DTOs, deliberately
//!
//! These structs are written independently of `townhall-http`'s. Two struct sets
//! that must agree over a socket is a real test of the wire contract; one shared
//! set makes the contract vacuously true, because a field renamed on both sides
//! at once breaks nothing and proves nothing.

use bld_types::{BookingId, BookingRequirements, CouncilBookingRef};
use std::time::Duration;
use thiserror::Error;

pub mod dto;
pub use dto::{AuditRow, Projection, SlotFacts, VenueRow};

/// What one turn amounted to.
///
/// # Why 202 is a first-class variant and not an error
///
/// An earlier plan revision claimed 202 was the *normal* result of `book` and
/// `cancel`. It is not: an answering council settles synchronously and the turn
/// returns 200. 202 means the answer went missing and the chase now owns the
/// outcome — a real state, not a failure, and the caller has something true to
/// say to a human either way.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Turn {
    /// The transition committed. `state` is where it landed.
    Committed { state: String, version: u64 },
    /// The chase caught up with an effect that was already settled.
    Converged { state: Option<String> },
    /// A durable intent exists and its outcome is not yet knowable.
    ///
    /// The caller now has two things to say at two different times, which is why
    /// this returns rather than blocking: *"Booking now"* is a reply to a person
    /// who just texted, and the outcome is an unsolicited message later. A
    /// gateway that converged internally would collapse them into one and make
    /// the two-message shape unexpressible.
    Accepted { retry_after: Duration },
    /// The behaviour is not in this state's menu. Carries what is.
    NotAvailable { menu: Vec<String> },
    /// A guard refused. `reason` is the domain's own error name.
    Denied { reason: String },
}

/// Everything the wire can answer that is not a turn.
#[derive(Debug, Error)]
pub enum GatewayError {
    /// 404 — absent, **or** invisible. M5.1 made those deliberately the same
    /// answer: a 403 would confirm the resource exists, which is the oracle
    /// somebody guessing council references wants.
    #[error("no such booking")]
    UnknownBooking,
    /// 409 with a version: the caller owns this id and may retry with the `ETag`.
    #[error("already exists at version {current}")]
    Existing { current: u64 },
    /// 409 without one: the id is taken by someone else. Deliberately carries no
    /// version, state or owner — a different variant from `Existing`, because a
    /// caller that cannot tell them apart cannot tell "retry with this `ETag`"
    /// from "choose another id".
    #[error("that identifier is unavailable")]
    IdentifierUnavailable,
    #[error("stale precondition; current version is {current}")]
    Stale { current: u64 },
    /// 422 with no `ETag` and no error name — the request itself was malformed.
    /// The same status as a domain denial, an entirely different situation.
    #[error("the server could not parse the request: {0}")]
    Malformed(String),
    /// 429, after the bounded retry gave up.
    #[error("contention budget exhausted")]
    Contended,
    /// 503 carrying a domain denial: the provider could not be asked.
    #[error("the provider is silent: {0}")]
    ProviderSilent(String),
    /// 503 plain: the service itself cannot answer.
    #[error("service unavailable: {0}")]
    Unavailable(String),
    #[error("unauthenticated")]
    Unauthenticated,
    #[error("bad request: {0}")]
    BadRequest(String),
    #[error("a precondition is required for this route")]
    PreconditionRequired,
    #[error("internal server error: {0}")]
    Internal(String),
    #[error("transport failure: {0}")]
    Transport(String),
    /// The convergence loop ran out of attempts or deadline.
    #[error("the chase did not converge within {attempts} attempts")]
    NotConverged { attempts: u32 },
    #[error("the response did not match the wire contract: {0}")]
    Unrecognized(String),
}

/// How hard the gateway tries before giving up.
///
/// There is deliberately NO contention-retry knob. The wire's own 429 body says
/// "re-read and retry", and building this gateway proved why: a contended turn
/// may already have committed (the test that found it got `Stale {current: 3}`
/// back on its verbatim retry — the version had moved). A blind re-POST of the
/// same `If-Match` can therefore only answer 412 or 429 again; the one correct
/// follow-up is a fresh read, and fresh reads are the CALLER's discipline
/// (reload-before-propose), not something to bury in a client loop.
#[derive(Clone, Copy, Debug)]
pub struct RetryPolicy {
    /// Bounded, because an unbounded poll is a hang wearing a loop.
    pub max_convergence_polls: u32,
    pub convergence_deadline: Duration,
}

impl Default for RetryPolicy {
    fn default() -> Self {
        Self {
            max_convergence_polls: 8,
            convergence_deadline: Duration::from_secs(30),
        }
    }
}

/// The states an effect can still be in flight in.
///
/// Convergence means leaving this set. Listed here rather than asked of the
/// server because the gateway has no domain dependency — and a client that
/// decided for itself which states are terminal would drift from the domain
/// silently. The compensation is a test that the set is exactly the states whose
/// menus the wire reports as in-flight.
const IN_FLIGHT: &[&str] = &[
    "BookingInProgress",
    "CancellationRequested",
    "CancellingBooking",
];

pub struct Gateway {
    base: String,
    bearer: String,
    /// Whose bookings this client reads.
    ///
    /// Sent as `X-BLD-Principal`, and the server checks it against a live
    /// channel binding rather than believing it (M7B). A read is scoped to
    /// somebody; there is no unscoped read.
    principal: String,
    /// The grant reference this client presents when it CHANGES something.
    ///
    /// `None` until an approval has produced one, and that is a real state
    /// rather than an oversight: spec §23.1 puts approval before the first
    /// mutation, so a client that has not been approved yet genuinely holds no
    /// reference and every change it attempts is refused with 401.
    delegation: Option<String>,
    http: reqwest::Client,
    policy: RetryPolicy,
    /// Set when the caller wants request ids it chose rather than minted.
    request_id: Option<String>,
    /// The id the server answered with on the most recent call.
    ///
    /// Recorded because a correlation key nobody retains correlates nothing —
    /// the PR review found the gateway reading this header and throwing it
    /// away, which made the "recorded" claim in the contract a hope.
    last_request_id: std::sync::Mutex<Option<String>>,
}

impl Gateway {
    /// A client that can read `principal`'s bookings and change nothing.
    ///
    /// Changing requires [`Self::with_delegation`], because a change requires a
    /// grant — which is the whole of M7B's header split.
    #[must_use]
    pub fn new(
        base: impl Into<String>,
        bearer: impl Into<String>,
        principal: impl Into<String>,
    ) -> Self {
        Self {
            base: base.into(),
            bearer: bearer.into(),
            principal: principal.into(),
            delegation: None,
            http: reqwest::Client::new(),
            policy: RetryPolicy::default(),
            request_id: None,
            last_request_id: std::sync::Mutex::new(None),
        }
    }

    /// The `X-Request-ID` the server answered with most recently — the caller's
    /// own if one was configured (the middleware echoes it), the server's mint
    /// otherwise.
    #[must_use]
    pub fn last_request_id(&self) -> Option<String> {
        self.last_request_id
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .clone()
    }

    /// Present this grant reference on every subsequent change.
    ///
    /// Takes `self` rather than `&mut self` so a client that has been approved
    /// is a DIFFERENT VALUE from one that has not — an approved client cannot
    /// be un-approved by accident, and code holding the unapproved one cannot
    /// mutate however it is called.
    #[must_use]
    pub fn with_delegation(mut self, reference: impl Into<String>) -> Self {
        self.delegation = Some(reference.into());
        self
    }

    #[must_use]
    pub fn with_policy(mut self, policy: RetryPolicy) -> Self {
        self.policy = policy;
        self
    }

    /// Send this request id rather than letting the server mint one.
    ///
    /// The middleware echoes a supplied id verbatim, so a caller that wants its
    /// own correlation key can have it — and a caller that does not gets the
    /// server's, recorded either way.
    #[must_use]
    pub fn with_request_id(mut self, id: impl Into<String>) -> Self {
        self.request_id = Some(id.into());
        self
    }

    fn url(&self, path: &str) -> String {
        format!("{}{path}", self.base)
    }

    /// The three headers, applied in one place so no call site can omit one.
    ///
    /// `Authorization` says who is calling; `X-BLD-Principal` says whose
    /// bookings are in scope; `X-BLD-Delegation` says what may change, and is
    /// sent only when this client holds a grant. The server refuses a change
    /// without it, which is how an unapproved client fails loudly rather than
    /// quietly succeeding (spec §10.1).
    fn authorized(&self, builder: reqwest::RequestBuilder) -> reqwest::RequestBuilder {
        let builder = builder
            .header("authorization", format!("Bearer {}", self.bearer))
            .header("x-bld-principal", &self.principal);
        let builder = match &self.delegation {
            Some(reference) => builder.header("x-bld-delegation", reference),
            None => builder,
        };
        match &self.request_id {
            Some(id) => builder.header("x-request-id", id),
            None => builder,
        }
    }
}

impl Gateway {
    async fn call(
        &self,
        method: &str,
        path: &str,
        if_match: Option<u64>,
        body: Option<serde_json::Value>,
    ) -> Result<dto::RawResponse, GatewayError> {
        let url = self.url(path);
        let mut request = match method {
            "GET" => self.http.get(&url),
            "POST" => self.http.post(&url),
            other => return Err(GatewayError::Transport(format!("method {other}"))),
        };
        request = self.authorized(request);
        if let Some(version) = if_match {
            request = request.header("if-match", format!("\"{version}\""));
        }
        if let Some(json) = body {
            request = request.json(&json);
        }
        let response = request
            .send()
            .await
            .map_err(|error| GatewayError::Transport(error.to_string()))?;

        let status = response.status().as_u16();
        let header = |name: &str| {
            response
                .headers()
                .get(name)
                .and_then(|value| value.to_str().ok())
                .map(str::to_owned)
        };
        // The ETag is a quoted version, and it is the only place a converged
        // turn reports one at all.
        let etag = header("etag").and_then(|raw| raw.trim_matches('"').parse::<u64>().ok());
        let retry_after = header("retry-after").and_then(|raw| raw.parse::<u64>().ok());
        let request_id = header("x-request-id");
        let body = response.json().await.unwrap_or(serde_json::Value::Null);

        request_id.clone_into(
            &mut self
                .last_request_id
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner),
        );

        Ok(dto::RawResponse {
            status,
            etag,
            retry_after,
            request_id,
            body,
        })
    }

    fn classify_error(response: &dto::RawResponse) -> GatewayError {
        classify_error(response)
    }

    /// Create a booking intent. No precondition applies to a new resource.
    ///
    /// # Errors
    /// [`GatewayError::Existing`] when the caller already owns this id;
    /// [`GatewayError::IdentifierUnavailable`] when somebody else does.
    pub async fn create(
        &self,
        id: &BookingId,
        requirements: &BookingRequirements,
    ) -> Result<Projection, GatewayError> {
        let body = serde_json::json!({
            "id": id.as_str(),
            "purpose": requirements.purpose,
            "requested_date": requirements.requested_date,
            "from": requirements.time_window.from,
            "to": requirements.time_window.to,
            "attendees": requirements.attendees,
            "wheelchair_accessible": requirements.wheelchair_accessible,
            "max_fee_pence": requirements.max_fee.pence(),
        });
        let response = self
            .call("POST", "/booking-intents", None, Some(body))
            .await?;
        if response.status == 201 {
            return serde_json::from_value(response.body.clone())
                .map_err(|error| GatewayError::Unrecognized(error.to_string()));
        }
        Err(Self::classify_error(&response))
    }

    /// The authoritative projection.
    ///
    /// # Errors
    /// [`GatewayError::UnknownBooking`] for absent and invisible alike.
    pub async fn read(&self, id: &BookingId) -> Result<Projection, GatewayError> {
        let path = format!("/booking-intents/{}", id.as_str());
        let response = self.call("GET", &path, None, None).await?;
        if response.status == 200 {
            return serde_json::from_value(response.body.clone())
                .map_err(|error| GatewayError::Unrecognized(error.to_string()));
        }
        Err(Self::classify_error(&response))
    }

    /// The caller's bookings that currently offer `Cancel` (M5.1).
    ///
    /// # Errors
    /// As [`Self::read`].
    pub async fn cancellable(&self) -> Result<Vec<Projection>, GatewayError> {
        self.lookup("/booking-intents?cancellable=true").await
    }

    /// The caller's booking with this council reference (M5.1).
    ///
    /// A foreign reference answers an **empty list**, not a refusal — a 403 would
    /// confirm the reference exists.
    ///
    /// # Errors
    /// As [`Self::read`].
    pub async fn by_reference(
        &self,
        reference: &CouncilBookingRef,
    ) -> Result<Vec<Projection>, GatewayError> {
        self.lookup(&format!(
            "/booking-intents?booking_ref={}",
            reference.as_str()
        ))
        .await
    }

    /// The browsable catalogue — M6B's proposer needs candidates, and the gate
    /// journey's step 2 is `GET /venues`. Absent from the first build, which is
    /// why its 503 test had to bypass the gateway entirely.
    ///
    /// # Errors
    /// [`GatewayError::Unavailable`] when the catalogue cannot be asked.
    pub async fn venues(&self) -> Result<Vec<VenueRow>, GatewayError> {
        let response = self.call("GET", "/venues", None, None).await?;
        if response.status == 200 {
            let rows = response
                .body
                .get("venues")
                .cloned()
                .unwrap_or(serde_json::Value::Null);
            return serde_json::from_value(rows)
                .map_err(|error| GatewayError::Unrecognized(error.to_string()));
        }
        Err(Self::classify_error(&response))
    }

    /// One slot's verified facts, or `None` where the council answers "no such
    /// slot" — which is an answer, not an error.
    ///
    /// # Errors
    /// [`GatewayError::ProviderSilent`] when the provider cannot be asked.
    pub async fn slot(&self, venue: &str, slot: &str) -> Result<Option<SlotFacts>, GatewayError> {
        let path = format!("/venues/{venue}/slots/{slot}");
        let response = self.call("GET", &path, None, None).await?;
        match response.status {
            200 => serde_json::from_value(response.body.clone())
                .map(Some)
                .map_err(|error| GatewayError::Unrecognized(error.to_string())),
            404 => Ok(None),
            _ => Err(Self::classify_error(&response)),
        }
    }

    async fn lookup(&self, path: &str) -> Result<Vec<Projection>, GatewayError> {
        let response = self.call("GET", path, None, None).await?;
        if response.status == 200 {
            let rows = response
                .body
                .get("bookings")
                .cloned()
                .unwrap_or(serde_json::Value::Null);
            return serde_json::from_value(rows)
                .map_err(|error| GatewayError::Unrecognized(error.to_string()));
        }
        Err(Self::classify_error(&response))
    }

    /// The audit trail.
    ///
    /// # Errors
    /// As [`Self::read`].
    pub async fn audit(&self, id: &BookingId) -> Result<Vec<AuditRow>, GatewayError> {
        let path = format!("/booking-intents/{}/audit", id.as_str());
        let response = self.call("GET", &path, None, None).await?;
        if response.status == 200 {
            let rows = response
                .body
                .get("audit")
                .cloned()
                .unwrap_or(serde_json::Value::Null);
            return serde_json::from_value(rows)
                .map_err(|error| GatewayError::Unrecognized(error.to_string()));
        }
        Err(Self::classify_error(&response))
    }

    /// One versioned mutation, at a version the caller just read.
    ///
    /// Returns `Accepted` **without converging** — see [`Turn::Accepted`], and
    /// `Err(Contended)` immediately on a 429 — see [`RetryPolicy`] for why a
    /// client-side retry of a contended mutation is wrong on this wire.
    ///
    /// # Errors
    /// The full vocabulary of [`GatewayError`].
    pub async fn propose_at(
        &self,
        id: &BookingId,
        expected_version: u64,
        behaviour: &str,
        body: Option<serde_json::Value>,
    ) -> Result<Turn, GatewayError> {
        let path = format!("/booking-intents/{}/behaviours/{behaviour}", id.as_str());
        let response = self
            .call("POST", &path, Some(expected_version), body)
            .await?;
        // 429 surfaces IMMEDIATELY as Contended — no retry loop. See
        // [`RetryPolicy`]: a contended turn may already have committed, so the
        // only correct follow-up is the caller re-reading, which no amount of
        // re-POSTing the same version can substitute for.
        classify_turn(&response)
    }

    /// Follow an accepted turn to its outcome.
    ///
    /// Re-reads the authoritative projection until the booking leaves the
    /// in-flight set. **Never re-POSTs the behaviour** — the chase owns the
    /// effect (ADR-019), and a second POST would risk a second council booking.
    ///
    /// # Errors
    /// [`GatewayError::NotConverged`] once attempts or the deadline run out.
    pub async fn converge(
        &self,
        id: &BookingId,
        first_wait: Duration,
    ) -> Result<Projection, GatewayError> {
        let started = std::time::Instant::now();
        for attempt in 0..self.policy.max_convergence_polls {
            // The deadline bounds the SLEEP, not just the loop. The first
            // version slept the whole Retry-After before ever consulting the
            // clock — so a server saying `Retry-After: 3600` against a
            // thirty-second deadline blocked for an hour, which is a deadline
            // in name only.
            let remaining = self
                .policy
                .convergence_deadline
                .checked_sub(started.elapsed())
                .ok_or(GatewayError::NotConverged { attempts: attempt })?;
            tokio::time::sleep(first_wait.min(remaining)).await;

            let projection = self.read(id).await?;
            if !IN_FLIGHT.contains(&projection.state.as_str()) {
                return Ok(projection);
            }
        }
        Err(GatewayError::NotConverged {
            attempts: self.policy.max_convergence_polls,
        })
    }
}

/// One turn's response, classified — pure, so malformed shapes are testable
/// without a server that would have to misbehave on purpose.
///
/// # This POLICES the wire, not just reads it
///
/// The PR review's finding: the first version keyed on the status number plus
/// whichever body field happened to exist, so `202 {}` became a legitimate
/// acceptance with an invented one-second wait, and `422 + ETag + {"detail"}`
/// with no error name became a `Denied("(no error field)")`. An untrusted driver
/// is the one place that must not extend good faith to the wire: a response
/// missing the fields its status promises is `Unrecognized`, loudly, because the
/// alternative is acting on a contract nobody actually sent.
///
/// # Errors
/// Everything non-turn, via [`classify_error`]; `Unrecognized` for a shape that
/// breaks the contract its status claims.
#[allow(clippy::missing_panics_doc)] // the unwraps below are guarded by checks
pub fn classify_turn(response: &dto::RawResponse) -> Result<Turn, GatewayError> {
    match response.status {
        200 => {
            if response.body.get("converged").is_some() {
                return Ok(Turn::Converged {
                    state: response
                        .body
                        .get("state")
                        .and_then(serde_json::Value::as_str)
                        .map(str::to_owned),
                });
            }
            let (Some(state), Some(version)) = (
                response
                    .body
                    .get("state")
                    .and_then(serde_json::Value::as_str),
                response.etag,
            ) else {
                return Err(GatewayError::Unrecognized(
                    "a committed turn must carry a state and an ETag".to_owned(),
                ));
            };
            Ok(Turn::Committed {
                state: state.to_owned(),
                version,
            })
        }
        // A 202 without its schedule or its ETag is not an acceptance — it is a
        // response shaped like one, and inventing a wait for it would paper
        // over a server that stopped keeping its own contract.
        202 => match (response.retry_after, response.etag) {
            (Some(seconds), Some(_)) => Ok(Turn::Accepted {
                retry_after: Duration::from_secs(seconds),
            }),
            _ => Err(GatewayError::Unrecognized(
                "a 202 must carry Retry-After and an ETag".to_owned(),
            )),
        },
        409 if response.body.get("available_behaviours").is_some() => {
            let menu = response
                .body
                .get("available_behaviours")
                .and_then(serde_json::Value::as_array)
                .map(|rows| {
                    rows.iter()
                        .filter_map(|row| row.as_str().map(str::to_owned))
                        .collect()
                })
                .unwrap_or_default();
            Ok(Turn::NotAvailable { menu })
        }
        403 | 422 | 503 if response.is_domain_denial() => Ok(Turn::Denied {
            reason: response.error_text(),
        }),
        // A bare 403 with no denial shape is not a refusal this wire defines.
        403 => Err(GatewayError::Unrecognized(
            "a 403 must carry a domain denial shape".to_owned(),
        )),
        _ => Err(classify_error(response)),
    }
}

/// Everything that is not a turn, classified once.
///
/// Exhaustive over the statuses M5 and M5.1 actually emit — and keyed on the
/// distinguisher rather than the number wherever one number covers two
/// situations.
#[must_use]
pub fn classify_error(response: &dto::RawResponse) -> GatewayError {
    match response.status {
        401 => GatewayError::Unauthenticated,
        404 => GatewayError::UnknownBooking,
        428 => GatewayError::PreconditionRequired,
        400 => GatewayError::BadRequest(response.error_text()),
        409 => match (response.etag, response.body.get("version")) {
            // The owner's duplicate ships a version so a retry can carry a
            // precondition; a stranger's ships nothing at all. An ETag with no
            // version field is neither shape — refuse to guess.
            (Some(current), Some(_)) => GatewayError::Existing { current },
            (None, None) => GatewayError::IdentifierUnavailable,
            _ => GatewayError::Unrecognized(
                "a 409 must be the owner's shape (ETag + version) or the generic one (neither)"
                    .to_owned(),
            ),
        },
        412 => match response.etag {
            Some(current) => GatewayError::Stale { current },
            None => GatewayError::Unrecognized(
                "a 412 must carry the current version as an ETag".to_owned(),
            ),
        },
        422 => {
            if response.is_domain_denial() {
                GatewayError::Internal(format!(
                    "a domain denial reached the error path: {}",
                    response.error_text()
                ))
            } else {
                GatewayError::Malformed(response.error_text())
            }
        }
        429 => GatewayError::Contended,
        503 => {
            if response.is_domain_denial() {
                GatewayError::ProviderSilent(response.error_text())
            } else {
                GatewayError::Unavailable(response.error_text())
            }
        }
        500 => GatewayError::Internal(response.error_text()),
        other => GatewayError::Unrecognized(format!("status {other}")),
    }
}
