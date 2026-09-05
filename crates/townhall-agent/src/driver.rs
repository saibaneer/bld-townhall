//! The driver (M11, ADR-031): it carries messages between a [`Proposer`] and the
//! boundary, and decides NOTHING itself.
//!
//! Each turn it reads the authoritative projection through `bld-client`, hands the
//! proposer that projected view, submits whatever the proposer returns, and
//! records the boundary's answer. The boundary decides every step; the driver is
//! a courier. That is why the same driver runs a helpful LLM proposer and a
//! deterministic [`crate::hostile::HostileProposer`] without change — and why the
//! adversarial suite's claims are about the boundary, not the courier.

use crate::{ProjectedContext, ProposedAction, Proposer, VenueOption};
use bld_client::{BldClient, ClientError, Fetched};

/// The resource the manifest publishes; the driver spells it once, here.
const RESOURCE: &str = "booking-intents";
/// The read-only venue browse surface (spec §18.3).
const VENUES_PATH: &str = "/venues";

/// One step of a journey: what a proposer proposed, and what the boundary did.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Step {
    pub action: ProposedAction,
    pub outcome: Outcome,
}

/// What the boundary answered to one submitted action.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Outcome {
    /// The boundary accepted it; the booking is now in `state`.
    Committed { state: String, version: Option<u64> },
    /// The boundary refused it — the whole point of the adversarial suite.
    Refused { status: u16, detail: String },
}

/// The record of one run: every proposal and the boundary's answer to it.
#[derive(Clone, Debug, Default)]
pub struct Journey {
    pub steps: Vec<Step>,
}

impl Journey {
    /// The last state the boundary actually committed, if any.
    #[must_use]
    pub fn final_state(&self) -> Option<&str> {
        self.steps
            .iter()
            .rev()
            .find_map(|step| match &step.outcome {
                Outcome::Committed { state, .. } => Some(state.as_str()),
                Outcome::Refused { .. } => None,
            })
    }

    /// Whether the boundary ever committed this state during the run.
    #[must_use]
    pub fn reached(&self, state: &str) -> bool {
        self.steps.iter().any(
            |step| matches!(&step.outcome, Outcome::Committed { state: seen, .. } if seen == state),
        )
    }

    /// Every refusal the boundary returned — `(status, detail)`.
    #[must_use]
    pub fn refusals(&self) -> Vec<(u16, &str)> {
        self.steps
            .iter()
            .filter_map(|step| match &step.outcome {
                Outcome::Refused { status, detail } => Some((*status, detail.as_str())),
                Outcome::Committed { .. } => None,
            })
            .collect()
    }
}

/// Runs a proposer against one discovered service.
pub struct Driver<'a> {
    client: &'a BldClient,
}

impl<'a> Driver<'a> {
    #[must_use]
    pub const fn new(client: &'a BldClient) -> Self {
        Self { client }
    }

    /// Browse the read-only venue surface into typed candidates. Best-effort: if
    /// the browse fails, a proposer simply sees no candidates.
    async fn venues(&self) -> Vec<VenueOption> {
        let Ok(json) = self.client.browse(VENUES_PATH).await else {
            return Vec::new();
        };
        json.get("venues")
            .and_then(serde_json::Value::as_array)
            .map(|rows| rows.iter().filter_map(parse_venue).collect())
            .unwrap_or_default()
    }

    /// Run one proposer through a booking journey for `id`, up to `max_steps`.
    /// Reads the projection, hands it to the proposer, submits, and records the
    /// answer — until the proposer is `Done`, a read fails, or the budget runs out.
    pub async fn run(
        &self,
        proposer: &dyn Proposer,
        request: &str,
        id: &str,
        max_steps: usize,
    ) -> Journey {
        let venues = self.venues().await;
        let mut journey = Journey::default();

        for _ in 0..max_steps {
            // The authoritative projection, or a note that the booking is not there
            // yet (a 404 before creation).
            let context = match self.client.read(RESOURCE, id).await {
                Ok(Fetched {
                    state,
                    available_behaviours,
                    version,
                }) => ProjectedContext {
                    request: request.to_owned(),
                    state: Some(state),
                    available_behaviours,
                    version,
                    venues: venues.clone(),
                },
                Err(ClientError::Refused { status: 404, .. }) => ProjectedContext {
                    request: request.to_owned(),
                    state: None,
                    available_behaviours: Vec::new(),
                    version: None,
                    venues: venues.clone(),
                },
                // Transport or another error — stop the journey rather than guess.
                Err(_) => break,
            };

            match proposer.propose(&context).await {
                ProposedAction::Done => break,
                ProposedAction::Create { mut body } => {
                    // The driver assigns the routing id (non-authoritative); the
                    // proposer supplies only the requirements.
                    if let serde_json::Value::Object(map) = &mut body {
                        map.insert("id".to_owned(), serde_json::Value::String(id.to_owned()));
                    }
                    let outcome = apply(self.client.create(RESOURCE, body.clone()).await);
                    journey.steps.push(Step {
                        action: ProposedAction::Create { body },
                        outcome,
                    });
                }
                ProposedAction::Drive {
                    behaviour,
                    body,
                    if_match,
                } => {
                    let outcome = apply(
                        self.client
                            .drive(RESOURCE, id, &behaviour, body.clone(), if_match)
                            .await,
                    );
                    journey.steps.push(Step {
                        action: ProposedAction::Drive {
                            behaviour,
                            body,
                            if_match,
                        },
                        outcome,
                    });
                }
            }
        }
        journey
    }
}

fn apply(result: Result<Fetched, ClientError>) -> Outcome {
    match result {
        Ok(Fetched { state, version, .. }) => Outcome::Committed { state, version },
        Err(ClientError::Refused { status, detail }) => Outcome::Refused { status, detail },
        // A transport failure is recorded as a refusal with status 0, so a journey
        // never silently swallows it.
        Err(other) => Outcome::Refused {
            status: 0,
            detail: other.to_string(),
        },
    }
}

fn parse_venue(row: &serde_json::Value) -> Option<VenueOption> {
    Some(VenueOption {
        venue_id: row.get("venue_id")?.as_str()?.to_owned(),
        slot_id: row.get("slot_id")?.as_str()?.to_owned(),
        fee_pence: row
            .get("fee_pence")
            .and_then(serde_json::Value::as_u64)
            .unwrap_or(0),
        accessible: row
            .get("accessible")
            .and_then(serde_json::Value::as_bool)
            .unwrap_or(false),
        capacity: row
            .get("capacity")
            .and_then(serde_json::Value::as_u64)
            .and_then(|c| u16::try_from(c).ok())
            .unwrap_or(0),
    })
}
