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
#[derive(Clone, Copy, Debug)]
pub struct RetryPolicy {
    /// Bounded, because an unbounded retry is a hang wearing a loop.
    pub max_contention_retries: u32,
    pub max_convergence_polls: u32,
    pub convergence_deadline: Duration,
}

impl Default for RetryPolicy {
    fn default() -> Self {
        Self {
            max_contention_retries: 3,
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
    http: reqwest::Client,
    policy: RetryPolicy,
    /// Set when the caller wants request ids it chose rather than minted.
    request_id: Option<String>,
}

impl Gateway {
    #[must_use]
    pub fn new(base: impl Into<String>, bearer: impl Into<String>) -> Self {
        Self {
            base: base.into(),
            bearer: bearer.into(),
            http: reqwest::Client::new(),
            policy: RetryPolicy::default(),
            request_id: None,
        }
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

    fn authorized(&self, builder: reqwest::RequestBuilder) -> reqwest::RequestBuilder {
        let builder = builder.header("authorization", format!("Bearer {}", self.bearer));
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

        Ok(dto::RawResponse {
            status,
            etag,
            retry_after,
            request_id,
            body,
        })
    }

    /// Everything that is not a turn, classified once.
    ///
    /// Exhaustive over the statuses M5 and M5.1 actually emit — and keyed on the
    /// distinguisher rather than the number wherever one number covers two
    /// situations.
    fn classify_error(response: &dto::RawResponse) -> GatewayError {
        match response.status {
            401 => GatewayError::Unauthenticated,
            404 => GatewayError::UnknownBooking,
            428 => GatewayError::PreconditionRequired,
            400 => GatewayError::BadRequest(response.error_text()),
            409 => match response.etag {
                // The owner's duplicate ships a version so a retry can carry a
                // precondition; a stranger's ships nothing at all.
                Some(current) => GatewayError::Existing { current },
                None => GatewayError::IdentifierUnavailable,
            },
            412 => GatewayError::Stale {
                current: response.etag.unwrap_or_default(),
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
    /// Returns `Accepted` **without converging** — see [`Turn::Accepted`]. On
    /// contention it retries within the policy, then gives up typed rather than
    /// looping.
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
        let mut attempt = 0;
        loop {
            let response = self
                .call("POST", &path, Some(expected_version), body.clone())
                .await?;
            match response.status {
                200 => {
                    // Two shapes share this number: a committed transition, and
                    // a chase that found its effect already settled.
                    if response.body.get("converged").is_some() {
                        return Ok(Turn::Converged {
                            state: response
                                .body
                                .get("state")
                                .and_then(serde_json::Value::as_str)
                                .map(str::to_owned),
                        });
                    }
                    let state = response
                        .body
                        .get("state")
                        .and_then(serde_json::Value::as_str)
                        .ok_or_else(|| {
                            GatewayError::Unrecognized(
                                "a committed turn without a state".to_owned(),
                            )
                        })?
                        .to_owned();
                    return Ok(Turn::Committed {
                        state,
                        version: response.etag.unwrap_or_default(),
                    });
                }
                202 => {
                    return Ok(Turn::Accepted {
                        retry_after: Duration::from_secs(response.retry_after.unwrap_or(1)),
                    });
                }
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
                    return Ok(Turn::NotAvailable { menu });
                }
                403 => {
                    return Ok(Turn::Denied {
                        reason: response.error_text(),
                    });
                }
                // One arm for two statuses on purpose: a 422 guard story and a
                // 503 provider-silence are both domain denials wearing different
                // numbers, and the caller's next move is the same for each.
                422 | 503 if response.is_domain_denial() => {
                    return Ok(Turn::Denied {
                        reason: response.error_text(),
                    });
                }
                429 if attempt < self.policy.max_contention_retries => {
                    attempt += 1;
                    tokio::time::sleep(Duration::from_secs(response.retry_after.unwrap_or(1)))
                        .await;
                }
                _ => return Err(Self::classify_error(&response)),
            }
        }
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
        let mut wait = first_wait;
        for attempt in 0..self.policy.max_convergence_polls {
            tokio::time::sleep(wait).await;
            let projection = self.read(id).await?;
            if !IN_FLIGHT.contains(&projection.state.as_str()) {
                return Ok(projection);
            }
            if started.elapsed() > self.policy.convergence_deadline {
                return Err(GatewayError::NotConverged {
                    attempts: attempt + 1,
                });
            }
            wait = first_wait;
        }
        Err(GatewayError::NotConverged {
            attempts: self.policy.max_convergence_polls,
        })
    }
}
