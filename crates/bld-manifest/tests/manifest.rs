//! The manifest's crypto contract: a signed manifest verifies, and every tamper
//! — of the core, of the digest field, of the signature, or of the key — is
//! rejected. Each test fails a specific wrong `verify` (never-fake-tests).

use bld_manifest::{BehaviourLink, Manifest, ResourceLink, SigningKey};
use std::collections::BTreeMap;

fn sample() -> Manifest {
    let mut behaviours = BTreeMap::new();
    behaviours.insert(
        "Book".to_owned(),
        BehaviourLink {
            segment: "book".to_owned(),
            body: vec![],
        },
    );
    let mut resource_links = BTreeMap::new();
    resource_links.insert(
        "booking-intents".to_owned(),
        ResourceLink {
            collection: "/booking-intents".to_owned(),
            item: "/booking-intents/{id}".to_owned(),
            behaviour_template: "/booking-intents/{id}/behaviours/{segment}".to_owned(),
            behaviours,
        },
    );
    Manifest {
        bld_version: "0.2".to_owned(),
        service: "demo-town-hall-booking".to_owned(),
        publisher: "demo-council".to_owned(),
        resources: vec!["booking-intents".to_owned()],
        concurrency: "etag-if-match".to_owned(),
        authority_profile: "bld-demo-delegation-v1".to_owned(),
        resource_links,
    }
}

fn key(seed: u8) -> SigningKey {
    SigningKey::from_bytes(&[seed; 32])
}

#[test]
fn a_signed_manifest_verifies_against_its_publisher() {
    let signing = key(7);
    let signed = sample().sign(&signing).expect("sign");
    signed
        .verify(&signing.verifying_key())
        .expect("its own publisher verifies it");
    // The digest field really is the digest of the core.
    assert_eq!(signed.manifest_digest, sample().digest().unwrap());
}

#[test]
fn a_tampered_core_is_rejected() {
    let signing = key(7);
    let mut signed = sample().sign(&signing).expect("sign");
    // Alter a core field WITHOUT re-signing — the signature no longer covers it.
    // (The digest field is now also stale, so this trips the integrity check
    // first; either way it must not verify.)
    signed.manifest.service = "impostor-service".to_owned();
    assert!(
        signed.verify(&signing.verifying_key()).is_err(),
        "an altered core must not verify"
    );
}

#[test]
fn a_tampered_digest_is_rejected() {
    let signing = key(7);
    let mut signed = sample().sign(&signing).expect("sign");
    signed.manifest_digest = "0000000000000000000000000000000000000000000".to_owned();
    assert_eq!(
        signed.verify(&signing.verifying_key()),
        Err(bld_manifest::ManifestError::DigestMismatch),
        "a digest that does not describe the core is rejected"
    );
}

#[test]
fn a_core_re_tampered_with_a_matching_digest_still_fails_the_signature() {
    // The sharp case: an attacker edits the core AND recomputes a matching digest,
    // so the integrity check passes — the SIGNATURE is what refuses them, because
    // they cannot re-sign without the publisher key.
    let signing = key(7);
    let mut signed = sample().sign(&signing).expect("sign");
    signed.manifest.publisher = "attacker".to_owned();
    signed.manifest_digest = signed.manifest.digest().unwrap(); // matches the tampered core
    assert_eq!(
        signed.verify(&signing.verifying_key()),
        Err(bld_manifest::ManifestError::BadSignature),
        "a re-digested tamper is caught by the signature, not the digest"
    );
}

#[test]
fn an_impostor_key_does_not_verify() {
    let signed = sample().sign(&key(7)).expect("sign");
    assert_eq!(
        signed.verify(&key(9).verifying_key()),
        Err(bld_manifest::ManifestError::BadSignature),
        "only the pinned publisher's key verifies"
    );
}

#[test]
fn a_malformed_signature_is_rejected_not_panicked() {
    let signing = key(7);
    let mut signed = sample().sign(&signing).expect("sign");
    signed.manifest_signature = "not-base-64-!!".to_owned();
    assert_eq!(
        signed.verify(&signing.verifying_key()),
        Err(bld_manifest::ManifestError::MalformedSignature),
    );
}
