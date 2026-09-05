#![forbid(unsafe_code)]

//! The BLD discovery manifest (M9, spec §12; ADR-029).
//!
//! A service publishes this, signed, at `GET /.well-known/bld`; an untrusted
//! client fetches it, verifies it against a pinned publisher key, and drives the
//! service's API from it — resource bases and behaviour segments come from here,
//! never hard-coded (the gate: "no hard-coded behaviour URLs beyond bootstrap").
//!
//! # Why the signature is over the CORE bytes, not just the digest
//!
//! `manifest_digest` is integrity (did the bytes arrive intact); the signature is
//! authenticity (did the pinned publisher produce them). Signing the canonical
//! CORE serialization directly — not merely a digest string — leaves no gap where
//! a tampered core could ride a stale digest: verification recomputes the core
//! bytes, checks the digest against them, AND verifies the signature over them.
//!
//! # Why ed25519, not an HMAC
//!
//! Two parties: a publisher signs, a distinct client verifies, and the manifest
//! is relayed and listed in a local catalogue. That is the codebase's rule for a
//! signature over a MAC (a MAC is for one party that both writes and reads).

use base64::Engine as _;
use ed25519_dalek::{Signer as _, Verifier as _};
use serde::{Deserialize, Serialize};
use sha2::{Digest as _, Sha256};
use std::collections::BTreeMap;

pub use ed25519_dalek::{SigningKey, VerifyingKey};

const B64: base64::engine::general_purpose::GeneralPurpose =
    base64::engine::general_purpose::STANDARD_NO_PAD;

/// The signed, transport-shaped manifest — what `/.well-known/bld` returns and a
/// catalogue lists. The core fields are flattened to the top level (§12's shape),
/// with the digest and signature beside them.
#[derive(Clone, Debug, Serialize, Deserialize, PartialEq, Eq)]
pub struct SignedManifest {
    #[serde(flatten)]
    pub manifest: Manifest,
    /// `base64(sha256(canonical core bytes))` — integrity.
    pub manifest_digest: String,
    /// `base64(ed25519_sign(publisher key, canonical core bytes))` — authenticity.
    pub manifest_signature: String,
}

/// The manifest's CORE — the fields that are digested and signed. Field order is
/// fixed by the struct, and serde serializes a struct deterministically, so the
/// canonical bytes are stable across signer and verifier.
#[derive(Clone, Debug, Serialize, Deserialize, PartialEq, Eq)]
pub struct Manifest {
    pub bld_version: String,
    pub service: String,
    pub publisher: String,
    pub resources: Vec<String>,
    pub concurrency: String,
    pub authority_profile: String,
    /// Per-resource paths + the behaviour name→segment table (M9 extension). A
    /// `BTreeMap` so serialization is order-stable (canonicalization depends on it).
    pub resource_links: BTreeMap<String, ResourceLink>,
}

/// How one resource is addressed and driven.
#[derive(Clone, Debug, Serialize, Deserialize, PartialEq, Eq)]
pub struct ResourceLink {
    pub collection: String,
    pub item: String,
    /// The URL template a behaviour is driven by, e.g.
    /// `/booking-intents/{id}/behaviours/{segment}`.
    pub behaviour_template: String,
    /// Discovery name (the `PascalCase` a projection publishes) -> its wire link.
    pub behaviours: BTreeMap<String, BehaviourLink>,
}

/// The wire link for one behaviour: the kebab segment the route matches, and the
/// body fields it expects (a hint, so a client assembles a body without hard-coding).
#[derive(Clone, Debug, Serialize, Deserialize, PartialEq, Eq)]
pub struct BehaviourLink {
    pub segment: String,
    #[serde(default)]
    pub body: Vec<String>,
}

#[derive(Debug, thiserror::Error, PartialEq, Eq)]
pub enum ManifestError {
    /// The recomputed digest does not match `manifest_digest` — the core bytes
    /// were altered (or the digest field was).
    #[error("manifest digest does not match its core")]
    DigestMismatch,
    /// The signature is malformed (not base64, or not 64 bytes).
    #[error("manifest signature is malformed")]
    MalformedSignature,
    /// The signature is well-formed but not this publisher's over these bytes.
    #[error("manifest signature does not verify against the publisher key")]
    BadSignature,
    /// The core could not be serialized to canonical bytes.
    #[error("manifest could not be canonicalized")]
    Uncanonical,
}

impl Manifest {
    /// The canonical bytes that are digested and signed — the serialized core.
    ///
    /// # Errors
    /// Serialization failed (not reachable for this struct, but surfaced rather
    /// than panicked).
    pub fn canonical_bytes(&self) -> Result<Vec<u8>, ManifestError> {
        serde_json::to_vec(self).map_err(|_| ManifestError::Uncanonical)
    }

    /// `base64(sha256(canonical bytes))`.
    ///
    /// # Errors
    /// The core could not be canonicalized.
    pub fn digest(&self) -> Result<String, ManifestError> {
        Ok(B64.encode(Sha256::digest(self.canonical_bytes()?)))
    }

    /// Sign this manifest with the publisher's key, producing the served form.
    ///
    /// # Errors
    /// The core could not be canonicalized.
    pub fn sign(self, key: &SigningKey) -> Result<SignedManifest, ManifestError> {
        let bytes = self.canonical_bytes()?;
        let digest = B64.encode(Sha256::digest(&bytes));
        let signature = B64.encode(key.sign(&bytes).to_bytes());
        Ok(SignedManifest {
            manifest: self,
            manifest_digest: digest,
            manifest_signature: signature,
        })
    }
}

impl SignedManifest {
    /// Verify integrity AND authenticity against a pinned publisher key. Both are
    /// checked before a client may trust a manifest to drive from.
    ///
    /// # Errors
    /// [`ManifestError::DigestMismatch`] if the digest does not match the core,
    /// [`ManifestError::MalformedSignature`] / [`ManifestError::BadSignature`] if
    /// the signature is malformed or not the publisher's.
    pub fn verify(&self, publisher: &VerifyingKey) -> Result<(), ManifestError> {
        let bytes = self.manifest.canonical_bytes()?;
        // Integrity: the digest field must describe the core we actually received.
        if B64.encode(Sha256::digest(&bytes)) != self.manifest_digest {
            return Err(ManifestError::DigestMismatch);
        }
        // Authenticity: the signature must be the publisher's over those bytes.
        let raw = B64
            .decode(&self.manifest_signature)
            .map_err(|_| ManifestError::MalformedSignature)?;
        let sig_bytes: [u8; 64] = raw
            .try_into()
            .map_err(|_| ManifestError::MalformedSignature)?;
        let signature = ed25519_dalek::Signature::from_bytes(&sig_bytes);
        publisher
            .verify(&bytes, &signature)
            .map_err(|_| ManifestError::BadSignature)
    }
}

/// Build an ed25519 verifying key from 32 hex bytes (64 hex chars) — how a
/// catalogue entry pins a publisher.
#[must_use]
pub fn verifying_key_from_hex(hex: &str) -> Option<VerifyingKey> {
    let bytes = decode_hex_32(hex)?;
    VerifyingKey::from_bytes(&bytes).ok()
}

/// Build an ed25519 signing key from 32 hex bytes — how the composition root
/// provisions the publisher key from `--manifest-key`.
#[must_use]
pub fn signing_key_from_hex(hex: &str) -> Option<SigningKey> {
    Some(SigningKey::from_bytes(&decode_hex_32(hex)?))
}

fn decode_hex_32(hex: &str) -> Option<[u8; 32]> {
    if hex.len() != 64 {
        return None;
    }
    let mut out = [0u8; 32];
    for (i, byte) in out.iter_mut().enumerate() {
        *byte = u8::from_str_radix(hex.get(2 * i..2 * i + 2)?, 16).ok()?;
    }
    Some(out)
}
