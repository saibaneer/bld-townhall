//! Formal-verification proof harnesses for the kernel's sequencing invariant
//! (a proof-of-concept, ADR-001 / M0), checked by **Kani** — a bit-precise model
//! checker (CBMC). Compiled ONLY under `cfg(kani)`, so a normal `cargo build`,
//! `clippy`, or `cargo test` never sees this module.
//!
//! Run it with:
//!
//! ```text
//! cargo install --locked kani-verifier && cargo kani-setup
//! cargo kani -p bld-kernel
//! ```
//!
//! Where the M0 acceptance test asserts the invariant for a handful of fixtures,
//! these harnesses assert it for EVERY possible input: `kani::any()` yields a
//! symbolic value and Kani proves the assertion cannot fail for any concrete
//! choice. The proof runs against the real [`Resolution`] / [`BoundaryOutcome`]
//! types and their real methods — only `commit_step` is a model, and it models the
//! single decision the invariant is about (the coordinator's async CAS is
//! orthogonal to *which resolutions may commit at all*).

#![cfg(kani)]

use crate::{BoundaryOutcome, Resolution};

/// The commit DECISION, as the coordinator applies it: a `Ready` resolution's plan
/// is committed; a `Denied` stays denied; an `Undefined` stays undefined. This is
/// the pure core of the sequencing rule — nothing here may turn a non-`Ready`
/// resolution into a committed state.
fn commit_step(resolution: Resolution<u8, u8>) -> BoundaryOutcome<u8, u8> {
    match resolution {
        Resolution::Ready(plan) => BoundaryOutcome::Committed(plan),
        Resolution::Denied(error) => BoundaryOutcome::Denied(error),
        Resolution::Undefined => BoundaryOutcome::Undefined,
    }
}

/// A symbolic resolution ranging over every variant and every payload value.
fn any_resolution() -> Resolution<u8, u8> {
    match kani::any::<u8>() % 3 {
        0 => Resolution::Undefined,
        1 => Resolution::Denied(kani::any()),
        _ => Resolution::Ready(kani::any()),
    }
}

/// THE sequencing invariant: a stage that did not resolve `Ready` can never
/// commit. Proven for EVERY possible resolution — Kani explores all variants and
/// all payload bytes and shows the assertion cannot fail.
#[kani::proof]
fn a_non_ready_stage_never_commits() {
    let resolution = any_resolution();
    let was_ready = resolution.is_ready();
    let outcome = commit_step(resolution);
    if outcome.committed().is_some() {
        assert!(
            was_ready,
            "a committed outcome must have come from a Ready resolution"
        );
    }
}

/// The mirror: a `Ready` resolution is the ONLY thing that commits — the other two
/// arms never produce a `Committed`.
#[kani::proof]
fn only_ready_commits() {
    let resolution = any_resolution();
    let ready = resolution.is_ready();
    let committed = commit_step(resolution).committed().is_some();
    assert_eq!(
        committed, ready,
        "commit happens exactly when, and only when, Ready"
    );
}

/// The real `From<Result>` conversion preserves the trichotomy: `Ok` becomes
/// `Ready`, `Err` becomes `Denied`, and it NEVER fabricates an `Undefined` — so a
/// domain that returns a `Result` can never accidentally make a behaviour vanish.
#[kani::proof]
fn from_result_preserves_the_trichotomy() {
    let result: Result<u8, u8> = if kani::any() {
        Ok(kani::any())
    } else {
        Err(kani::any())
    };
    let was_ok = result.is_ok();
    let resolution = Resolution::from(result);
    assert_eq!(resolution.is_ready(), was_ok);
    assert!(
        !resolution.is_undefined(),
        "a converted Result is Ready or Denied — never Undefined"
    );
}
