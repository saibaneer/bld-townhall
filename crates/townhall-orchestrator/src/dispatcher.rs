//! The deterministic router: every message becomes a decision here, and every
//! decision is checked again downstream.
//!
//! Ordering is the contract (spec §15.1, Appendix B): channel controls are
//! answered before the proposer is consulted, before any budget would be spent,
//! before the wire is touched. The tests hold that order with a panicking
//! proposer and a counting wire — a control command that reaches either fails
//! loudly.

use crate::ports::{
    ApprovalError, ApprovalPort, BeginApproval, BookingRequest, CandidateSummary, Continuation,
    ContinuationStore, CredentialSource, EvidenceDeposit, InboundEvidence, PendingSummary,
    PrincipalDirectory, ProjectedContext, Proposed, Proposer, Request, UsageBalance, WireFactory,
};
use bld_types::{BookingId, CouncilBookingRef, PrincipalId};
use std::collections::HashMap;
use std::sync::{Arc, Mutex};
use std::time::Duration;
use townhall_channel::{
    ChannelAddress, ChannelError, Command, ControlCommand, HumanChannel, InboundMessage,
    OutboundMessage, RawInbound, Region, ResourceCommand, SuppressionStore, classify,
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

/// How a booking walk ended, and therefore what its durable continuation owes.
enum Walk {
    /// Reached `Booked` — mark the continuation booked, so its grant can cancel
    /// it, and reply the outcome.
    Booked(String),
    /// Submitted but not yet `Booked` — leave the continuation for the resume
    /// runner (or the follow-up queue) and reply the acknowledgement.
    InFlight(String),
    /// A terminal refusal (no venue fits, a denial) — clear the continuation, the
    /// approval spent with nothing to book, and reply why.
    Failed(String),
    /// The service was briefly unreachable — leave the continuation for the next
    /// resume rather than declare an outcome, and reply so.
    Unreachable(String),
}

pub struct Dispatcher<C: HumanChannel<Address = ChannelAddress>> {
    channel: Arc<C>,
    directory: Arc<dyn PrincipalDirectory>,
    credentials: Arc<dyn CredentialSource>,
    balance: Arc<dyn UsageBalance>,
    proposer: Arc<dyn Proposer>,
    suppression: Arc<dyn SuppressionStore>,
    wires: Arc<dyn WireFactory>,
    /// Raise a challenge and answer it — the approve-first authority, over HTTP.
    approvals: Arc<dyn ApprovalPort>,
    /// Deposit a reply's evidence and get a one-use receipt (ADR-026).
    evidence: Arc<dyn EvidenceDeposit>,
    /// Durable parked/approved/booked bookings, so a `YES` — or a booking owed —
    /// survives a restart.
    continuations: Arc<dyn ContinuationStore>,
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

    /// A wire that may change one booking, presenting the delegation `reference`
    /// an approval produced.
    ///
    /// # The reference this presents, and what it replaced
    ///
    /// M7B's dev lane treated the reference as the booking id, because nobody had
    /// been asked and there was no approval to name. M7C changed exactly this: the
    /// reference is now the one a person's `YES` produced, and a turn nobody
    /// approved *cannot* build a changer at all — there is no booking id to fall
    /// back on, only a reference an approval issued (§23.1, W8).
    fn changer_with_reference(
        &self,
        reference: &str,
    ) -> std::sync::Arc<dyn crate::ports::BookingWire> {
        self.factory
            .changer_for(&self.token, &self.principal, reference)
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
        approvals: Arc<dyn ApprovalPort>,
        evidence: Arc<dyn EvidenceDeposit>,
        continuations: Arc<dyn ContinuationStore>,
    ) -> Self {
        Self {
            channel,
            directory,
            credentials,
            balance,
            proposer,
            suppression,
            wires,
            approvals,
            evidence,
            continuations,
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
                // 3. The proposer — LAST, with a projection and the REDUCED view
                //    (no transport evidence), and nothing else. The full message
                //    is kept here for the deposit path an approval reply takes.
                let context = self.project(&principal, wires.reader()).await;
                let utterance = message.utterance();
                match self.proposer.propose(&context, &utterance).await {
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
                 from=HH:MM to=HH:MM people=N accessible=yes max=PENCE, then reply YES <code> \
                 to approve (or NO to decline). Also: STATUS, CANCEL <ref>, BALANCE, STOP, START."
                .to_owned(),
            ControlCommand::Balance => match self.directory.resolve(&message.address) {
                Some(principal) => self.balance.describe(&principal),
                None => "I don't recognize this number.".to_owned(),
            },
            // STOP TERMINATES the agent's action — it does not merely mute it.
            // While stopped, no automated message is sent AND no owed booking is
            // completed on your behalf (§14.1: automated messaging AND scheduled
            // agent turns). It does not undo a booking you already have — that is
            // CANCEL. A failed persist is a failed STOP, said plainly: a
            // confirmation that lasts until the next restart is the lie the
            // review named.
            ControlCommand::Stop => match self.suppression.suppress(&message.address) {
                Ok(()) => "Stopped. I won't act on your behalf or message you until you reply \
                 START. This does not remove bookings you already have — reply CANCEL <ref> \
                 for that."
                    .to_owned(),
                Err(_) => "I couldn't make that stick — I am NOT stopped. Reply STOP to try again."
                    .to_owned(),
            },
            ControlCommand::Start => match self.suppression.allow(&message.address) {
                Ok(()) => "Resumed. I'll pick up anything that was owed.".to_owned(),
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
            Request::Book(booking) => self.book(booking, principal, message).await,
            Request::Approve => self.approve(principal, wires, message).await,
            Request::Decline => self.decline(principal, message).await,
            Request::CancelIntent => self.cancel_intent(principal, wires, message).await,
        }
    }

    /// Build the wire body the `/approvals` endpoint expects for a booking.
    ///
    /// `binding_version` is hardcoded to `1` and `purpose` to a fixed string:
    /// the demo binds every channel at revision 1, and `BookingRequest` carries
    /// no purpose. Production needs the directory to return a `BindingRef` (with
    /// the revision) and a purpose to ride the request — noted, not built here.
    #[allow(clippy::unused_self)] // a method for cohesion with the booking paths
    fn begin_request(
        &self,
        id: &BookingId,
        principal: &PrincipalId,
        request: &BookingRequest,
    ) -> BeginApproval {
        BeginApproval {
            booking: id.as_str().to_owned(),
            grantor: principal.as_str().to_owned(),
            subject: principal.as_str().to_owned(),
            binding_principal: principal.as_str().to_owned(),
            binding_version: 1,
            behaviours: vec![
                "SelectVenue".to_owned(),
                "VerifySlot".to_owned(),
                "Book".to_owned(),
                "Cancel".to_owned(),
            ],
            purpose: "community meeting".to_owned(),
            requested_date: request.date.clone(),
            from: request.from.clone(),
            to: request.to.clone(),
            attendees: request.people,
            wheelchair_accessible: request.accessible,
            max_fee_pence: request.max_pence,
        }
    }

    /// BOOK raises a challenge and creates NOTHING (§23.1). The booking is owed
    /// but not made: a durable continuation records that, and the person is sent
    /// the preview to approve. The booking only happens after a `YES`.
    async fn book(
        &self,
        request: BookingRequest,
        principal: &PrincipalId,
        message: &InboundMessage,
    ) -> String {
        // The message IS the intent, so it names the intent: a carrier
        // redelivery derives the same id, the server's idempotent begin returns
        // the same challenge, and the continuation upserts — no second prompt,
        // no second booking, even across a restart that emptied the replay
        // window.
        let id = message.identity.booking_id();
        let raised = match self
            .approvals
            .begin(&self.begin_request(&id, principal, &request))
            .await
        {
            Ok(raised) => raised,
            Err(error) => return approval_error_text(&error),
        };
        // Record the parked challenge BEFORE replying: a crash after the preview
        // is sent but before this persisted would leave a `YES` answering
        // nothing.
        let continuation = Continuation {
            principal: principal.clone(),
            challenge_id: raised.challenge.clone(),
            booking_id: id.clone(),
            request: Request::Book(request),
            address_revealed: message.address.revealed().to_owned(),
            region: region_tag(message),
            reference: None,
            booked: false,
        };
        if let Err(why) = self.continuations.record(continuation) {
            return format!("I couldn't hold onto that request ({why}). Reply BOOK to try again.");
        }
        self.remember(principal, &id);
        // The preview is the server's own — it names the code the person sends
        // back and the ceiling they are approving.
        raised.preview
    }

    /// `YES <code>` — deposit the reply's evidence, forward the receipt, and on
    /// approval walk the booking to `Booked` under the reference the grant
    /// produced.
    async fn approve(
        &self,
        principal: &PrincipalId,
        wires: &Wires<'_>,
        message: &InboundMessage,
    ) -> String {
        let Some(continuation) = self.continuations.load(principal) else {
            return "Nothing to approve — start with BOOK. Reply HELP for the format.".to_owned();
        };
        let code = answer_code(message.body.revealed(), "yes");
        let deposited = match self.evidence.deposit(&inbound_evidence(message)).await {
            Ok(deposited) => deposited,
            Err(error) => return approval_error_text(&error),
        };
        let reference = match self
            .approvals
            .reply(&deposited.challenge, "YES", &code, &deposited.receipt)
            .await
        {
            Ok(Some(reference)) => reference,
            // A YES that produced no reference is not an approval the server
            // recorded — treat it as a decline outcome rather than invent one.
            Ok(None) => {
                let _ = self.continuations.clear(&continuation.challenge_id);
                return "That wasn't recorded as an approval.".to_owned();
            }
            Err(error) => return self.after_reply_error(&continuation, &error),
        };
        // Persist the reference BEFORE the booking walk: the grant is live now,
        // so a crash mid-walk must leave a durable record of the booking owed —
        // which is exactly what the resume runner completes (W7).
        let approved = Continuation {
            reference: Some(reference.clone()),
            ..continuation.clone()
        };
        if let Err(why) = self.continuations.record(approved.clone()) {
            return format!("Approved, but I couldn't record it ({why}). Reply STATUS to check.");
        }
        self.remember(principal, &continuation.booking_id);
        let Request::Book(request) = &continuation.request else {
            return "That request can't be booked. Reply HELP.".to_owned();
        };
        let wire = wires.changer_with_reference(&reference);
        self.settle_walk(
            self.complete_booking(
                &continuation.booking_id,
                request,
                wire.as_ref(),
                wires.reader(),
                &message.address,
                principal,
            )
            .await,
            &approved,
        )
    }

    /// `NO <code>` — deposit the reply's evidence, decline the challenge
    /// terminally, and book nothing.
    async fn decline(&self, principal: &PrincipalId, message: &InboundMessage) -> String {
        let Some(continuation) = self.continuations.load(principal) else {
            return "Nothing to decline — start with BOOK. Reply HELP for the format.".to_owned();
        };
        let code = answer_code(message.body.revealed(), "no");
        let deposited = match self.evidence.deposit(&inbound_evidence(message)).await {
            Ok(deposited) => deposited,
            Err(error) => return approval_error_text(&error),
        };
        match self
            .approvals
            .reply(&deposited.challenge, "NO", &code, &deposited.receipt)
            .await
        {
            Ok(_) => {
                let _ = self.continuations.clear(&continuation.challenge_id);
                "Cancelled the pending request. Nothing was booked.".to_owned()
            }
            Err(error) => self.after_reply_error(&continuation, &error),
        }
    }

    /// Drive a fresh or resumed booking through create → select-venue →
    /// verify-slot → book, reading state each step so a resumed walk continues
    /// from wherever a crash left it. `create` is idempotent (the id is derived
    /// from the message), so a re-run lands on `Existing` and books once (W7).
    async fn complete_booking(
        &self,
        id: &BookingId,
        request: &BookingRequest,
        wire: &dyn crate::ports::BookingWire,
        reader: &dyn crate::ports::BookingWire,
        address: &ChannelAddress,
        principal: &PrincipalId,
    ) -> Walk {
        match wire.create(id, &request.requirements()).await {
            Ok(_) | Err(GatewayError::Existing { .. }) => {}
            Err(error) => return Walk::Unreachable(cannot_answer(&error)),
        }
        // Bounded: created → select-venue → verify-slot → book is the longest
        // legal walk, with revalidation as the one detour.
        for _ in 0..6 {
            let projection = match reader.read(id).await {
                Ok(projection) => projection,
                Err(error) => return Walk::Unreachable(cannot_answer(&error)),
            };
            let (behaviour, body) = match projection.state.as_str() {
                "Created" | "Draft" => {
                    let venues = match reader.venues().await {
                        Ok(rows) => rows,
                        Err(error) => return Walk::Unreachable(cannot_answer(&error)),
                    };
                    let Some(venue) = venues.iter().find(|venue| {
                        venue.available
                            && venue.capacity >= request.people
                            && (!request.accessible || venue.accessible)
                            && venue.fee_pence <= request.max_pence
                    }) else {
                        // The person approved, but no venue fits the ceiling they
                        // approved — a terminal refusal, not a retry.
                        return Walk::Failed("No venue fits those limits.".to_owned());
                    };
                    (
                        "select-venue",
                        Some(serde_json::json!({
                            "venue_id": venue.venue_id,
                            "slot_id": venue.slot_id,
                        })),
                    )
                }
                "VenueSelected" => ("verify-slot", None),
                "NeedsRevalidation" => ("revalidate-venue", None),
                "AwaitingBooking" => ("book", None),
                "Booked" => return Walk::Booked(outcome_text("Booked", &projection)),
                other => {
                    return Walk::Failed(format!(
                        "Can't book from here ({other}). Reply STATUS to see it."
                    ));
                }
            };
            match wire
                .propose_at(id, projection.version, behaviour, body)
                .await
            {
                Ok(Turn::Committed { state, .. }) if state == "Booked" => {
                    return match reader.read(id).await {
                        Ok(done) => Walk::Booked(outcome_text("Booked", &done)),
                        Err(error) => Walk::Unreachable(cannot_answer(&error)),
                    };
                }
                Ok(Turn::Committed { .. }) => { /* next leg of the walk */ }
                Ok(Turn::Accepted { retry_after }) => {
                    // The two-message shape: acknowledge now; the outcome arrives
                    // later as an automated follow-up the queue owns.
                    self.queue_followup(address, principal, id, retry_after, "Booked");
                    return Walk::InFlight("Booking now.".to_owned());
                }
                other => return Walk::Failed(turn_text("book", other)),
            }
        }
        Walk::InFlight("Still working on it. Reply STATUS to check.".to_owned())
    }

    /// Apply a walk's disposition to the durable continuation: mark a booked one
    /// so its grant can later cancel it, clear a terminally-failed one, and leave
    /// an in-flight or unreachable one for the resume runner.
    fn settle_walk(&self, walk: Walk, continuation: &Continuation) -> String {
        match walk {
            Walk::Booked(text) => {
                let booked = Continuation {
                    booked: true,
                    ..continuation.clone()
                };
                let _ = self.continuations.record(booked);
                text
            }
            Walk::Failed(text) => {
                let _ = self.continuations.clear(&continuation.challenge_id);
                text
            }
            // Left in place: the booking is submitted (in flight) or the service
            // was briefly unreachable — either way the resume runner finishes it.
            Walk::InFlight(text) | Walk::Unreachable(text) => text,
        }
    }

    /// Map a reply-time error onto a reply, clearing the continuation when the
    /// challenge is gone (nothing left to answer) and keeping it on a wrong code
    /// (the person may retry) or a transport failure (they may retry too).
    fn after_reply_error(&self, continuation: &Continuation, error: &ApprovalError) -> String {
        if let ApprovalError::Gone(_) = error {
            let _ = self.continuations.clear(&continuation.challenge_id);
        }
        approval_error_text(error)
    }

    /// Complete every approved-but-unbooked booking left durable by a crash
    /// between a `YES` and `Booked` (W7). Idempotent: `create` lands on
    /// `Existing`, so a booking already made is read, marked, and not re-made.
    ///
    /// Unlike [`Self::run_followups`], this builds a CHANGER — it completes an
    /// approved effect that was never submitted, where the follow-up converges
    /// one that was (ADR-019). A `reference: None` row (a parked challenge) is
    /// left alone: only a later human `YES` may act on it.
    ///
    /// # STOP halts this, it does not merely mute it
    ///
    /// §14.1: STOP stops automated messaging AND scheduled agent turns. This
    /// runner IS a scheduled agent turn — it commits a booking on its own — so a
    /// suppressed number's owed booking is NOT completed here. Muting only the
    /// outcome would leave the agent quietly booking after a person said stop;
    /// STOP has to actually stop it. The booking is not cancelled: it stays
    /// durable and owed, and `START` re-enables the agent to finish it.
    pub async fn resume(&self) {
        for continuation in self.continuations.take_resumable() {
            if continuation.booked {
                continue; // already done; retained only for CANCEL.
            }
            let Some(reference) = continuation.reference.clone() else {
                continue; // a parked challenge — a human's YES owns it.
            };
            let Request::Book(request) = &continuation.request else {
                continue;
            };
            let Ok(address) =
                ChannelAddress::parse(&continuation.address_revealed, region_of(&continuation))
            else {
                continue;
            };
            // STOP halts the agent's autonomous work: while this number is
            // suppressed, its owed booking is left in place, not completed. START
            // lets a later resume finish it. This is the whole point of STOP
            // meaning "stop", not "carry on silently".
            if self.suppression.is_suppressed(&address) {
                continue;
            }
            // Re-resolve the binding at resume time, and drop on drift — the same
            // guard `run_followups` applies, for the same reason.
            if self.directory.resolve(&address).as_ref() != Some(&continuation.principal) {
                continue;
            }
            let Some(token) = self.credentials.token_for(&continuation.principal) else {
                continue;
            };
            let wire = self
                .wires
                .changer_for(&token, &continuation.principal, &reference);
            let reader = self.wires.reader_for(&token, &continuation.principal);
            let walk = self
                .complete_booking(
                    &continuation.booking_id,
                    request,
                    wire.as_ref(),
                    reader.as_ref(),
                    &address,
                    &continuation.principal,
                )
                .await;
            match walk {
                Walk::Booked(text) => {
                    let booked = Continuation {
                        booked: true,
                        ..continuation.clone()
                    };
                    let _ = self.continuations.record(booked);
                    if !self.suppression.is_suppressed(&address) {
                        let _ = self
                            .channel
                            .send(&address, OutboundMessage::automated(text))
                            .await;
                    }
                }
                Walk::Failed(_) => {
                    let _ = self.continuations.clear(&continuation.challenge_id);
                }
                // Still in flight or briefly unreachable — leave it for the next
                // resume rather than declaring an outcome.
                Walk::InFlight(_) | Walk::Unreachable(_) => {}
            }
        }
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
        // Cancelling is a change, so it needs a delegation reference — and the
        // grant that booked this room already permits `Cancel` over it. Reuse it
        // rather than raise a second challenge; a booking this dispatcher never
        // approved has no reference to present, and cannot be cancelled here.
        let Some(continuation) = self.continuations.load_for_booking(&id) else {
            return "I can only cancel a booking you approved through me. Reply STATUS to see yours.".to_owned();
        };
        let Some(reference) = continuation.reference.clone() else {
            return "That request isn't approved yet — there's nothing booked to cancel."
                .to_owned();
        };
        let wire = wires.changer_with_reference(&reference);
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
                let _ = self.continuations.clear(&continuation.challenge_id);
                match projection.booking_ref {
                    Some(reference) => format!("Cancelled. Council ref {reference}."),
                    None => "Cancelled.".to_owned(),
                }
            }
            Ok(Turn::Committed { state, .. }) => {
                format!("Cancellation under way ({state}). Reply STATUS to check.")
            }
            Ok(Turn::Accepted { retry_after }) => {
                self.queue_followup(&message.address, principal, &id, retry_after, "Cancelled");
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
        // A pending challenge awaiting this caller's YES/NO, as an OPAQUE state
        // string — enough for a real proposer to tell an approval from a fresh
        // request, and nothing it could act on.
        let pending = self.continuations.load(principal).map(|continuation| {
            let state = if continuation.reference.is_some() {
                "approved"
            } else {
                "awaiting-reply"
            };
            PendingSummary {
                state: state.to_owned(),
            }
        });
        ProjectedContext {
            cancellable,
            pending,
        }
    }

    fn queue_followup(
        &self,
        address: &ChannelAddress,
        principal: &PrincipalId,
        booking: &BookingId,
        first_wait: Duration,
        verb: &'static str,
    ) {
        self.followups
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .push(Followup {
                address: address.clone(),
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

/// The code a person sent after `YES`/`NO`, as the dispatcher parses it — NOT the
/// proposer. The classifier only decides "this is an approval"; reading the code
/// is the deterministic seat's job, so the probabilistic one never carries it.
///
/// `word` is ASCII (`"yes"`/`"no"`); `get(..len)` returns `None` rather than
/// panicking if the body opens on a multi-byte character, and an empty code is a
/// bare `YES` — the server then answers with the tries remaining.
fn answer_code(body: &str, word: &str) -> String {
    let trimmed = body.trim();
    match trimmed.get(..word.len()) {
        Some(prefix) if prefix.eq_ignore_ascii_case(word) => {
            trimmed[word.len()..].trim().to_owned()
        }
        _ => String::new(),
    }
}

/// Build the deposit DTO from the FULL message — the identity triple is
/// transport-set (from `InboundIdentity`), never the caller-chosen sender, which
/// is what stops the model seat naming an evidence row into being.
fn inbound_evidence(message: &InboundMessage) -> InboundEvidence {
    InboundEvidence {
        provider: message.identity.provider.clone(),
        account: message.identity.provider_account.clone(),
        message_id: message.identity.provider_message_id.clone(),
        address: message.address.revealed().to_owned(),
        verified: message.transport_evidence.verified(),
        signature: message.transport_evidence.signature().map(str::to_owned),
    }
}

fn approval_error_text(error: &ApprovalError) -> String {
    match error {
        ApprovalError::WrongCode { tries_left } => {
            format!("That code didn't match. {tries_left} attempt(s) left.")
        }
        ApprovalError::Gone(why) => format!("That request is no longer open ({why})."),
        ApprovalError::Transport(why) => {
            format!("I couldn't reach the approval service ({why}). Nothing was changed.")
        }
    }
}

/// The region stored beside a continuation's address, so it can be reparsed on
/// resume. The demo is UK-only and `ChannelAddress` does not expose the region it
/// was parsed with, so the default is stored explicitly rather than guessed.
fn region_tag(_message: &InboundMessage) -> String {
    "Gb".to_owned()
}

fn region_of(_continuation: &Continuation) -> Region {
    Region::Gb
}
