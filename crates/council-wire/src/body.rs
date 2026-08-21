//! The JSON on the wire, and its conversion to and from the signed shapes.
//!
//! Here rather than in the service because both sides need it and neither should
//! depend on the other. A client that imported the server's crate to learn the
//! response shape would make the adapter a downstream of the thing it is meant to
//! be independent of — and would quietly permit the server's internals into the
//! client's compile graph.
//!
//! # Why the outcome is a tag plus optional fields
//!
//! Not serde's externally-tagged enum. Two reasons, and the second is the one that
//! matters: a flat object reads legibly in a failing test, and a response that
//! arrives with a tag this build does not know becomes a *refusal* rather than a
//! deserialisation error at the transport layer. An unknown outcome is a thing the
//! provider said that we cannot interpret, which is exactly `Unknown` — not a
//! malformed request.

use crate::{
    AvailabilityFacts, BookingFacts, EffectOutcome, SignedAvailabilityResponse,
    SignedEffectResponse, WireError,
};
use serde::{Deserialize, Serialize};

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct CreateBookingBody {
    pub effect_intent_id: String,
    pub expires_at_ms: i64,
    pub venue_id: String,
    pub slot_id: String,
    pub attendees: u16,
    /// The fee the caller believes applies.
    ///
    /// An assertion, not an instruction: the council checks it against its own
    /// catalogue and refuses on disagreement. It will not book at a price the
    /// caller made up, and the value it stores is always its own.
    pub fee_pence: u64,
    pub principal: String,
    /// The council's warrant for the availability facts this plan was built on.
    pub grant: String,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct CancelBookingBody {
    pub effect_intent_id: String,
    pub expires_at_ms: i64,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ResolveBody {
    pub expires_at_ms: i64,
    /// Explicit, so the council never has to parse our identity format to tell a
    /// booking from a cancellation.
    pub operation_kind: String,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct EffectResponseBody {
    pub effect_intent_id: String,
    pub outcome: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub booking_reference: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub venue_id: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub slot_id: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub attendees: Option<u16>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub fee_pence: Option<u64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub principal: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub reason: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub signature: Option<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct AvailabilityResponseBody {
    pub venue_id: String,
    pub slot_id: String,
    pub outcome: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub capacity: Option<u16>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub accessible: Option<bool>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub available: Option<bool>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub fee_pence: Option<u64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub grant: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub valid_until_ms: Option<i64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub signature: Option<String>,
}

impl From<SignedEffectResponse> for EffectResponseBody {
    fn from(response: SignedEffectResponse) -> Self {
        let mut body = Self {
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
}

impl TryFrom<EffectResponseBody> for SignedEffectResponse {
    type Error = WireError;

    fn try_from(body: EffectResponseBody) -> Result<Self, Self::Error> {
        // A tagged outcome missing a field it must carry is refused rather than
        // filled in. Defaulting a booking reference to the empty string would turn
        // a malformed response into a confidently wrong fact.
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

        Ok(Self {
            effect_intent_id: body.effect_intent_id,
            outcome,
            signature: body.signature,
        })
    }
}

impl From<SignedAvailabilityResponse> for AvailabilityResponseBody {
    fn from(response: SignedAvailabilityResponse) -> Self {
        let outcome = if response.facts.is_some() {
            "SlotFacts"
        } else {
            "NoSuchSlot"
        };
        Self {
            venue_id: response.venue_id,
            slot_id: response.slot_id,
            outcome: outcome.to_owned(),
            capacity: response.facts.as_ref().map(|f| f.capacity),
            accessible: response.facts.as_ref().map(|f| f.accessible),
            available: response.facts.as_ref().map(|f| f.available),
            fee_pence: response.facts.as_ref().map(|f| f.fee_pence),
            grant: response.facts.as_ref().map(|f| f.grant.clone()),
            valid_until_ms: response.facts.as_ref().map(|f| f.valid_until_ms),
            signature: response.signature,
        }
    }
}

impl From<AvailabilityResponseBody> for SignedAvailabilityResponse {
    /// A body missing any fact field yields `facts: None`.
    ///
    /// Infallible on purpose: an availability answer we cannot read is *no*
    /// answer, and the proposal door already treats a missing observation as
    /// grounds to refuse. Distinguishing "malformed" from "unknown slot" would
    /// give a caller a decision to make where the only safe action is the same
    /// either way.
    ///
    /// Note the signature check happens *after* this — a body that lost a field in
    /// transit will fail verification too, because the signature covers the whole
    /// payload including the presence of the facts.
    fn from(body: AvailabilityResponseBody) -> Self {
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
        Self {
            venue_id: body.venue_id,
            slot_id: body.slot_id,
            facts,
            signature: body.signature,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::{AvailabilityResponseBody, EffectResponseBody};
    use crate::{
        AvailabilityFacts, BookingFacts, EffectOutcome, SignedAvailabilityResponse,
        SignedEffectResponse, WireError,
    };

    fn created() -> SignedEffectResponse {
        SignedEffectResponse {
            effect_intent_id: "EFF-1".to_owned(),
            outcome: EffectOutcome::BookingCreated(BookingFacts {
                booking_reference: "TH-90001".to_owned(),
                venue_id: "TH-A".to_owned(),
                slot_id: "SLOT-A".to_owned(),
                attendees: 20,
                fee_pence: 4_500,
                principal: "lucy".to_owned(),
            }),
            signature: Some("sig".to_owned()),
        }
    }

    /// Every outcome survives the round trip. A variant added later without a
    /// conversion arm fails here rather than in production.
    #[test]
    fn every_outcome_round_trips_through_json() {
        let outcomes = [
            created().outcome,
            EffectOutcome::CancellationApplied {
                booking_reference: "TH-90001".to_owned(),
            },
            EffectOutcome::DefinitivelyAbsent,
            EffectOutcome::NotYetVisible,
            EffectOutcome::ProviderRejected {
                reason: "no".to_owned(),
            },
            EffectOutcome::ProtocolConflict {
                reason: "clash".to_owned(),
            },
            EffectOutcome::Unavailable {
                reason: "busy".to_owned(),
            },
        ];

        for outcome in outcomes {
            let original = SignedEffectResponse {
                effect_intent_id: "EFF-1".to_owned(),
                outcome,
                signature: Some("sig".to_owned()),
            };
            let body = EffectResponseBody::from(original.clone());
            let json = serde_json::to_string(&body).expect("serialise");
            let decoded: EffectResponseBody = serde_json::from_str(&json).expect("deserialise");
            let back = SignedEffectResponse::try_from(decoded).expect("convert");
            assert_eq!(back, original);
        }
    }

    /// An outcome tag from a newer council is a thing we cannot interpret, which
    /// is `Unknown` — not a transport error and certainly not a guess.
    #[test]
    fn an_unrecognised_outcome_is_refused_by_name() {
        let body = EffectResponseBody {
            effect_intent_id: "EFF-1".to_owned(),
            outcome: "SomethingNewer".to_owned(),
            booking_reference: None,
            venue_id: None,
            slot_id: None,
            attendees: None,
            fee_pence: None,
            principal: None,
            reason: None,
            signature: Some("sig".to_owned()),
        };
        assert_eq!(
            SignedEffectResponse::try_from(body),
            Err(WireError::UnknownOutcome("SomethingNewer".to_owned()))
        );
    }

    /// A creation missing its fee is refused, not defaulted. A zero fee would be a
    /// confidently wrong fact the domain would then bind against.
    #[test]
    fn a_creation_missing_a_fact_field_is_refused() {
        let mut body = EffectResponseBody::from(created());
        body.fee_pence = None;
        assert!(matches!(
            SignedEffectResponse::try_from(body),
            Err(WireError::UnknownOutcome(_))
        ));
    }

    #[test]
    fn availability_round_trips_and_a_partial_body_yields_no_facts() {
        let original = SignedAvailabilityResponse {
            venue_id: "TH-A".to_owned(),
            slot_id: "SLOT-A".to_owned(),
            facts: Some(AvailabilityFacts {
                capacity: 30,
                accessible: true,
                available: true,
                fee_pence: 4_500,
                grant: "GRANT".to_owned(),
                valid_until_ms: 1_000_060_000,
            }),
            signature: Some("sig".to_owned()),
        };
        let body = AvailabilityResponseBody::from(original.clone());
        let json = serde_json::to_string(&body).expect("serialise");
        let decoded: AvailabilityResponseBody = serde_json::from_str(&json).expect("deserialise");
        assert_eq!(SignedAvailabilityResponse::from(decoded), original);

        let mut partial = AvailabilityResponseBody::from(original);
        partial.grant = None;
        assert!(SignedAvailabilityResponse::from(partial).facts.is_none());
    }
}
