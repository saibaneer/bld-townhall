//! The dispatcher's ports — every question it can ask, as a trait someone else
//! answers.
//!
//! Each of these is a seam a later milestone fills properly: M7 replaces the
//! credential source with the approval flow's issuer, M8 gives the balance port
//! a ledger, M11 puts a model behind the proposer. M6 supplies the narrow,
//! honest versions — and the tests supply hostile ones, which is why these are
//! traits and not functions.

use async_trait::async_trait;
use bld_types::{BookingId, BookingRequirements, CouncilBookingRef, PrincipalId};
use townhall_channel::{ChannelAddress, InboundMessage};
use townhall_gateway::{Gateway, GatewayError, Projection, Turn, VenueRow};

/// Which principal an address is bound to, if any.
///
/// Spec Appendix C: the phone number is a channel binding, never the identity
/// key. `None` is a first-class answer — an unbound address is refused, not
/// promoted to an implicit new principal.
pub trait PrincipalDirectory: Send + Sync {
    fn resolve(&self, address: &ChannelAddress) -> Option<PrincipalId>;
}

/// The bearer token a principal's requests carry.
///
/// **Not** an authority resolver — ADR-021 gives the server the only one of
/// those. This supplies a credential the SERVER then resolves, and its M6
/// implementation can only hand out the dev allowlist's fixed tokens, so it
/// cannot widen authority even in principle. M7 replaces it with the approval
/// flow's issued grants.
pub trait CredentialSource: Send + Sync {
    fn token_for(&self, principal: &PrincipalId) -> Option<String>;
}

/// What BALANCE answers.
///
/// Deliberately no debit method: zero-cost safety commands are structural here,
/// not remembered. M8's ledger implements this with real numbers; M6's honest
/// answer is that there are none yet.
pub trait UsageBalance: Send + Sync {
    fn describe(&self, principal: &PrincipalId) -> String;
}

/// M6's balance: no ledger exists, and inventing a number a person could act on
/// would be worse than saying so.
#[derive(Debug, Default)]
pub struct NoLedgerYet;

impl UsageBalance for NoLedgerYet {
    fn describe(&self, _principal: &PrincipalId) -> String {
        "Balance unavailable until usage accounting is enabled. This command costs zero.".to_owned()
    }
}

/// What a freeform message amounted to, in the proposer's judgment.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Proposed {
    Typed(Request),
    /// A first-class answer, not a failure: the dispatcher replies with a
    /// pointer to HELP and submits nothing.
    Unclear,
}

/// The typed requests M6's conversations can express.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Request {
    /// Start a booking: create the intent and walk it to `AwaitingBooking`.
    Book(BookingRequest),
    /// Finish the most recent booking: walk it from wherever it stands to
    /// `Book`. (M7 replaces the bare word with its challenge flow — the M6
    /// trigger is deliberately a stand-in, and says so.)
    Confirm,
    /// "Cancel it" — a cancellation with the referent left to authoritative
    /// resolution, where ambiguity can be ASKED about (spec §14.1).
    CancelIntent,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct BookingRequest {
    pub date: String,
    pub from: String,
    pub to: String,
    pub people: u16,
    pub accessible: bool,
    pub max_pence: u64,
}

impl BookingRequest {
    #[must_use]
    pub fn requirements(&self) -> BookingRequirements {
        BookingRequirements {
            purpose: "town hall booking".to_owned(),
            requested_date: self.date.clone(),
            time_window: bld_types::TimeWindow {
                from: self.from.clone(),
                to: self.to.clone(),
            },
            attendees: self.people,
            wheelchair_accessible: self.accessible,
            max_fee: bld_types::Money::from_pence(self.max_pence),
        }
    }
}

/// What the proposer is allowed to see: a projection, never a capability.
///
/// Spec §3.2 grades the proposer untrusted; it reads this and returns a typed
/// request, and everything it returns is re-checked downstream by the boundary.
#[derive(Clone, Debug, Default)]
pub struct ProjectedContext {
    /// The caller's currently cancellable bookings, for referent talk.
    pub cancellable: Vec<CandidateSummary>,
}

#[derive(Clone, Debug)]
pub struct CandidateSummary {
    pub id: BookingId,
    pub reference: Option<CouncilBookingRef>,
    pub state: String,
}

/// The probabilistic seat. M6's occupant is a strict grammar; M11's is a model.
///
/// Either way the shape holds: projected context in, typed request out, and no
/// route to anything that could act — this trait's implementors receive no
/// gateway, no channel, no port.
#[async_trait]
pub trait Proposer: Send + Sync {
    async fn propose(&self, context: &ProjectedContext, message: &InboundMessage) -> Proposed;
}

/// The wire, as the dispatcher sees it — an abstraction over [`Gateway`] so
/// tests can put a counting or panicking wire in its place.
///
/// The methods are exactly the gateway's; this trait adds nothing but the seam.
#[async_trait]
pub trait BookingWire: Send + Sync {
    async fn create(
        &self,
        id: &BookingId,
        requirements: &BookingRequirements,
    ) -> Result<Projection, GatewayError>;
    async fn read(&self, id: &BookingId) -> Result<Projection, GatewayError>;
    async fn cancellable(&self) -> Result<Vec<Projection>, GatewayError>;
    async fn by_reference(
        &self,
        reference: &CouncilBookingRef,
    ) -> Result<Vec<Projection>, GatewayError>;
    async fn venues(&self) -> Result<Vec<VenueRow>, GatewayError>;
    async fn propose_at(
        &self,
        id: &BookingId,
        expected_version: u64,
        behaviour: &str,
        body: Option<serde_json::Value>,
    ) -> Result<Turn, GatewayError>;
    async fn converge(
        &self,
        id: &BookingId,
        first_wait: std::time::Duration,
    ) -> Result<Projection, GatewayError>;
}

#[async_trait]
impl BookingWire for Gateway {
    async fn create(
        &self,
        id: &BookingId,
        requirements: &BookingRequirements,
    ) -> Result<Projection, GatewayError> {
        Gateway::create(self, id, requirements).await
    }
    async fn read(&self, id: &BookingId) -> Result<Projection, GatewayError> {
        Gateway::read(self, id).await
    }
    async fn cancellable(&self) -> Result<Vec<Projection>, GatewayError> {
        Gateway::cancellable(self).await
    }
    async fn by_reference(
        &self,
        reference: &CouncilBookingRef,
    ) -> Result<Vec<Projection>, GatewayError> {
        Gateway::by_reference(self, reference).await
    }
    async fn venues(&self) -> Result<Vec<VenueRow>, GatewayError> {
        Gateway::venues(self).await
    }
    async fn propose_at(
        &self,
        id: &BookingId,
        expected_version: u64,
        behaviour: &str,
        body: Option<serde_json::Value>,
    ) -> Result<Turn, GatewayError> {
        Gateway::propose_at(self, id, expected_version, behaviour, body).await
    }
    async fn converge(
        &self,
        id: &BookingId,
        first_wait: std::time::Duration,
    ) -> Result<Projection, GatewayError> {
        Gateway::converge(self, id, first_wait).await
    }
}

/// Builds the wire a principal's requests travel on.
///
/// A factory rather than one shared wire because the credential differs per
/// principal — and because tests inject wires that count or panic, which is how
/// "the control commands reach nothing" becomes an assertion instead of a hope.
pub trait WireFactory: Send + Sync {
    /// A wire that may READ this principal's bookings and change nothing.
    ///
    /// The principal is not the credential. M7B's header split made that
    /// concrete on the wire: `Authorization` says which workload is calling and
    /// `X-BLD-Principal` says whose bookings are in scope, because a stolen
    /// workload credential must not become a licence to read everyone's.
    fn reader_for(&self, token: &str, principal: &PrincipalId) -> std::sync::Arc<dyn BookingWire>;

    /// A wire that may CHANGE one booking, presenting `reference`.
    ///
    /// # Why this is a second method rather than an argument
    ///
    /// Because reading and changing need different things, and a single
    /// constructor taking an `Option<reference>` would make "no grant" a
    /// forgettable default rather than a different kind of wire. A caller that
    /// only holds a reader cannot mutate however it is written — the server
    /// refuses a change with no delegation header — and that is a property of
    /// which method was called, not of a flag somebody remembered to set.
    ///
    /// Lucy's conversation needs both, in this order: read her bookings to find
    /// out what "cancel it" means, THEN change the one she meant (spec §23.1).
    fn changer_for(
        &self,
        token: &str,
        principal: &PrincipalId,
        reference: &str,
    ) -> std::sync::Arc<dyn BookingWire>;
}

/// The production factory: a [`Gateway`] per token, against one base URL.
pub struct GatewayFactory {
    pub base: String,
}

impl WireFactory for GatewayFactory {
    fn reader_for(&self, token: &str, principal: &PrincipalId) -> std::sync::Arc<dyn BookingWire> {
        // No delegation reference, so every change this wire attempts is a 401
        // — spec §23.1's ordering enforced by the server rather than remembered
        // by the client.
        std::sync::Arc::new(Gateway::new(self.base.clone(), token, principal.as_str()))
    }

    fn changer_for(
        &self,
        token: &str,
        principal: &PrincipalId,
        reference: &str,
    ) -> std::sync::Arc<dyn BookingWire> {
        std::sync::Arc::new(
            Gateway::new(self.base.clone(), token, principal.as_str()).with_delegation(reference),
        )
    }
}
