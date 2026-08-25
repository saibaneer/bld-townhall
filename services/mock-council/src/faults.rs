//! Armed misbehaviour, for slice E's failure-injection suite.
//!
//! Compiled only with the `test-faults` feature, which only the test harness
//! enables — an ordinary build has no fault state and no `/test/faults` route,
//! so there is nothing to reach.
//!
//! # Scoped, one-shot, and observable
//!
//! A fault is armed against a specific `(effect_intent_id, route)` and consumed
//! by the first matching request. Anything else about it would make the suite
//! timing-dependent: with two requests in flight, an unscoped fault is consumed
//! by whichever arrives first, and a pause on one lets the other steal it.
//!
//! Arming returns an id; `GET /test/faults/{id}` reports whether it fired. So a
//! test asserts *"the fault fired"* rather than inferring it from an outcome
//! that might have arisen another way — and a fault that was consumed without
//! producing its misbehaviour is caught by the paired wire assertion (gates
//! M11/M12).
//!
//! # Unavailability is not a fault here, and cannot be
//!
//! Refusing a connection happens before any byte of the request is readable, so
//! there is no route and no identity to scope an "outage" fault by. The suite
//! produces unavailability the honest way instead: it kills the council process
//! and talks to the dead socket (test 7 in
//! `council-client/tests/reconciliation.rs`).

use serde::{Deserialize, Serialize};
use std::{
    collections::HashMap,
    sync::{
        Mutex,
        atomic::{AtomicU64, Ordering},
    },
};

/// Which request path a fault is armed against.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Route {
    Create,
    Cancel,
    Resolve,
}

/// What the council should do to the matching request.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "fault", rename_all = "snake_case")]
pub enum Fault {
    /// Handle the request completely — the commit happens — then close the
    /// connection without a readable answer. The dropped-response scenario:
    /// anything that skipped the commit would be testing something easier.
    DropResponse,
    /// Answer late. Meaningful only against a client with an injected timeout,
    /// which turns lateness into `Unknown` — the honest classification, since a
    /// timeout says nothing about whether the effect happened.
    Delay { ms: u64 },
    /// Answer with bytes that are not the protocol.
    Garbage,
    /// Answer correctly, minus the signature. Field-perfect and unattributable,
    /// which the verifier must refuse.
    Unsigned,
    /// Answer a request with a *signed* `BookingCreated` regardless of what was
    /// asked — the wrong-kind fact test 2c needs, unreachable over the honest
    /// protocol because the real council refuses kind mismatches before
    /// answering.
    WrongKind,
}

#[derive(Debug)]
struct Armed {
    effect_intent_id: String,
    route: Route,
    fault: Fault,
    remaining: u64,
    consumed: u64,
}

/// The armed faults, keyed by the id arming returned.
#[derive(Debug, Default)]
pub struct FaultBank {
    next_id: AtomicU64,
    armed: Mutex<HashMap<u64, Armed>>,
}

#[derive(Debug, Deserialize)]
pub struct ArmRequest {
    pub effect_intent_id: String,
    pub route: Route,
    #[serde(flatten)]
    pub fault: Fault,
    #[serde(default = "one")]
    pub uses: u64,
}

const fn one() -> u64 {
    1
}

#[derive(Debug, Serialize)]
pub struct FaultStatus {
    pub id: u64,
    pub consumed: u64,
    pub remaining: u64,
}

impl FaultBank {
    /// Arm one fault; returns the id a test polls for consumption.
    pub fn arm(&self, request: ArmRequest) -> u64 {
        let id = self.next_id.fetch_add(1, Ordering::SeqCst);
        self.lock().insert(
            id,
            Armed {
                effect_intent_id: request.effect_intent_id,
                route: request.route,
                fault: request.fault,
                remaining: request.uses,
                consumed: 0,
            },
        );
        id
    }

    pub fn status(&self, id: u64) -> Option<FaultStatus> {
        self.lock().get(&id).map(|armed| FaultStatus {
            id,
            consumed: armed.consumed,
            remaining: armed.remaining,
        })
    }

    /// The fault matching this request, consuming one use. `None` is the
    /// ordinary case and means "behave".
    pub fn consume(&self, effect_intent_id: &str, route: Route) -> Option<Fault> {
        let mut armed = self.lock();
        let matched = armed.values_mut().find(|candidate| {
            candidate.remaining > 0
                && candidate.route == route
                && candidate.effect_intent_id == effect_intent_id
        })?;
        matched.remaining -= 1;
        matched.consumed += 1;
        Some(matched.fault.clone())
    }

    fn lock(&self) -> std::sync::MutexGuard<'_, HashMap<u64, Armed>> {
        self.armed
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
    }
}

#[cfg(test)]
mod tests {
    use super::{ArmRequest, Fault, FaultBank, Route};

    #[test]
    fn a_fault_matches_only_its_identity_and_route_and_is_one_shot() {
        let bank = FaultBank::default();
        let id = bank.arm(ArmRequest {
            effect_intent_id: "EFF-1".to_owned(),
            route: Route::Create,
            fault: Fault::DropResponse,
            uses: 1,
        });

        assert_eq!(bank.consume("EFF-2", Route::Create), None, "wrong identity");
        assert_eq!(bank.consume("EFF-1", Route::Resolve), None, "wrong route");
        assert_eq!(
            bank.consume("EFF-1", Route::Create),
            Some(Fault::DropResponse)
        );
        assert_eq!(
            bank.consume("EFF-1", Route::Create),
            None,
            "one-shot: the second matching request is served honestly"
        );

        let status = bank.status(id).expect("armed");
        assert_eq!((status.consumed, status.remaining), (1, 0));
    }
}
