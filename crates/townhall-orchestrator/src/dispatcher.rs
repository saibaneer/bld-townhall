//! The deterministic router: every message becomes a decision here, and every
//! decision is checked again downstream.
//!
//! Ordering is the contract (spec §15.1, Appendix B): channel controls are
//! answered before the proposer is consulted, before any budget would be spent,
//! before the wire is touched. The tests hold that order with a panicking
//! proposer and a counting wire — a control command that reaches either fails
//! loudly.

use crate::ports::{
    BookingRequest, CandidateSummary, CredentialSource, PrincipalDirectory, ProjectedContext,
    Proposed, Proposer, Request, UsageBalance, WireFactory,
};
use bld_types::{BookingId, CouncilBookingRef, PrincipalId};
use std::collections::HashMap;
use std::sync::{Arc, Mutex};
use std::time::Duration;
use townhall_channel::{
    ChannelAddress, ChannelError, Command, ControlCommand, HumanChannel, InboundMessage,
    OutboundMessage, RawInbound, ResourceCommand, SuppressionStore, classify,
};
use townhall_gateway::{GatewayError, Projection, Turn};

/// Conversation memory: a routing aid, never business state (spec §3.1).
///
/// # No version field, by construction
///
/// There is nothing here a proposal could reuse: every proposal path reloads
/// through the wire and carries the version it just read. Losing a session
/// costs a re-ask — candidates come from `?cancellable=true`, which is what
/// makes a restart survivable at all.
#[derive(Debug, Default)]
struct Session {
    /// Most recent first. Ids only — what "it" probably means, never what it is.
    recent: Vec<BookingId>,
}

impl Session {
    fn remember(&mut self, id: &BookingId) {
        self.recent.retain(|known| known != id);
        self.recent.insert(0, id.clone());
    }
}

/// A convergence follow-up the dispatcher owes someone, later.
///
/// Queued rather than spawned: the tests and the binary drain the queue
/// explicitly through [`Dispatcher::run_followups`], so nothing races and
/// nothing sleeps. This is also where STOP gates the TURN — see that method.
struct Followup {
    address: ChannelAddress,
    principal: PrincipalId,
    booking: BookingId,
    first_wait: Duration,
    /// What the outcome message should say the booking WAS about.
    verb: &'static str,
}

pub struct Dispatcher<C: HumanChannel<Address = ChannelAddress>> {
    channel: Arc<C>,
    directory: Arc<dyn PrincipalDirectory>,
    credentials: Arc<dyn CredentialSource>,
    balance: Arc<dyn UsageBalance>,
    proposer: Arc<dyn Proposer>,
    suppression: Arc<dyn SuppressionStore>,
    wires: Arc<dyn WireFactory>,
    sessions: Mutex<HashMap<PrincipalId, Session>>,
    followups: Mutex<Vec<Followup>>,
}

/// One turn's access to the wire, in its two kinds.
///
/// # Why a holder rather than a wire
///
/// Through M6 the dispatcher built ONE wire at the top of a turn and passed it
/// everywhere, which was right while a credential authorized everything its
/// holder could name. M7B split reading from changing: a read is scoped to a
/// principal, a change presents a grant naming ONE booking — and the dispatcher
/// does not know which booking a turn is about until it has read something.
///
/// "Cancel it" is the case that forces the shape. It needs Lucy's bookings read
/// before any grant can name the one she means, so the reader comes first and
/// the changer is built afterwards, per booking (spec §23.1).
struct Wires<'turn> {
    factory: &'turn dyn crate::ports::WireFactory,
    token: String,
    principal: PrincipalId,
    /// Built once and shared: reading is not per-booking.
    reader: std::sync::Arc<dyn crate::ports::BookingWire>,
}

impl<'turn> Wires<'turn> {
    fn new(
        factory: &'turn dyn crate::ports::WireFactory,
        token: String,
        principal: PrincipalId,
    ) -> Self {
        let reader = factory.reader_for(&token, &principal);
        Self {
            factory,
            token,
            principal,
            reader,
        }
    }

    /// The read wire. Cannot change anything, by construction.
    fn reader(&self) -> &dyn crate::ports::BookingWire {
        self.reader.as_ref()
    }

    /// A wire that may change `booking`, and only that booking.
    ///
    /// # The reference this presents, and what replaces it
    ///
    /// M7B's dev lane treats the reference as the booking id, because nobody
    /// has been asked yet and there is no approval to name. M7C changes exactly
    /// this line: the reference becomes the one an approval produced, and a
    /// turn nobody approved cannot build a changer at all.
    fn changer(&self, booking: &BookingId) -> std::sync::Arc<dyn crate::ports::BookingWire> {
        self.factory
            .changer_for(&self.token, &self.principal, booking.as_str())
    }
}

impl<C: HumanChannel<Address = ChannelAddress>> Dispatcher<C> {
    #[must_use]
    #[allow(clippy::too_many_arguments)] // the composition root names every port once
    pub fn new(
        channel: Arc<C>,
        directory: Arc<dyn PrincipalDirectory>,
        credentials: Arc<dyn CredentialSource>,
        balance: Arc<dyn UsageBalance>,
        proposer: Arc<dyn Proposer>,
        suppression: Arc<dyn SuppressionStore>,
        wires: Arc<dyn WireFactory>,
    ) -> Self {
        Self {
            channel,
            directory,
            credentials,
            balance,
            proposer,
            suppression,
            wires,
            sessions: Mutex::new(HashMap::new()),
            followups: Mutex::new(Vec::new()),
        }
    }

    /// One inbound message, handled to completion (its reply sent).
    ///
    /// # Errors
    /// Only transport-level channel failure. A refused message (unroutable,
    /// too long, duplicate) is handled *within* — refusals are conversation,
    /// not errors.
    pub async fn handle(&self, raw: RawInbound) -> Result<(), ChannelError> {
        let message = match self.channel.receive(raw).await {
            Ok(message) => message,
            // A duplicate was already answered once (answering again is the
            // double-act dedupe exists to stop); an unroutable address has no
            // one to answer; and TooLong is rejected before this layer holds a
            // parsed address to reply to. All three end the turn quietly.
            Err(
                ChannelError::Duplicate
                | ChannelError::UnroutableAddress(_)
                | ChannelError::TooLong { .. },
            ) => return Ok(()),
        };

        // 1. Channel controls — answered from ports, before ANYTHING else.
        //    (§15.1: "handled deterministically before invoking the LLM".)
        if let Command::Control(control) = classify(message.body.revealed()) {
            let text = self.answer_control(control, &message);
            self.reply(&message.address, text).await;
            return Ok(());
        }

        // 2. Identity. Everything past this line acts on someone's behalf.
        let Some(principal) = self.directory.resolve(&message.address) else {
            self.reply(
                &message.address,
                "I don't recognize this number.".to_owned(),
            )
            .await;
            return Ok(());
        };
        let Some(token) = self.credentials.token_for(&principal) else {
            self.reply(
                &message.address,
                "I can't act for this number yet.".to_owned(),
            )
            .await;
            return Ok(());
        };
        let wires = Wires::new(self.wires.as_ref(), token, principal.clone());

        let text = match classify(message.body.revealed()) {
            Command::Control(_) => unreachable!("handled above"),
            Command::Resource(resource) => {
                self.answer_resource(resource, &principal, &wires, &message)
                    .await
            }
            Command::Freeform => {
                // 3. The proposer — LAST, with a projection and nothing else.
                let context = self.project(&principal, wires.reader()).await;
                match self.proposer.propose(&context, &message).await {
                    Proposed::Unclear => {
                        "I didn't understand. Reply HELP for what I can do.".to_owned()
                    }
                    Proposed::Typed(request) => {
                        self.execute(request, &principal, &wires, &message).await
                    }
                }
            }
        };
        self.reply(&message.address, text).await;
        Ok(())
    }

    /// Drain the follow-up queue: the convergence turns the dispatcher owes.
    ///
    /// # STOP gates the TURN here, not just its message
    ///
    /// §14.1: STOP stops "automated/non-essential outbound messaging **and
    /// scheduled agent turns**". A suppressed follow-up is skipped BEFORE any
    /// wire is built or call made — suppressing only the outbound would do the
    /// work, spend what the work costs, and reach what the work reaches, with
    /// nothing visible to the person who said stop. Server-side reconciliation
    /// is untouched either way: the booking still settles at the council,
    /// because STOP silences the messenger, not the boundary.
    pub async fn run_followups(&self) {
        let pending: Vec<Followup> = std::mem::take(
            &mut *self
                .followups
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner),
        );
        for followup in pending {
            if self.suppression.is_suppressed(&followup.address) {
                continue; // the whole turn, skipped — deliberately, forever:
                // the human said stop, and the booking's truth stays
                // reachable through STATUS.
            }
            // Re-resolve the binding AT DRAIN TIME. The follow-up captured an
            // (address, principal) pair that was true when queued; if the
            // directory has since rebound that address, sending would put one
            // principal's booking reference on another principal's phone — the
            // review's sharpest scenario. Drift means drop, before any wire
            // exists to leak through.
            if self.directory.resolve(&followup.address).as_ref() != Some(&followup.principal) {
                continue;
            }

            let Some(token) = self.credentials.token_for(&followup.principal) else {
                continue;
            };
            // The RE-RESOLVED principal, not one captured when the follow-up
            // was queued. The drift check above compares them; using the
            // captured value here would reintroduce the same defect one line
            // later, on the header that decides whose bookings are in scope.
            // A READER: convergence only reads. The chase owns the effect
            // (ADR-019), so a follow-up that could change something would be a
            // second way to cause what recovery is already causing.
            let wire = self.wires.reader_for(&token, &followup.principal);
            let text = match wire.converge(&followup.booking, followup.first_wait).await {
                Ok(projection) => outcome_text(followup.verb, &projection),
                Err(error) => format!("Still working on it ({error})."),
            };
            let _ = self
                .channel
                .send(&followup.address, OutboundMessage::automated(text))
                .await;
        }
    }

    fn answer_control(&self, control: ControlCommand, message: &InboundMessage) -> String {
        match control {
            ControlCommand::Help => "I can book a town hall. Send: BOOK date=YYYY-MM-DD \
                 from=HH:MM to=HH:MM people=N accessible=yes max=PENCE, then CONFIRM. \
                 Also: STATUS, CANCEL <ref>, BALANCE, STOP, START."
                .to_owned(),
            ControlCommand::Balance => match self.directory.resolve(&message.address) {
                Some(principal) => self.balance.describe(&principal),
                None => "I don't recognize this number.".to_owned(),
            },
            // A failed persist is a failed STOP, and saying otherwise is the
            // failure mode the review named: a confirmation that lasts exactly
            // until the next restart. The human hears the truth and can retry.
            ControlCommand::Stop => match self.suppression.suppress(&message.address) {
                Ok(()) => "Automated messages stopped. Reply START to resume. \
                 This does not cancel bookings."
                    .to_owned(),
                Err(_) => "I couldn't make that stick — automated messages are NOT \
                 stopped. Reply STOP to try again."
                    .to_owned(),
            },
            ControlCommand::Start => match self.suppression.allow(&message.address) {
                Ok(()) => "Automated messages resumed.".to_owned(),
                Err(_) => "I couldn't make that stick — reply START to try again.".to_owned(),
            },
            ControlCommand::Revoke => {
                "Delegations arrive with M7; there is nothing to revoke yet.".to_owned()
            }
        }
    }

    async fn answer_resource(
        &self,
        resource: ResourceCommand,
        principal: &PrincipalId,
        wires: &Wires<'_>,
        message: &InboundMessage,
    ) -> String {
        match resource {
            ResourceCommand::Status { reference } => {
                let projection = match reference {
                    Some(reference) => {
                        match wires
                            .reader()
                            .by_reference(&CouncilBookingRef::new(reference.clone()))
                            .await
                        {
                            Ok(rows) if rows.is_empty() => {
                                return format!("No booking with reference {reference}.");
                            }
                            Ok(mut rows) => Ok(rows.remove(0)),
                            Err(error) => Err(error),
                        }
                    }
                    None => match self.most_recent(principal) {
                        Some(id) => wires.reader().read(&id).await,
                        None => return "You have no bookings.".to_owned(),
                    },
                };
                match projection {
                    Ok(projection) => status_text(&projection),
                    Err(GatewayError::UnknownBooking) => "You have no bookings.".to_owned(),
                    Err(error) => cannot_answer(&error),
                }
            }
            ResourceCommand::Cancel { reference } => {
                match wires
                    .reader()
                    .by_reference(&CouncilBookingRef::new(reference.clone()))
                    .await
                {
                    Ok(rows) if rows.is_empty() => {
                        format!("No booking with reference {reference}.")
                    }
                    Ok(mut rows) => {
                        let projection = rows.remove(0);
                        self.cancel(projection, principal, wires, message).await
                    }
                    Err(error) => cannot_answer(&error),
                }
            }
        }
    }

    async fn execute(
        &self,
        request: Request,
        principal: &PrincipalId,
        wires: &Wires<'_>,
        message: &InboundMessage,
    ) -> String {
        match request {
            Request::Book(booking) => self.book(booking, principal, wires, message).await,
            Request::Confirm => self.confirm(principal, wires, message).await,
            Request::CancelIntent => self.cancel_intent(principal, wires, message).await,
        }
    }

    /// The booking walk: create → venues → select → verify, stopping at
    /// `AwaitingBooking` with §15.2's preamble reply. Every step reloads
    /// nothing — each `propose_at` carries the version the previous turn's
    /// answer reported, and the walk starts from a fresh create or an
    /// authoritative read.
    async fn book(
        &self,
        request: BookingRequest,
        principal: &PrincipalId,
        wires: &Wires<'_>,
        message: &InboundMessage,
    ) -> String {
        // The message IS the intent, so it names the intent: a carrier
        // redelivery derives the same id and lands on AlreadyExists instead of
        // booking twice — even across a restart that emptied the replay window.
        let id = message.identity.booking_id();
        // The changer is built HERE, once the booking has a name — which is the
        // earliest a grant can exist for it. Everything above this line ran on
        // a wire that could not change anything.
        let wire = wires.changer(&id);
        let created = match wire.create(&id, &request.requirements()).await {
            Ok(projection) => projection,
            Err(GatewayError::Existing { .. }) => {
                // The redelivery path: this exact message already created it.
                self.remember(principal, &id);
                return match wires.reader().read(&id).await {
                    Ok(projection) => {
                        format!("Already working on that one. {}", status_text(&projection))
                    }
                    Err(error) => cannot_answer(&error),
                };
            }
            Err(error) => return cannot_answer(&error),
        };
        self.remember(principal, &id);

        // Pick the first venue whose facts fit the request. The COUNCIL's
        // guards re-check all of this at verify — the pick is a suggestion,
        // the boundary is the boundary.
        let venues = match wires.reader().venues().await {
            Ok(rows) => rows,
            Err(error) => return cannot_answer(&error),
        };
        let Some(venue) = venues.iter().find(|venue| {
            venue.available
                && venue.capacity >= request.people
                && (!request.accessible || venue.accessible)
                && venue.fee_pence <= request.max_pence
        }) else {
            return "No venue fits those limits.".to_owned();
        };

        let selected = wire
            .propose_at(
                &id,
                created.version,
                "select-venue",
                Some(serde_json::json!({
                    "venue_id": venue.venue_id,
                    "slot_id": venue.slot_id,
                })),
            )
            .await;
        let version = match selected {
            Ok(Turn::Committed { version, .. }) => version,
            other => return turn_text("select a venue", other),
        };
        match wire.propose_at(&id, version, "verify-slot", None).await {
            Ok(Turn::Committed { .. }) => format!(
                "I can ask TownHallAgent to make one booking matching those limits. \
                 Maximum booking fee: £{}.{:02}. Reply CONFIRM to book.",
                request.max_pence / 100,
                request.max_pence % 100
            ),
            other => turn_text("verify the slot", other),
        }
    }

    /// Walk the most recent booking to `Book`, wherever it stands — reloading
    /// FIRST, because the world may have moved since the session last saw it
    /// (spec §3.1), and following the menu the reload reports.
    async fn confirm(
        &self,
        principal: &PrincipalId,
        wires: &Wires<'_>,
        message: &InboundMessage,
    ) -> String {
        let Some(id) = self.most_recent(principal) else {
            return "Nothing to confirm — start with BOOK. Reply HELP for the format.".to_owned();
        };
        // The booking is known, so a grant over it can be presented. The reads
        // inside the walk still go through the reader: reloading is not
        // changing, and keeping them separate means a wire that could change
        // something is only ever held where something is being changed.
        let wire = wires.changer(&id);

        // Bounded: the longest legal walk is revalidate → verify → book.
        for _ in 0..4 {
            let projection = match wires.reader().read(&id).await {
                Ok(projection) => projection,
                Err(error) => return cannot_answer(&error),
            };
            let behaviour = match projection.state.as_str() {
                "NeedsRevalidation" => "revalidate-venue",
                "VenueSelected" => "verify-slot",
                "AwaitingBooking" => "book",
                "Booked" => return status_text(&projection),
                other => {
                    return format!("Can't book from here ({other}). Reply STATUS to see it.");
                }
            };
            match wire
                .propose_at(&id, projection.version, behaviour, None)
                .await
            {
                Ok(Turn::Committed { state, .. }) if state == "Booked" => {
                    return match wires.reader().read(&id).await {
                        Ok(done) => outcome_text("Booked", &done),
                        Err(error) => cannot_answer(&error),
                    };
                }
                Ok(Turn::Committed { .. }) => { /* next leg of the walk */ }
                Ok(Turn::Accepted { retry_after }) => {
                    // The two-message shape: acknowledge NOW as a reply; the
                    // outcome arrives later as an automated follow-up the
                    // queue owns (and STOP may silence).
                    self.queue_followup(message, principal, &id, retry_after, "Booked");
                    return "Booking now.".to_owned();
                }
                other => return turn_text("book", other),
            }
        }
        "Still working on it. Reply STATUS to check.".to_owned()
    }

    /// "Cancel it": referent resolution from the AUTHORITATIVE cancellable set,
    /// with session memory only ordering the candidates. Ambiguity asks —
    /// conversation memory may suggest, never choose (spec §14.1).
    async fn cancel_intent(
        &self,
        principal: &PrincipalId,
        wires: &Wires<'_>,
        message: &InboundMessage,
    ) -> String {
        let mut candidates = match wires.reader().cancellable().await {
            Ok(rows) => rows,
            Err(error) => return cannot_answer(&error),
        };
        match candidates.len() {
            0 => "You have no bookings to cancel.".to_owned(),
            1 => {
                let projection = candidates.remove(0);
                self.cancel(projection, principal, wires, message).await
            }
            _ => {
                // Order by the session's recency so the question reads
                // naturally, but ASK — do not pick.
                let recent = self.recent_order(principal);
                candidates.sort_by_key(|row| {
                    recent
                        .iter()
                        .position(|id| id.as_str() == row.id)
                        .unwrap_or(usize::MAX)
                });
                let named: Vec<String> = candidates.iter().map(candidate_name).collect();
                format!(
                    "You have {} bookings: {}. Reply CANCEL <ref> to choose.",
                    named.len(),
                    named.join(", ")
                )
            }
        }
    }

    async fn cancel(
        &self,
        projection: Projection,
        principal: &PrincipalId,
        wires: &Wires<'_>,
        message: &InboundMessage,
    ) -> String {
        let id = BookingId::new(projection.id.clone());
        self.remember(principal, &id);
        // The booking arrived already identified — a reader found it — so the
        // grant naming it can be presented now and not before.
        let wire = wires.changer(&id);
        let turn = wire
            .propose_at(
                &id,
                projection.version,
                "cancel",
                Some(serde_json::json!({"reason": "cancelled by SMS"})),
            )
            .await;
        match turn {
            Ok(Turn::Committed { state, .. }) if state == "Cancelled" => {
                match projection.booking_ref {
                    Some(reference) => format!("Cancelled. Council ref {reference}."),
                    None => "Cancelled.".to_owned(),
                }
            }
            Ok(Turn::Committed { state, .. }) => {
                format!("Cancellation under way ({state}). Reply STATUS to check.")
            }
            Ok(Turn::Accepted { retry_after }) => {
                self.queue_followup(message, principal, &id, retry_after, "Cancelled");
                "Cancelling now.".to_owned()
            }
            other => turn_text("cancel", other),
        }
    }

    async fn project(
        &self,
        principal: &PrincipalId,
        wire: &dyn crate::ports::BookingWire,
    ) -> ProjectedContext {
        let cancellable = match wire.cancellable().await {
            Ok(rows) => rows
                .into_iter()
                .map(|row| CandidateSummary {
                    id: BookingId::new(row.id),
                    reference: row.booking_ref.map(CouncilBookingRef::new),
                    state: row.state,
                })
                .collect(),
            Err(_) => Vec::new(),
        };
        let _ = principal;
        ProjectedContext { cancellable }
    }

    fn queue_followup(
        &self,
        message: &InboundMessage,
        principal: &PrincipalId,
        booking: &BookingId,
        first_wait: Duration,
        verb: &'static str,
    ) {
        self.followups
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .push(Followup {
                address: message.address.clone(),
                principal: principal.clone(),
                booking: booking.clone(),
                first_wait,
                verb,
            });
    }

    async fn reply(&self, to: &ChannelAddress, text: String) {
        let _ = self.channel.send(to, OutboundMessage::reply(text)).await;
    }

    fn remember(&self, principal: &PrincipalId, id: &BookingId) {
        self.sessions
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .entry(principal.clone())
            .or_default()
            .remember(id);
    }

    fn most_recent(&self, principal: &PrincipalId) -> Option<BookingId> {
        self.sessions
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .get(principal)
            .and_then(|session| session.recent.first().cloned())
    }

    fn recent_order(&self, principal: &PrincipalId) -> Vec<BookingId> {
        self.sessions
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .get(principal)
            .map(|session| session.recent.clone())
            .unwrap_or_default()
    }
}

fn candidate_name(projection: &Projection) -> String {
    projection
        .booking_ref
        .clone()
        .unwrap_or_else(|| projection.id.clone())
}

fn status_text(projection: &Projection) -> String {
    let mut text = format!(
        "{}. Attendees {}.",
        projection.state, projection.requirements.attendees
    );
    if let Some(reference) = &projection.booking_ref {
        use std::fmt::Write as _;
        let _ = write!(text, " Council ref {reference}.");
    }
    text
}

fn outcome_text(verb: &str, projection: &Projection) -> String {
    match &projection.booking_ref {
        Some(reference) => format!(
            "{verb}. Council ref {reference}. Reply CANCEL {reference} at any time to cancel."
        ),
        None => format!("{verb}. ({}).", projection.state),
    }
}

fn turn_text(doing: &str, turn: Result<Turn, GatewayError>) -> String {
    match turn {
        Ok(Turn::Denied { reason }) => format!("Couldn't {doing}: {reason}."),
        Ok(Turn::NotAvailable { .. }) => {
            format!("Couldn't {doing} from where the booking stands. Reply STATUS to see it.")
        }
        Ok(other) => format!("Unexpected answer while trying to {doing}: {other:?}."),
        Err(error) => cannot_answer(&error),
    }
}

fn cannot_answer(error: &GatewayError) -> String {
    match error {
        GatewayError::Unavailable(_) | GatewayError::ProviderSilent(_) => {
            "The booking service can't be reached right now. Nothing was changed.".to_owned()
        }
        GatewayError::Stale { .. } | GatewayError::Contended => {
            "Things moved while I was working. Reply STATUS and try again.".to_owned()
        }
        other => format!("Something went wrong ({other}). Nothing was changed."),
    }
}
