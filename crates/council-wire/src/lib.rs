#![forbid(unsafe_code)]

//! The council's wire protocol: payload shapes, the bytes that get signed, and
//! the signing and verifying of them.
//!
//! # Why one crate and not one per side
//!
//! The plan for this slice had the service and the client each implement the
//! encoding, with a golden-vector test asserting they agreed. That is the wrong
//! shape for the same reason every other defect in this project was the wrong
//! shape: it makes one fact live in two places and then tests that the copies
//! match. Two encoders that agree today are two encoders that can disagree after
//! a refactor, and the test only catches it if the vector happens to cover the
//! field that moved.
//!
//! So there is one encoder, and the golden vectors here pin *it* rather than
//! pinning an agreement. Drift stops being a thing that can happen quietly.
//!
//! # What a signature does and does not establish
//!
//! It establishes that the holder of the council's private key produced these
//! exact bytes. That is what `bld-kernel`'s `Verifier` contract asks for, and it
//! is why an unsigned-but-field-perfect response is refused.
//!
//! It does **not** establish that the key belongs to the real council. That is
//! key distribution, and for a POC with a pinned test key it is out of scope —
//! stated rather than glossed.

pub mod codec;

use base64::Engine as _;
use codec::{CodecError, Decoder, Encoder, MessageType};
use ed25519_dalek::{Signature, Signer as _, SigningKey, Verifier as _, VerifyingKey};
use thiserror::Error;

pub use ed25519_dalek::{SECRET_KEY_LENGTH, SigningKey as CouncilSigningKey};

const B64: base64::engine::general_purpose::GeneralPurpose =
    base64::engine::general_purpose::STANDARD_NO_PAD;

#[derive(Debug, Error, PartialEq, Eq)]
pub enum WireError {
    #[error(transparent)]
    Codec(#[from] CodecError),
    #[error("the response carries no signature")]
    Unsigned,
    #[error("the signature is not the council's")]
    BadSignature,
    #[error("the signature is not well formed")]
    MalformedSignature,
    #[error("unknown outcome {0:?}")]
    UnknownOutcome(String),
    #[error("the payload is not valid base64")]
    NotBase64,
}

/// What the council said about one effect.
///
/// Seven variants, because a boolean cannot distinguish them and the first draft
/// of this protocol tried. Only the first four may become a provider fact; the
/// last three are `Unknown` and drive nothing.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum EffectOutcome {
    /// A booking exists for this identity, with these facts.
    BookingCreated(BookingFacts),
    /// A cancellation exists for this identity.
    CancellationApplied { booking_reference: String },
    /// Nothing was created for this identity, and nothing ever can be.
    DefinitivelyAbsent,
    /// The council authoritatively refused. Terminal.
    ProviderRejected { reason: String },
    /// The council has heard of this identity and nothing has settled yet.
    ///
    /// Not absence. The distinction is the whole of ADR-016: absence is a durable
    /// tombstone, this is a report that the story is still running.
    NotYetVisible,
    /// The request contradicts what the council already bound for this identity —
    /// a different deadline, or a different kind. Our bug, not a provider fact.
    ProtocolConflict { reason: String },
    /// The council could not answer. Says nothing about whether the effect exists.
    Unavailable { reason: String },
}

/// The complete canonical facts of a booking, as the council holds them.
///
/// Every field the domain binds against the persisted plan. An earlier draft
/// returned only a reference, which would have left the verifier taking these
/// from the caller's own context — the domain then comparing the plan against
/// itself and proving nothing.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct BookingFacts {
    pub booking_reference: String,
    pub venue_id: String,
    pub slot_id: String,
    pub attendees: u16,
    pub fee_pence: u64,
    pub principal: String,
}

/// A signed answer about one effect.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct SignedEffectResponse {
    pub effect_intent_id: String,
    pub outcome: EffectOutcome,
    /// Base64, absent when the response was not signed at all — which the
    /// verifier refuses. Present-and-wrong and absent are different failures and
    /// both must be refused, so both are representable.
    pub signature: Option<String>,
}

/// A signed answer about one venue slot.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct SignedAvailabilityResponse {
    pub venue_id: String,
    pub slot_id: String,
    pub facts: Option<AvailabilityFacts>,
    pub signature: Option<String>,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct AvailabilityFacts {
    pub capacity: u16,
    pub accessible: bool,
    pub available: bool,
    pub fee_pence: u64,
    /// The council's warrant for these facts, opaque to the holder.
    pub grant: String,
    /// When this observation stops being current, by the council's clock.
    ///
    /// On the wire purely so a client can skip a round trip it knows will fail.
    /// **Not** load-bearing: the council re-checks it against its own clock when
    /// the grant comes back, because a client clock running slow would otherwise
    /// accept a dead observation.
    pub valid_until_ms: i64,
}

/// The contents of an [`AvailabilityFacts::grant`], as the council reads it back.
///
/// Only the council ever constructs or inspects one. It is in this crate rather
/// than the service because the encoding must be defined once, not because a
/// client has any business here.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct GrantClaims {
    pub venue_id: String,
    pub slot_id: String,
    /// The catalogue row's version when the observation was made. Bumped by every
    /// mutation of that row, so a grant naming an older version is stale even if
    /// the fields a booking checks happen to be unchanged.
    pub row_version: u64,
    pub valid_until_ms: i64,
}

fn effect_payload(effect_intent_id: &str, outcome: &EffectOutcome) -> Result<Vec<u8>, CodecError> {
    let mut encoder = Encoder::new(MessageType::Effect);
    encoder.text(effect_intent_id)?;
    encoder.text(outcome_tag(outcome))?;
    match outcome {
        EffectOutcome::BookingCreated(facts) => {
            encoder.text(&facts.booking_reference)?;
            encoder.text(&facts.venue_id)?;
            encoder.text(&facts.slot_id)?;
            encoder.number(u64::from(facts.attendees));
            encoder.number(facts.fee_pence);
            encoder.text(&facts.principal)?;
        }
        EffectOutcome::CancellationApplied { booking_reference } => {
            encoder.text(booking_reference)?;
        }
        EffectOutcome::ProviderRejected { reason }
        | EffectOutcome::ProtocolConflict { reason }
        | EffectOutcome::Unavailable { reason } => {
            encoder.text(reason)?;
        }
        EffectOutcome::DefinitivelyAbsent | EffectOutcome::NotYetVisible => {}
    }
    Ok(encoder.finish())
}

const fn outcome_tag(outcome: &EffectOutcome) -> &'static str {
    match outcome {
        EffectOutcome::BookingCreated(_) => "BookingCreated",
        EffectOutcome::CancellationApplied { .. } => "CancellationApplied",
        EffectOutcome::DefinitivelyAbsent => "DefinitivelyAbsent",
        EffectOutcome::ProviderRejected { .. } => "ProviderRejected",
        EffectOutcome::NotYetVisible => "NotYetVisible",
        EffectOutcome::ProtocolConflict { .. } => "ProtocolConflict",
        EffectOutcome::Unavailable { .. } => "Unavailable",
    }
}

fn availability_payload(
    venue_id: &str,
    slot_id: &str,
    facts: Option<&AvailabilityFacts>,
) -> Result<Vec<u8>, CodecError> {
    let mut encoder = Encoder::new(MessageType::Availability);
    encoder.text(venue_id)?;
    encoder.text(slot_id)?;
    match facts {
        Some(facts) => {
            encoder.boolean(true);
            encoder.number(u64::from(facts.capacity));
            encoder.boolean(facts.accessible);
            encoder.boolean(facts.available);
            encoder.number(facts.fee_pence);
            encoder.text(&facts.grant)?;
            encoder.number(u64::try_from(facts.valid_until_ms).unwrap_or(0));
        }
        None => {
            encoder.boolean(false);
        }
    }
    Ok(encoder.finish())
}

fn grant_payload(claims: &GrantClaims) -> Result<Vec<u8>, CodecError> {
    let mut encoder = Encoder::new(MessageType::Grant);
    encoder.text(&claims.venue_id)?;
    encoder.text(&claims.slot_id)?;
    encoder.number(claims.row_version);
    encoder.number(u64::try_from(claims.valid_until_ms).unwrap_or(0));
    Ok(encoder.finish())
}

/// Signs the council's answers. Only the council holds one.
pub struct CouncilSigner {
    key: SigningKey,
}

impl CouncilSigner {
    #[must_use]
    pub const fn new(key: SigningKey) -> Self {
        Self { key }
    }

    #[must_use]
    pub fn verifying_key(&self) -> VerifyingKey {
        self.key.verifying_key()
    }

    /// # Errors
    /// [`CodecError`] if a field exceeds the wire's length limit.
    pub fn sign_effect(
        &self,
        effect_intent_id: &str,
        outcome: &EffectOutcome,
    ) -> Result<String, CodecError> {
        let payload = effect_payload(effect_intent_id, outcome)?;
        Ok(B64.encode(self.key.sign(&payload).to_bytes()))
    }

    /// # Errors
    /// [`CodecError`] if a field exceeds the wire's length limit.
    pub fn sign_availability(
        &self,
        venue_id: &str,
        slot_id: &str,
        facts: Option<&AvailabilityFacts>,
    ) -> Result<String, CodecError> {
        let payload = availability_payload(venue_id, slot_id, facts)?;
        Ok(B64.encode(self.key.sign(&payload).to_bytes()))
    }

    /// Mint a warrant for one availability observation.
    ///
    /// # Errors
    /// [`CodecError`] if a field exceeds the wire's length limit.
    pub fn mint_grant(&self, claims: &GrantClaims) -> Result<String, CodecError> {
        let payload = grant_payload(claims)?;
        let signature = self.key.sign(&payload).to_bytes();
        // The claims travel with the signature because the council must read them
        // back without keeping per-grant state. It is a bearer token, not a
        // database key: a council that stored issued grants would need to garbage
        // collect them, and a grant that outlived its row would then be
        // indistinguishable from a forged one.
        Ok(format!("{}.{}", B64.encode(payload), B64.encode(signature)))
    }

    /// Read a warrant back, refusing one this council did not mint.
    ///
    /// # Errors
    /// [`WireError`] if the token is malformed, not base64, not signed by this
    /// key, or not a grant.
    pub fn open_grant(&self, token: &str) -> Result<GrantClaims, WireError> {
        let (payload_b64, signature_b64) = token.split_once('.').ok_or(WireError::Unsigned)?;
        let payload = B64.decode(payload_b64).map_err(|_| WireError::NotBase64)?;
        let signature = decode_signature(signature_b64)?;
        self.key
            .verifying_key()
            .verify(&payload, &signature)
            .map_err(|_| WireError::BadSignature)?;

        let mut decoder = Decoder::new(&payload, MessageType::Grant)?;
        let claims = GrantClaims {
            venue_id: decoder.text()?,
            slot_id: decoder.text()?,
            row_version: decoder.number()?,
            valid_until_ms: decoder.i64()?,
        };
        decoder.finish()?;
        Ok(claims)
    }
}

/// Checks that an answer came from the council, using a pinned public key.
#[derive(Clone, Copy, Debug)]
pub struct CouncilKey {
    key: VerifyingKey,
}

impl CouncilKey {
    #[must_use]
    pub const fn new(key: VerifyingKey) -> Self {
        Self { key }
    }

    /// # Errors
    /// [`WireError::Unsigned`] if there is no signature,
    /// [`WireError::BadSignature`] if it is not this key's.
    pub fn check_effect(&self, response: &SignedEffectResponse) -> Result<(), WireError> {
        let payload = effect_payload(&response.effect_intent_id, &response.outcome)?;
        self.check(&payload, response.signature.as_deref())
    }

    /// # Errors
    /// As [`Self::check_effect`].
    pub fn check_availability(
        &self,
        response: &SignedAvailabilityResponse,
    ) -> Result<(), WireError> {
        let payload = availability_payload(
            &response.venue_id,
            &response.slot_id,
            response.facts.as_ref(),
        )?;
        self.check(&payload, response.signature.as_deref())
    }

    fn check(&self, payload: &[u8], signature: Option<&str>) -> Result<(), WireError> {
        let signature = decode_signature(signature.ok_or(WireError::Unsigned)?)?;
        self.key
            .verify(payload, &signature)
            .map_err(|_| WireError::BadSignature)
    }
}

fn decode_signature(encoded: &str) -> Result<Signature, WireError> {
    let raw = B64.decode(encoded).map_err(|_| WireError::NotBase64)?;
    let bytes: [u8; 64] = raw.try_into().map_err(|_| WireError::MalformedSignature)?;
    Ok(Signature::from_bytes(&bytes))
}

#[cfg(test)]
mod tests {
    use super::{
        AvailabilityFacts, BookingFacts, CouncilKey, CouncilSigner, EffectOutcome, GrantClaims,
        SignedAvailabilityResponse, SignedEffectResponse, WireError, availability_payload,
        effect_payload,
    };
    use base64::Engine as _;
    use ed25519_dalek::SigningKey;

    /// A fixed key, so the golden vectors below are stable.
    fn signer() -> CouncilSigner {
        CouncilSigner::new(SigningKey::from_bytes(&[7u8; 32]))
    }

    fn booking() -> BookingFacts {
        BookingFacts {
            booking_reference: "TH-90001".to_owned(),
            venue_id: "TH-A".to_owned(),
            slot_id: "SLOT-A".to_owned(),
            attendees: 20,
            fee_pence: 4_500,
            principal: "lucy".to_owned(),
        }
    }

    fn availability() -> AvailabilityFacts {
        AvailabilityFacts {
            capacity: 30,
            accessible: true,
            available: true,
            fee_pence: 4_500,
            grant: "GRANT".to_owned(),
            valid_until_ms: 1_000_060_000,
        }
    }

    /// The golden vector. It pins the encoder rather than pinning an agreement
    /// between two encoders — see this module's header for why that distinction
    /// matters. If this string moves, every signature the council has ever issued
    /// means something different, and that has to be a deliberate act rather than
    /// a side effect of a refactor.
    #[test]
    fn the_effect_payload_encoding_is_pinned() {
        let payload =
            effect_payload("EFF-1", &EffectOutcome::BookingCreated(booking())).expect("encode");
        assert_eq!(payload_hex(&payload), EFFECT_GOLDEN, "the encoding changed");
    }

    /// And one for availability, because a single vector covers a single shape.
    #[test]
    fn the_availability_payload_encoding_is_pinned() {
        let payload =
            availability_payload("TH-A", "SLOT-A", Some(&availability())).expect("encode");
        assert_eq!(
            payload_hex(&payload),
            AVAILABILITY_GOLDEN,
            "the encoding changed"
        );
    }

    fn payload_hex(bytes: &[u8]) -> String {
        use core::fmt::Write as _;
        bytes.iter().fold(String::new(), |mut hex, byte| {
            let _ = write!(hex, "{byte:02x}");
            hex
        })
    }

    const EFFECT_GOLDEN: &str = "626c642e746f776e68616c6c2e636f756e63696c2e7631000101000000054546462d31010000000e426f6f6b696e6743726561746564010000000854482d3930303031010000000454482d410100000006534c4f542d4101000000000000001401000000000000119401000000046c756379";
    const AVAILABILITY_GOLDEN: &str = "626c642e746f776e68616c6c2e636f756e63696c2e76310002010000000454482d410100000006534c4f542d41010101000000000000001e0101010101000000000000119401000000054752414e5401000000003b9bb460";

    /// Round-trip through the real signer and the real verifier, for every
    /// outcome variant — so a variant added later without an encoding arm is
    /// caught here rather than in production.
    #[test]
    fn every_outcome_signs_and_verifies() {
        let signer = signer();
        let key = CouncilKey::new(signer.verifying_key());

        for outcome in [
            EffectOutcome::BookingCreated(booking()),
            EffectOutcome::CancellationApplied {
                booking_reference: "TH-90001".to_owned(),
            },
            EffectOutcome::DefinitivelyAbsent,
            EffectOutcome::ProviderRejected {
                reason: "fee disagreement".to_owned(),
            },
            EffectOutcome::NotYetVisible,
            EffectOutcome::ProtocolConflict {
                reason: "deadline mismatch".to_owned(),
            },
            EffectOutcome::Unavailable {
                reason: "busy".to_owned(),
            },
        ] {
            let response = SignedEffectResponse {
                effect_intent_id: "EFF-1".to_owned(),
                signature: Some(signer.sign_effect("EFF-1", &outcome).expect("sign")),
                outcome,
            };
            assert_eq!(key.check_effect(&response), Ok(()));
        }
    }

    /// The test that would have caught this protocol shipping without provenance
    /// at all: a response whose every field is correct, and which is refused.
    #[test]
    fn a_field_perfect_unsigned_response_is_refused() {
        let key = CouncilKey::new(signer().verifying_key());
        let response = SignedEffectResponse {
            effect_intent_id: "EFF-1".to_owned(),
            outcome: EffectOutcome::BookingCreated(booking()),
            signature: None,
        };
        assert_eq!(key.check_effect(&response), Err(WireError::Unsigned));
    }

    #[test]
    fn a_signature_from_another_key_is_refused() {
        let impostor = CouncilSigner::new(SigningKey::from_bytes(&[9u8; 32]));
        let key = CouncilKey::new(signer().verifying_key());
        let outcome = EffectOutcome::BookingCreated(booking());
        let response = SignedEffectResponse {
            effect_intent_id: "EFF-1".to_owned(),
            signature: Some(impostor.sign_effect("EFF-1", &outcome).expect("sign")),
            outcome,
        };
        assert_eq!(key.check_effect(&response), Err(WireError::BadSignature));
    }

    /// An availability signature must not verify as an effect signature. The
    /// message-type byte is what stops it.
    #[test]
    fn an_availability_signature_does_not_verify_as_an_effect() {
        let signer = signer();
        let key = CouncilKey::new(signer.verifying_key());
        let stolen = signer
            .sign_availability("TH-A", "SLOT-A", Some(&availability()))
            .expect("sign");

        let response = SignedEffectResponse {
            effect_intent_id: "EFF-1".to_owned(),
            outcome: EffectOutcome::BookingCreated(booking()),
            signature: Some(stolen),
        };
        assert_eq!(key.check_effect(&response), Err(WireError::BadSignature));
    }

    #[test]
    fn an_effect_signature_does_not_verify_as_availability() {
        let signer = signer();
        let key = CouncilKey::new(signer.verifying_key());
        let stolen = signer
            .sign_effect("EFF-1", &EffectOutcome::DefinitivelyAbsent)
            .expect("sign");

        let response = SignedAvailabilityResponse {
            venue_id: "TH-A".to_owned(),
            slot_id: "SLOT-A".to_owned(),
            facts: Some(availability()),
            signature: Some(stolen),
        };
        assert_eq!(
            key.check_availability(&response),
            Err(WireError::BadSignature)
        );
    }

    /// Every signed element, altered one at a time. One "a field was changed"
    /// test passes an implementation that signs the fee and omits the principal;
    /// which field the test happens to pick decides whether the gap is found.
    #[test]
    fn altering_any_signed_field_breaks_the_signature() {
        let signer = signer();
        let key = CouncilKey::new(signer.verifying_key());
        let genuine = signer
            .sign_effect("EFF-1", &EffectOutcome::BookingCreated(booking()))
            .expect("sign");

        let mutations: Vec<(&str, EffectOutcome)> = vec![
            (
                "booking_reference",
                EffectOutcome::BookingCreated(BookingFacts {
                    booking_reference: "TH-OTHER".to_owned(),
                    ..booking()
                }),
            ),
            (
                "venue_id",
                EffectOutcome::BookingCreated(BookingFacts {
                    venue_id: "TH-B".to_owned(),
                    ..booking()
                }),
            ),
            (
                "slot_id",
                EffectOutcome::BookingCreated(BookingFacts {
                    slot_id: "SLOT-B".to_owned(),
                    ..booking()
                }),
            ),
            (
                "attendees",
                EffectOutcome::BookingCreated(BookingFacts {
                    attendees: 21,
                    ..booking()
                }),
            ),
            (
                "fee_pence",
                EffectOutcome::BookingCreated(BookingFacts {
                    fee_pence: 1,
                    ..booking()
                }),
            ),
            (
                "principal",
                EffectOutcome::BookingCreated(BookingFacts {
                    principal: "not-lucy".to_owned(),
                    ..booking()
                }),
            ),
            ("outcome tag", EffectOutcome::DefinitivelyAbsent),
        ];

        for (field, altered) in mutations {
            let response = SignedEffectResponse {
                effect_intent_id: "EFF-1".to_owned(),
                outcome: altered,
                signature: Some(genuine.clone()),
            };
            assert_eq!(
                key.check_effect(&response),
                Err(WireError::BadSignature),
                "altering {field} did not break the signature"
            );
        }

        // And the identity itself.
        let response = SignedEffectResponse {
            effect_intent_id: "EFF-2".to_owned(),
            outcome: EffectOutcome::BookingCreated(booking()),
            signature: Some(genuine),
        };
        assert_eq!(
            key.check_effect(&response),
            Err(WireError::BadSignature),
            "altering the effect id did not break the signature"
        );
    }

    #[test]
    fn altering_any_signed_availability_field_breaks_the_signature() {
        let signer = signer();
        let key = CouncilKey::new(signer.verifying_key());
        let genuine = signer
            .sign_availability("TH-A", "SLOT-A", Some(&availability()))
            .expect("sign");

        let mutations: Vec<(&str, SignedAvailabilityResponse)> = vec![
            (
                "capacity",
                AvailabilityFacts {
                    capacity: 31,
                    ..availability()
                },
            ),
            (
                "accessible",
                AvailabilityFacts {
                    accessible: false,
                    ..availability()
                },
            ),
            (
                "available",
                AvailabilityFacts {
                    available: false,
                    ..availability()
                },
            ),
            (
                "fee_pence",
                AvailabilityFacts {
                    fee_pence: 1,
                    ..availability()
                },
            ),
            (
                "grant",
                AvailabilityFacts {
                    grant: "OTHER".to_owned(),
                    ..availability()
                },
            ),
            (
                "valid_until_ms",
                AvailabilityFacts {
                    valid_until_ms: 1,
                    ..availability()
                },
            ),
        ]
        .into_iter()
        .map(|(field, facts)| {
            (
                field,
                SignedAvailabilityResponse {
                    venue_id: "TH-A".to_owned(),
                    slot_id: "SLOT-A".to_owned(),
                    facts: Some(facts),
                    signature: Some(genuine.clone()),
                },
            )
        })
        .collect();

        for (field, response) in mutations {
            assert_eq!(
                key.check_availability(&response),
                Err(WireError::BadSignature),
                "altering {field} did not break the signature"
            );
        }
    }

    #[test]
    fn a_grant_round_trips_and_a_forged_one_does_not() {
        let signer = signer();
        let claims = GrantClaims {
            venue_id: "TH-A".to_owned(),
            slot_id: "SLOT-A".to_owned(),
            row_version: 7,
            valid_until_ms: 1_000_060_000,
        };
        let token = signer.mint_grant(&claims).expect("mint");
        assert_eq!(signer.open_grant(&token).expect("open"), claims);

        let impostor = CouncilSigner::new(SigningKey::from_bytes(&[9u8; 32]));
        let forged = impostor.mint_grant(&claims).expect("mint");
        assert_eq!(signer.open_grant(&forged), Err(WireError::BadSignature));
    }

    /// The grant is signed with the same key as the responses, so it must not be
    /// interchangeable with them.
    #[test]
    fn an_availability_signature_is_not_a_grant() {
        let signer = signer();
        let stolen = signer
            .sign_availability("TH-A", "SLOT-A", Some(&availability()))
            .expect("sign");
        let payload =
            availability_payload("TH-A", "SLOT-A", Some(&availability())).expect("encode");

        let token = format!("{}.{}", super::B64.encode(&payload), stolen);
        // Refused on the message-type byte, before any claim is read.
        assert!(matches!(
            signer.open_grant(&token),
            Err(WireError::Codec(_))
        ));
    }
}
