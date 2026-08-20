#![forbid(unsafe_code)]

//! The mock council: a real HTTP provider with its own database, its own
//! catalogue, its own clock and its own signing key.
//!
//! # What makes it worth building rather than faking
//!
//! Slice C proved the boundary's protocol against an in-process fake, which said
//! so in as many words: it *"deliberately does not enforce expiry… claiming this
//! fake proves them would be an overclaim."* Three obligations were left standing:
//!
//! - **Expiry** is enforced, from one clock, inside the write transaction
//!   ([`clock`], [`registry`]).
//! - **Absence is a durable tombstone**, committed before the answer is
//!   observable, so it survives a clock that steps backwards.
//! - **The facts are the council's own**, read from a catalogue rather than echoed
//!   from the request — otherwise a signature over a response proves only that the
//!   council signed the caller's claim.
//!
//! # The endpoints
//!
//! ```text
//! GET  /venues/{venue}/slots/{slot}   signed facts + a warrant for them
//! POST /bookings                      create, idempotent on effect identity
//! POST /bookings/{reference}/cancel   cancel, under its own effect identity
//! POST /effects/{id}/resolve          what became of this identity
//! ```
//!
//! The last is a `POST` because answering it **writes**: definitive absence is a
//! tombstone. A `GET` that tombstones is a lie about the method, and a caching
//! layer would eventually prove it.
//!
//! Cancellation carries its own effect identity and its own deadline. An earlier
//! draft took the spec's `POST /bookings/{ref}/cancel` at face value, with neither
//! — which would have left half the boundary's protocol unrepresentable, since a
//! cancellation *is* an external effect with an identity of its own.

pub mod clock;
pub mod pause;
pub mod registry;

use axum::{
    Json, Router,
    extract::{Path, State},
    http::StatusCode,
    routing::{get, post},
};
use clock::{Clock, SystemClock};
use council_wire::{
    AvailabilityFacts, CouncilSigner, EffectOutcome, SignedAvailabilityResponse,
    SignedEffectResponse,
};
use pause::{NeverPauses, Pauses};
use registry::{
    ApplyCancellation, CouncilError, CreateBooking, OperationKind, Registry, ResolveEffect,
};
use serde::{Deserialize, Serialize};
use sqlx::{
    SqlitePool,
    sqlite::{SqliteConnectOptions, SqliteJournalMode, SqlitePoolOptions},
};
use std::{path::Path as FsPath, sync::Arc, time::Duration};

/// How long an availability observation stays current, by the council's clock.
///
/// Short on purpose. It bounds how stale the facts behind a booking can be, and
/// the boundary re-reads availability every turn anyway.
pub const DEFAULT_AVAILABILITY_TTL_MS: i64 = 60_000;

/// Spec §11's four venues, as the catalogue holds them.
///
/// One passes every guard; the other three each fail exactly one, which is what
/// makes an end-to-end denial test about the guard rather than about plumbing.
pub const SPEC_SLOTS: &[SeedSlot] = &[
    SeedSlot {
        venue_id: "TH-A",
        slot_id: "SLOT-A",
        fee_pence: 4_500,
        capacity: 30,
        accessible: true,
        available: true,
    },
    // Fails accessibility, and only accessibility.
    SeedSlot {
        venue_id: "TH-B",
        slot_id: "SLOT-A",
        fee_pence: 4_500,
        capacity: 30,
        accessible: false,
        available: true,
    },
    // Fails the fee ceiling, and only the fee ceiling.
    SeedSlot {
        venue_id: "TH-C",
        slot_id: "SLOT-A",
        fee_pence: 9_000,
        capacity: 30,
        accessible: true,
        available: true,
    },
    // Fails capacity, and only capacity.
    SeedSlot {
        venue_id: "TH-D",
        slot_id: "SLOT-A",
        fee_pence: 4_500,
        capacity: 10,
        accessible: true,
        available: true,
    },
];

#[derive(Clone, Copy, Debug)]
pub struct SeedSlot {
    pub venue_id: &'static str,
    pub slot_id: &'static str,
    pub fee_pence: u64,
    pub capacity: u16,
    pub accessible: bool,
    pub available: bool,
}

/// Everything the council needs to run.
pub struct Council {
    registry: Arc<Registry>,
}

impl Council {
    /// Open the council's database at `path`, run migrations and seed spec §11's
    /// venues.
    ///
    /// # Errors
    /// [`CouncilError`] if the database cannot be opened, migrated or seeded.
    pub async fn open(
        path: impl AsRef<FsPath>,
        signer: Arc<CouncilSigner>,
    ) -> Result<Self, CouncilError> {
        Self::build(
            path,
            signer,
            Arc::new(SystemClock),
            Arc::new(NeverPauses),
            DEFAULT_AVAILABILITY_TTL_MS,
        )
        .await
    }

    /// Open with an injected clock and pause hook.
    ///
    /// # Errors
    /// As [`Self::open`].
    pub async fn build(
        path: impl AsRef<FsPath>,
        signer: Arc<CouncilSigner>,
        clock: Arc<dyn Clock>,
        pauses: Arc<dyn Pauses>,
        availability_ttl_ms: i64,
    ) -> Result<Self, CouncilError> {
        let options = SqliteConnectOptions::new()
            .filename(path)
            .create_if_missing(true)
            .foreign_keys(true)
            .journal_mode(SqliteJournalMode::Wal)
            .busy_timeout(Duration::from_secs(5));

        let pool = SqlitePoolOptions::new()
            .max_connections(5)
            .connect_with(options)
            .await?;

        let registry = Registry::open(pool, clock, signer, pauses, availability_ttl_ms).await?;
        let council = Self {
            registry: Arc::new(registry),
        };
        council.seed(SPEC_SLOTS).await?;
        Ok(council)
    }

    /// # Errors
    /// [`CouncilError::Sqlx`] on a write failure.
    pub async fn seed(&self, slots: &[SeedSlot]) -> Result<(), CouncilError> {
        for slot in slots {
            self.registry
                .seed_slot(
                    slot.venue_id,
                    slot.slot_id,
                    slot.fee_pence,
                    slot.capacity,
                    slot.accessible,
                    slot.available,
                )
                .await?;
        }
        Ok(())
    }

    pub fn registry(&self) -> &Arc<Registry> {
        &self.registry
    }

    pub fn pool(&self) -> &SqlitePool {
        self.registry.pool()
    }

    pub fn router(&self) -> Router {
        Router::new()
            .route("/venues/{venue_id}/slots/{slot_id}", get(get_availability))
            .route("/bookings", post(create_booking))
            .route("/bookings/{booking_reference}/cancel", post(cancel_booking))
            .route("/effects/{effect_intent_id}/resolve", post(resolve_effect))
            .with_state(Arc::clone(&self.registry))
    }
}

// ---------------------------------------------------------------- wire bodies

#[derive(Debug, Deserialize)]
pub struct CreateBookingBody {
    pub effect_intent_id: String,
    pub expires_at_ms: i64,
    pub venue_id: String,
    pub slot_id: String,
    pub attendees: u16,
    /// The fee the caller believes applies. Checked, never stored.
    pub fee_pence: u64,
    pub principal: String,
    /// The council's warrant for the availability facts this plan was built on.
    pub grant: String,
}

#[derive(Debug, Deserialize)]
pub struct CancelBookingBody {
    pub effect_intent_id: String,
    pub expires_at_ms: i64,
}

#[derive(Debug, Deserialize)]
pub struct ResolveBody {
    pub expires_at_ms: i64,
    /// Explicit, because the council must not have to parse our identity format
    /// to tell a booking from a cancellation.
    pub operation_kind: String,
}

/// An effect answer on the wire.
///
/// The outcome is a tag plus its fields rather than an externally-tagged enum, so
/// the JSON shape is stable and readable in a test failure.
#[derive(Debug, Serialize, Deserialize)]
pub struct EffectResponseBody {
    pub effect_intent_id: String,
    pub outcome: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub booking_reference: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub venue_id: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub slot_id: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub attendees: Option<u16>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub fee_pence: Option<u64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub principal: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub reason: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub signature: Option<String>,
}

#[derive(Debug, Serialize, Deserialize)]
pub struct AvailabilityResponseBody {
    pub venue_id: String,
    pub slot_id: String,
    pub outcome: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub capacity: Option<u16>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub accessible: Option<bool>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub available: Option<bool>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub fee_pence: Option<u64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub grant: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub valid_until_ms: Option<i64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub signature: Option<String>,
}

// ------------------------------------------------------------------- handlers

type Reg = State<Arc<Registry>>;

async fn get_availability(
    State(registry): Reg,
    Path((venue_id, slot_id)): Path<(String, String)>,
) -> (StatusCode, Json<AvailabilityResponseBody>) {
    // Unsigned on purpose when the council could not build an answer: a body it
    // could not produce is not its statement about anything, and signing an error
    // would make it look like one.
    let Ok(answer) = registry.availability(&venue_id, &slot_id).await else {
        return (
            StatusCode::SERVICE_UNAVAILABLE,
            Json(AvailabilityResponseBody {
                venue_id,
                slot_id,
                outcome: "Unavailable".to_owned(),
                capacity: None,
                accessible: None,
                available: None,
                fee_pence: None,
                grant: None,
                valid_until_ms: None,
                signature: None,
            }),
        );
    };

    let facts = answer.map(|answer| AvailabilityFacts {
        capacity: answer.capacity,
        accessible: answer.accessible,
        available: answer.available,
        fee_pence: answer.fee_pence,
        grant: answer.grant,
        valid_until_ms: answer.valid_until_ms,
    });

    let signature = registry
        .signer()
        .sign_availability(&venue_id, &slot_id, facts.as_ref())
        .ok();

    let status = if facts.is_some() {
        StatusCode::OK
    } else {
        StatusCode::NOT_FOUND
    };
    let outcome = if facts.is_some() {
        "SlotFacts"
    } else {
        "NoSuchSlot"
    };

    (
        status,
        Json(AvailabilityResponseBody {
            venue_id,
            slot_id,
            outcome: outcome.to_owned(),
            capacity: facts.as_ref().map(|f| f.capacity),
            accessible: facts.as_ref().map(|f| f.accessible),
            available: facts.as_ref().map(|f| f.available),
            fee_pence: facts.as_ref().map(|f| f.fee_pence),
            grant: facts.as_ref().map(|f| f.grant.clone()),
            valid_until_ms: facts.as_ref().map(|f| f.valid_until_ms),
            signature,
        }),
    )
}

async fn create_booking(
    State(registry): Reg,
    Json(body): Json<CreateBookingBody>,
) -> (StatusCode, Json<EffectResponseBody>) {
    let id = body.effect_intent_id.clone();
    let outcome = registry
        .create_booking(&CreateBooking {
            effect_intent_id: body.effect_intent_id,
            expires_at_ms: body.expires_at_ms,
            venue_id: body.venue_id,
            slot_id: body.slot_id,
            attendees: body.attendees,
            asserted_fee_pence: body.fee_pence,
            principal: body.principal,
            grant: body.grant,
        })
        .await;
    effect_reply(&registry, id, outcome)
}

async fn cancel_booking(
    State(registry): Reg,
    Path(booking_reference): Path<String>,
    Json(body): Json<CancelBookingBody>,
) -> (StatusCode, Json<EffectResponseBody>) {
    let id = body.effect_intent_id.clone();
    let outcome = registry
        .apply_cancellation(&ApplyCancellation {
            effect_intent_id: body.effect_intent_id,
            expires_at_ms: body.expires_at_ms,
            booking_reference,
        })
        .await;
    effect_reply(&registry, id, outcome)
}

async fn resolve_effect(
    State(registry): Reg,
    Path(effect_intent_id): Path<String>,
    Json(body): Json<ResolveBody>,
) -> (StatusCode, Json<EffectResponseBody>) {
    let kind = match body.operation_kind.as_str() {
        "Book" => OperationKind::Book,
        "Cancel" => OperationKind::Cancel,
        other => {
            // An unreadable kind cannot be bound, so nothing is written and the
            // answer says nothing about the effect.
            return unsigned_unavailable(
                effect_intent_id,
                format!("unknown operation_kind {other:?}"),
            );
        }
    };

    let outcome = registry
        .resolve(&ResolveEffect {
            effect_intent_id: effect_intent_id.clone(),
            expires_at_ms: body.expires_at_ms,
            kind,
        })
        .await;
    effect_reply(&registry, effect_intent_id, outcome)
}

fn unsigned_unavailable(
    effect_intent_id: String,
    reason: String,
) -> (StatusCode, Json<EffectResponseBody>) {
    (
        StatusCode::SERVICE_UNAVAILABLE,
        Json(EffectResponseBody {
            effect_intent_id,
            outcome: "Unavailable".to_owned(),
            booking_reference: None,
            venue_id: None,
            slot_id: None,
            attendees: None,
            fee_pence: None,
            principal: None,
            reason: Some(reason),
            signature: None,
        }),
    )
}

/// Turn an outcome into a signed reply.
///
/// A storage failure becomes `Unavailable`, never a rejection: a council that
/// could not write says nothing about whether the effect exists, and a rejection
/// is acted on irreversibly.
fn effect_reply(
    registry: &Arc<Registry>,
    effect_intent_id: String,
    outcome: Result<EffectOutcome, CouncilError>,
) -> (StatusCode, Json<EffectResponseBody>) {
    let outcome = match outcome {
        Ok(outcome) => outcome,
        Err(error) => EffectOutcome::Unavailable {
            reason: error.to_string(),
        },
    };

    let signature = registry
        .signer()
        .sign_effect(&effect_intent_id, &outcome)
        .ok();
    let response = SignedEffectResponse {
        effect_intent_id,
        outcome,
        signature,
    };
    (status_for(&response.outcome), Json(to_body(response)))
}

const fn status_for(outcome: &EffectOutcome) -> StatusCode {
    match outcome {
        EffectOutcome::BookingCreated(_) | EffectOutcome::CancellationApplied { .. } => {
            StatusCode::OK
        }
        EffectOutcome::DefinitivelyAbsent | EffectOutcome::NotYetVisible => StatusCode::OK,
        EffectOutcome::ProviderRejected { .. } => StatusCode::UNPROCESSABLE_ENTITY,
        EffectOutcome::ProtocolConflict { .. } => StatusCode::CONFLICT,
        EffectOutcome::Unavailable { .. } => StatusCode::SERVICE_UNAVAILABLE,
    }
}

fn to_body(response: SignedEffectResponse) -> EffectResponseBody {
    let mut body = EffectResponseBody {
        effect_intent_id: response.effect_intent_id,
        outcome: String::new(),
        booking_reference: None,
        venue_id: None,
        slot_id: None,
        attendees: None,
        fee_pence: None,
        principal: None,
        reason: None,
        signature: response.signature,
    };

    match response.outcome {
        EffectOutcome::BookingCreated(facts) => {
            "BookingCreated".clone_into(&mut body.outcome);
            body.booking_reference = Some(facts.booking_reference);
            body.venue_id = Some(facts.venue_id);
            body.slot_id = Some(facts.slot_id);
            body.attendees = Some(facts.attendees);
            body.fee_pence = Some(facts.fee_pence);
            body.principal = Some(facts.principal);
        }
        EffectOutcome::CancellationApplied { booking_reference } => {
            "CancellationApplied".clone_into(&mut body.outcome);
            body.booking_reference = Some(booking_reference);
        }
        EffectOutcome::DefinitivelyAbsent => {
            "DefinitivelyAbsent".clone_into(&mut body.outcome);
        }
        EffectOutcome::NotYetVisible => {
            "NotYetVisible".clone_into(&mut body.outcome);
        }
        EffectOutcome::ProviderRejected { reason } => {
            "ProviderRejected".clone_into(&mut body.outcome);
            body.reason = Some(reason);
        }
        EffectOutcome::ProtocolConflict { reason } => {
            "ProtocolConflict".clone_into(&mut body.outcome);
            body.reason = Some(reason);
        }
        EffectOutcome::Unavailable { reason } => {
            "Unavailable".clone_into(&mut body.outcome);
            body.reason = Some(reason);
        }
    }
    body
}

/// Read an availability body back into the verifiable payload shape.
///
/// # Errors
/// Never — malformed bodies simply produce `facts: None`, which the client treats
/// as no answer.
#[must_use]
pub fn availability_from_body(body: AvailabilityResponseBody) -> SignedAvailabilityResponse {
    let facts = match (
        body.capacity,
        body.accessible,
        body.available,
        body.fee_pence,
        body.grant,
        body.valid_until_ms,
    ) {
        (
            Some(capacity),
            Some(accessible),
            Some(available),
            Some(fee_pence),
            Some(grant),
            Some(valid_until_ms),
        ) => Some(AvailabilityFacts {
            capacity,
            accessible,
            available,
            fee_pence,
            grant,
            valid_until_ms,
        }),
        _ => None,
    };
    SignedAvailabilityResponse {
        venue_id: body.venue_id,
        slot_id: body.slot_id,
        facts,
        signature: body.signature,
    }
}

/// Read an effect body back into the verifiable payload shape.
///
/// # Errors
/// [`council_wire::WireError::UnknownOutcome`] if the tag is not one the council
/// emits, and [`council_wire::WireError::Codec`] if a tagged outcome is missing a
/// field it must carry — both refusals rather than a guessed outcome.
pub fn effect_from_body(
    body: EffectResponseBody,
) -> Result<SignedEffectResponse, council_wire::WireError> {
    use council_wire::{BookingFacts, WireError};

    let missing = |field: &'static str| WireError::UnknownOutcome(format!("missing {field}"));

    let outcome = match body.outcome.as_str() {
        "BookingCreated" => EffectOutcome::BookingCreated(BookingFacts {
            booking_reference: body.booking_reference.ok_or_else(|| missing("reference"))?,
            venue_id: body.venue_id.ok_or_else(|| missing("venue_id"))?,
            slot_id: body.slot_id.ok_or_else(|| missing("slot_id"))?,
            attendees: body.attendees.ok_or_else(|| missing("attendees"))?,
            fee_pence: body.fee_pence.ok_or_else(|| missing("fee_pence"))?,
            principal: body.principal.ok_or_else(|| missing("principal"))?,
        }),
        "CancellationApplied" => EffectOutcome::CancellationApplied {
            booking_reference: body.booking_reference.ok_or_else(|| missing("reference"))?,
        },
        "DefinitivelyAbsent" => EffectOutcome::DefinitivelyAbsent,
        "NotYetVisible" => EffectOutcome::NotYetVisible,
        "ProviderRejected" => EffectOutcome::ProviderRejected {
            reason: body.reason.ok_or_else(|| missing("reason"))?,
        },
        "ProtocolConflict" => EffectOutcome::ProtocolConflict {
            reason: body.reason.ok_or_else(|| missing("reason"))?,
        },
        "Unavailable" => EffectOutcome::Unavailable {
            reason: body.reason.unwrap_or_default(),
        },
        other => return Err(WireError::UnknownOutcome(other.to_owned())),
    };

    Ok(SignedEffectResponse {
        effect_intent_id: body.effect_intent_id,
        outcome,
        signature: body.signature,
    })
}
