//! Scoped, one-shot HTTP faults for hermetic adapter tests.

use serde::{Deserialize, Serialize};
use std::{
    collections::HashMap,
    sync::{
        Mutex,
        atomic::{AtomicU64, Ordering},
    },
};

#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Route {
    Create,
    Retrieve,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "fault", rename_all = "snake_case")]
pub enum Fault {
    DropResponse,
    Delay { ms: u64 },
    Garbage,
}

#[derive(Debug)]
struct Armed {
    key: String,
    route: Route,
    fault: Fault,
    remaining: u64,
    consumed: u64,
}

#[derive(Debug, Default)]
pub struct FaultBank {
    next_id: AtomicU64,
    armed: Mutex<HashMap<u64, Armed>>,
}

#[derive(Debug, Deserialize)]
pub struct ArmRequest {
    pub key: String,
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
    pub fn arm(&self, request: ArmRequest) -> u64 {
        let id = self.next_id.fetch_add(1, Ordering::SeqCst);
        self.lock().insert(
            id,
            Armed {
                key: request.key,
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

    pub fn consume(&self, key: &str, route: Route) -> Option<Fault> {
        let mut armed = self.lock();
        let matched = armed.values_mut().find(|candidate| {
            candidate.remaining > 0 && candidate.route == route && candidate.key == key
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
    fn faults_are_scoped_and_one_shot() {
        let bank = FaultBank::default();
        let id = bank.arm(ArmRequest {
            key: "EFF-1".to_owned(),
            route: Route::Create,
            fault: Fault::DropResponse,
            uses: 1,
        });

        assert_eq!(bank.consume("EFF-2", Route::Create), None);
        assert_eq!(bank.consume("EFF-1", Route::Retrieve), None);
        assert_eq!(
            bank.consume("EFF-1", Route::Create),
            Some(Fault::DropResponse)
        );
        assert_eq!(bank.consume("EFF-1", Route::Create), None);
        let status = bank.status(id).expect("armed");
        assert_eq!((status.consumed, status.remaining), (1, 0));
    }
}
