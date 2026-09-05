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
use serde::{Deserialize, Serialize};
use townhall_channel::{ChannelAddress, Utterance};
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

/// The zero-price usage meter, as the dispatcher reaches it (M8, ADR-027).
///
/// Every method takes the inbound's transport EVIDENCE — never a principal or a
/// unit count. The server derives the principal (resolving the sender to a live
/// binding), the intent (from the transport triple) and the unit cost (from its
/// own `PricingSchedule`), so a compromised dispatcher can meter only turns it
/// can present transport evidence for. This is the `/revocations` anti-forgery
/// property, applied to the meter: the caller names nothing load-bearing. A unit
/// is £0 and grants no authority.
///
/// `reserve` is the quota gate and runs BEFORE the proposer; `debit` settles a
/// turn that produced a chargeable request; `release` rescinds — on an unclear
/// turn, a failure before consumption, or a turn the proposer resolved to a
/// zero-unit action (a cancellation, §16.2). `debit`/`release` are best-effort
/// and idempotent, so they return nothing.
#[async_trait]
pub trait UsageLedger: Send + Sync {
    /// # Errors
    /// [`UsageDenied::QuotaExhausted`] (a typed denial, surfaced before the model
    /// turn) or a transport failure.
    async fn reserve(&self, evidence: &InboundEvidence) -> Result<(), UsageDenied>;
    async fn debit(&self, evidence: &InboundEvidence);
    async fn release(&self, evidence: &InboundEvidence);
    /// The account's balance as a person-facing line — a zero-unit read (BALANCE).
    async fn describe_balance(&self, evidence: &InboundEvidence) -> String;
}

/// Why a metered turn was refused before it ran. The three resource denials are
/// distinct so the person gets a true reason and the gate can prove each alone,
/// even though the server sent them all as one HTTP 429 (ADR-028).
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum UsageDenied {
    /// The account is out of TOTAL quota (M8-1).
    QuotaExhausted,
    /// This principal hit its per-window turn rate (M8-2).
    PrincipalRateLimited,
    /// This channel hit its per-window turn rate (M8-2).
    ChannelRateLimited,
    /// The global per-window provider ceiling is spent (M8-2).
    ProviderBudgetExhausted,
    /// The meter could not be reached — not a denial, a call that got no answer,
    /// which must not read as "quota spent".
    Transport(String),
}

/// The no-metering fallback: reserves always succeed, nothing is charged, and
/// BALANCE says accounting is not enabled here. It keeps the M6 scripts and the
/// tests that do not exercise quota unchanged, while the composition root wires
/// the real `HttpUsage` for the demo.
#[derive(Debug, Default)]
pub struct UnmeteredLedger;

#[async_trait]
impl UsageLedger for UnmeteredLedger {
    async fn reserve(&self, _evidence: &InboundEvidence) -> Result<(), UsageDenied> {
        Ok(())
    }
    async fn debit(&self, _evidence: &InboundEvidence) {}
    async fn release(&self, _evidence: &InboundEvidence) {}
    async fn describe_balance(&self, _evidence: &InboundEvidence) -> String {
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

/// The typed requests the conversation can express.
///
/// Serde-able because a `Book` request is persisted in the durable continuation
/// while its challenge awaits a human answer (ADR-026 approve-first).
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub enum Request {
    /// Start a booking: raise a challenge and create NOTHING until it is
    /// approved (§23.1). Approve-first replaces M6's book-then-confirm.
    Book(BookingRequest),
    /// `YES` — approve the pending challenge. A UNIT variant: the classifier
    /// only decides "this is an approval"; the deterministic dispatcher extracts
    /// the code from the message body, so the probabilistic seat never carries
    /// it.
    Approve,
    /// `NO` — decline the pending challenge, terminally. Same split as
    /// [`Self::Approve`]: the code is the dispatcher's to read.
    Decline,
    /// "Cancel it" — a cancellation with the referent left to authoritative
    /// resolution, where ambiguity can be ASKED about (spec §14.1).
    CancelIntent,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
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
    /// The pending challenge awaiting this caller's `YES`/`NO`, if any — so a
    /// real proposer can tell an approval from a fresh request. An OPAQUE state
    /// string only: never a receipt, a challenge id, or any authority type the
    /// proposer could act on.
    pub pending: Option<PendingSummary>,
}

/// That a challenge is awaiting a reply, and nothing a proposer could act with.
#[derive(Clone, Debug)]
pub struct PendingSummary {
    pub state: String,
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
    /// Classify one utterance. The proposer sees the reduced [`Utterance`], never
    /// the full `InboundMessage` — no transport evidence reaches the one seat BLD
    /// cannot trust (ADR-026).
    async fn propose(&self, context: &ProjectedContext, utterance: &Utterance) -> Proposed;
}

/// One inbound reply's transport evidence, as the dispatcher hands it to the
/// deposit port — primitives only, so this crate names no authority type.
///
/// The identity triple is transport-set (from `InboundIdentity`), which is what
/// stops the model seat naming an evidence row into being by choosing a sender.
#[derive(Clone, Debug)]
pub struct InboundEvidence {
    pub provider: String,
    pub account: String,
    pub message_id: String,
    pub address: String,
    pub verified: bool,
    pub signature: Option<String>,
}

/// What a deposit returned: the challenge the reply answers, and its receipt.
#[derive(Clone, Debug)]
pub struct Deposited {
    pub challenge: String,
    pub receipt: String,
}

/// What the dispatcher asks the authority to raise a challenge over — primitives
/// only (the wire body the server's `/approvals` endpoint expects).
#[derive(Clone, Debug)]
pub struct BeginApproval {
    pub booking: String,
    pub grantor: String,
    pub subject: String,
    pub binding_principal: String,
    pub binding_version: u64,
    pub behaviours: Vec<String>,
    pub purpose: String,
    pub requested_date: String,
    pub from: String,
    pub to: String,
    pub attendees: u16,
    pub wheelchair_accessible: bool,
    pub max_fee_pence: u64,
}

/// A raised challenge: its id, and the preview to send the person verbatim.
#[derive(Clone, Debug)]
pub struct Raised {
    pub challenge: String,
    pub preview: String,
}

/// Why an approval call did not produce a reference.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum ApprovalError {
    /// The code was wrong; this many tries remain. The person may retry.
    WrongCode { tries_left: u8 },
    /// The challenge is gone — expired, replayed, or already settled.
    Gone(String),
    /// The transport itself failed. Not a denial — a call that never got an
    /// answer, which must not read as "the person said no".
    Transport(String),
}

/// A parked, approved, or booked booking, durable across a restart (ADR-026).
///
/// Its lifecycle, in three states:
/// - `reference: None` — a challenge awaiting a human `YES`. A `YES` moves it on;
///   a `NO`, expiry, or restart-then-`YES` resolves it.
/// - `reference: Some`, `booked: false` — approved, its booking not yet walked to
///   `Booked`. This is the resume runner's target: a crash here left a live grant
///   with a booking owed.
/// - `reference: Some`, `booked: true` — the booking is `Booked`, and the row is
///   RETAINED as the live grant that permits cancelling it. The booking approval
///   grants `Cancel` over the booking, so `CANCEL` reuses this reference rather
///   than raising a second challenge. Cleared when the booking is cancelled.
///
/// The address is stored revealed plus its region so it can be reparsed after a
/// restart.
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct Continuation {
    pub principal: PrincipalId,
    pub challenge_id: String,
    pub booking_id: BookingId,
    pub request: Request,
    pub address_revealed: String,
    pub region: String,
    pub reference: Option<String>,
    /// Whether the booking has reached `Booked`. A resume target is
    /// `reference: Some` AND `booked: false`; a `booked: true` row is kept only
    /// so its grant can cancel the booking.
    pub booked: bool,
}

/// Deposit an inbound reply's transport evidence, getting a one-use receipt.
///
/// The dispatcher calls this ONLY on a reply (`YES`/`NO`) — never on a fresh
/// `BOOK`, which answers no challenge and whose deposit the authority would
/// refuse. Every type here is orchestrator-local: this crate cannot name
/// `townhall-authority`.
#[async_trait]
pub trait EvidenceDeposit: Send + Sync {
    /// # Errors
    /// The reply's number is awaiting no challenge, or the transport failed.
    async fn deposit(&self, evidence: &InboundEvidence) -> Result<Deposited, ApprovalError>;
}

/// Raise a challenge, and answer one with a forwarded receipt.
#[async_trait]
pub trait ApprovalPort: Send + Sync {
    /// # Errors
    /// The transport failed. (A reused challenge is not an error — the server
    /// returns the same id.)
    async fn begin(&self, request: &BeginApproval) -> Result<Raised, ApprovalError>;

    /// Answer `challenge` with `answer` (`"YES"`/`"NO"`), the person's `code`,
    /// and the forwarded `receipt`. `Some(reference)` on approval, `None` on a
    /// recorded decline.
    ///
    /// # Errors
    /// Wrong code (with tries left), the challenge gone, or a transport failure.
    async fn reply(
        &self,
        challenge: &str,
        answer: &str,
        code: &str,
        receipt: &str,
    ) -> Result<Option<String>, ApprovalError>;

    /// Revoke EVERY live grant the channel this inbound proves may authorize —
    /// the bulk safety exit behind a texted REVOKE. Returns the count stopped.
    ///
    /// No challenge and no grant reference: the authority resolves the sender to
    /// a binding and sweeps by grantor, all inside the server (ADR-026). The
    /// receipt never reaches this crate. Idempotent — a re-sent REVOKE returns
    /// the count still stopped, never an error.
    ///
    /// # Errors
    /// The sender resolves to no live binding, or the transport failed.
    async fn revoke_via_receipt(&self, evidence: &InboundEvidence) -> Result<u32, ApprovalError>;
}

/// Where parked and approved bookings wait durably — the approve-first analogue
/// of [`townhall_channel::SuppressionStore`], and sync for the same reason (it
/// is `std::fs`, persist-first).
pub trait ContinuationStore: Send + Sync {
    /// The continuation this principal's channel is currently in, if any (the
    /// most recent, so a later `BOOK` supersedes an earlier one for the next
    /// `YES`).
    fn load(&self, principal: &PrincipalId) -> Option<Continuation>;

    /// The continuation for a specific booking, if one is held — how `CANCEL`
    /// finds the grant that permits cancelling a booking it named by reference.
    fn load_for_booking(&self, booking: &BookingId) -> Option<Continuation>;

    /// Upsert by booking id, persisting BEFORE the in-memory change — a crash
    /// must never leave a live grant with no durable record of the booking owed.
    ///
    /// # Errors
    /// The change could not be made durable.
    fn record(&self, continuation: Continuation) -> Result<(), String>;

    /// Forget a settled challenge's continuation, persist-first.
    ///
    /// # Errors
    /// The change could not be made durable.
    fn clear(&self, challenge_id: &str) -> Result<(), String>;

    /// Every approved-but-unbooked continuation (`reference: Some`), for the
    /// resume runner. A parked challenge (`reference: None`) is left in place so
    /// a later `YES` still finds it.
    fn take_resumable(&self) -> Vec<Continuation>;
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
