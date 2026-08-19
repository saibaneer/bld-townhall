#![forbid(unsafe_code)]

use async_trait::async_trait;
use bld_kernel::{BoundaryDomain, FactResolution, Resolution, TransitionPlan, Verified};
use bld_types::{
    ActorId, BookingId, BookingRequirements, BoundedString, CouncilBookingRef, EffectIntentId,
    Money, PrincipalId, Provenance, SlotId, TransitionDriver, VenueId,
};
use serde::{Deserialize, Serialize};
use thiserror::Error;

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct Draft;

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct VenueSelected {
    pub venue_id: VenueId,
    pub slot_id: SlotId,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize, Default)]
pub struct NeedsRevalidation {
    /// The venue selection carried forward from the state this was reached
    /// from, so `RevalidateVenue` can bind loaded facts back to what the user
    /// actually chose.
    ///
    /// `None` only for rows persisted before this field existed. Those deny
    /// revalidation and must re-select — fail-closed, because the alternative
    /// is revalidating against a venue nobody can vouch for.
    pub selected: Option<SelectedVenueRef>,
}

/// Accepts a `null` state payload as the default.
///
/// `NeedsRevalidation` was a unit struct, so existing rows carry
/// `{"state":"NeedsRevalidation","data":null}`. Serde will not decode `null`
/// into a struct even with `#[serde(default)]`, so without this every such row
/// fails to load and M3's restart-survival gate breaks.
fn null_as_default<'de, D, T>(deserializer: D) -> Result<T, D::Error>
where
    D: serde::Deserializer<'de>,
    T: Deserialize<'de> + Default,
{
    Ok(Option::<T>::deserialize(deserializer)?.unwrap_or_default())
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct AwaitingBooking {
    pub venue_id: VenueId,
    pub slot_id: SlotId,
    pub verified_fee: Money,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct BookingInProgress {
    pub effect_intent_id: EffectIntentId,
}

/// A cancellation has been asked for while the *booking* is still in flight.
///
/// The effect it names is therefore the **booking** intent, not a cancellation
/// one — nothing has been sent to the council about cancelling yet, and the
/// booking's own outcome is still unknown. Until that resolves there is nothing
/// to cancel and no second identity to mint. The completeness matrix in
/// `docs/state-machine.md` says the same thing: this state's active intent is a
/// booking, and `BookingExists` is what moves it on to `CancellingBooking`.
///
/// Losing that pointer would be the real cost of leaving this a unit struct: a
/// crash here would find a booking whose council request is outstanding and no
/// record of which effect to reconcile.
///
/// Not yet *enterable* — `BookingInProgress + Cancel` lands in slice F with its
/// compensation protocol — but as of B3b its outbound fact edges are live:
/// `BookingExists` moves it to `CancellingBooking`, and absence or rejection of
/// the booking resolves it to `Cancelled`, because there was never anything to
/// cancel.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct CancellationRequested {
    pub effect_intent_id: EffectIntentId,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct Booked {
    pub booking_ref: CouncilBookingRef,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct CancellingBooking {
    pub booking_ref: CouncilBookingRef,
    /// The cancellation effect this state is waiting on.
    ///
    /// Symmetric with `BookingInProgress`. Without it the repository's
    /// state-versus-intent verification covers the booking path and silently
    /// skips the cancellation one, which is exactly the asymmetry that lets a
    /// recovery bug live in the half nobody looked at.
    pub effect_intent_id: EffectIntentId,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct Cancelled;

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct NeedsHuman;

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "state", content = "data")]
pub enum BookingState {
    Draft(Draft),
    VenueSelected(VenueSelected),
    NeedsRevalidation(#[serde(deserialize_with = "null_as_default")] NeedsRevalidation),
    AwaitingBooking(AwaitingBooking),
    BookingInProgress(BookingInProgress),
    CancellationRequested(CancellationRequested),
    Booked(Booked),
    CancellingBooking(CancellingBooking),
    Cancelled(Cancelled),
    NeedsHuman(NeedsHuman),
}

impl BookingState {
    #[must_use]
    pub const fn name(&self) -> &'static str {
        match self {
            Self::Draft(_) => "Draft",
            Self::VenueSelected(_) => "VenueSelected",
            Self::NeedsRevalidation(_) => "NeedsRevalidation",
            Self::AwaitingBooking(_) => "AwaitingBooking",
            Self::BookingInProgress(_) => "BookingInProgress",
            Self::CancellationRequested(_) => "CancellationRequested",
            Self::Booked(_) => "Booked",
            Self::CancellingBooking(_) => "CancellingBooking",
            Self::Cancelled(_) => "Cancelled",
            Self::NeedsHuman(_) => "NeedsHuman",
        }
    }
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub enum BookingProposal {
    SelectVenue { venue_id: VenueId, slot_id: SlotId },
    VerifySlot,
    ChangeVenue,
    UpdateRequirements { attendees: Option<u16> },
    RevalidateVenue,
    Book,
    Cancel { reason: String },
    // No `Reconcile`. Recovery is runtime machinery, not a business intention:
    // it must run when the model is offline, hostile or absent, so it cannot
    // depend on the model asking for it (ADR-012). Convergence arrives through
    // the verified-fact door instead.
}

impl BookingState {
    /// The effect identity this state carries, if it is an in-flight state.
    #[must_use]
    pub const fn effect_intent_id(&self) -> Option<&EffectIntentId> {
        match self {
            Self::BookingInProgress(in_progress) => Some(&in_progress.effect_intent_id),
            Self::CancellationRequested(requested) => Some(&requested.effect_intent_id),
            Self::CancellingBooking(cancelling) => Some(&cancelling.effect_intent_id),
            _ => None,
        }
    }

    /// The venue selection this state names, if it names one.
    ///
    /// Some states record the selection inside themselves as well as the
    /// aggregate recording it in `selected_venue`. This is the state's copy, so
    /// the two can be compared — see [`Booking::coherent`].
    #[must_use]
    pub fn selection(&self) -> Option<SelectedVenueRef> {
        match self {
            Self::VenueSelected(selected) => Some(SelectedVenueRef {
                venue_id: selected.venue_id.clone(),
                slot_id: selected.slot_id.clone(),
            }),
            Self::AwaitingBooking(waiting) => Some(SelectedVenueRef {
                venue_id: waiting.venue_id.clone(),
                slot_id: waiting.slot_id.clone(),
            }),
            Self::NeedsRevalidation(pending) => pending.selected.clone(),
            _ => None,
        }
    }

    /// The council reference this state names, if it names one.
    #[must_use]
    pub const fn council_booking_ref(&self) -> Option<&CouncilBookingRef> {
        match self {
            Self::Booked(booked) => Some(&booked.booking_ref),
            Self::CancellingBooking(cancelling) => Some(&cancelling.booking_ref),
            _ => None,
        }
    }

    /// Which kind of effect this state is waiting on, if it is in flight.
    ///
    /// An effect id does not encode its kind, so matching ids is not enough to
    /// prove a state and an effect belong together: `BookingInProgress` waiting
    /// on a cancellation is nonsense that an id comparison alone would accept.
    // `BookingInProgress` and `CancellationRequested` both answer `Book`, and
    // clippy wants them merged into one arm. Kept separate deliberately: they
    // answer the same way for entirely different reasons, and
    // `CancellationRequested => Book` is the surprising one. Folded into
    // `(A | B) => Book` it reads as an obvious pair, which is exactly the wrong
    // impression — a reader would stop asking why a state named for a
    // cancellation waits on a booking.
    #[allow(clippy::match_same_arms)]
    #[must_use]
    pub const fn in_flight_kind(&self) -> Option<OperationKind> {
        match self {
            Self::BookingInProgress(_) => Some(OperationKind::Book),
            // Deliberately `Book`. `CancellationRequested` means "cancel the
            // booking we are still waiting on", so the effect in flight is the
            // booking's — see [`CancellationRequested`]. Reading the name and
            // answering `Cancel` here would let a cancellation outcome resolve a
            // booking request.
            Self::CancellationRequested(_) => Some(OperationKind::Book),
            Self::CancellingBooking(_) => Some(OperationKind::Cancel),
            _ => None,
        }
    }
}

impl BookingProposal {
    #[must_use]
    pub const fn name(&self) -> &'static str {
        match self {
            Self::SelectVenue { .. } => "SelectVenue",
            Self::VerifySlot => "VerifySlot",
            Self::ChangeVenue => "ChangeVenue",
            Self::UpdateRequirements { .. } => "UpdateRequirements",
            Self::RevalidateVenue => "RevalidateVenue",
            Self::Book => "Book",
            Self::Cancel { .. } => "Cancel",
        }
    }
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct VerifiedAuthority {
    pub principal: PrincipalId,
    pub actor: ActorId,
    pub max_fee: Money,
    pub may_book: bool,
    pub may_cancel: bool,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct VenueFacts {
    pub venue_id: VenueId,
    pub slot_id: SlotId,
    pub capacity: u16,
    pub wheelchair_accessible: bool,
    pub fee: Money,
    pub available: bool,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct SelectedVenueRef {
    pub venue_id: VenueId,
    pub slot_id: SlotId,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct BookingAggregate {
    pub id: BookingId,
    pub version: u64,
    pub state: BookingState,
    pub requirements: BookingRequirements,
    pub selected_venue: Option<SelectedVenueRef>,
    pub availability: Option<VenueFacts>,
    pub booking_ref: Option<CouncilBookingRef>,
    pub active_effect: Option<EffectIntentId>,
    pub created_at_ms: i64,
    pub updated_at_ms: i64,
}

/// Every business field of a booking, as decided by the domain.
///
/// This is `BoundaryDomain::State`, and it is deliberately the whole thing
/// rather than the state discriminator alone (ADR-013). The repository owns
/// `version`, `created_at_ms` and `updated_at_ms`; the domain owns everything
/// here.
///
/// # Why not just `BookingState`
///
/// B2 used `BookingState`, which left `requirements`, `selected_venue`,
/// `availability`, `booking_ref` and `active_effect` to whoever assembled the
/// repository's write value. That is domain mutation semantics living outside
/// the domain, and it had already produced a real defect:
/// `UpdateRequirements { attendees }` could not apply its own patch, because a
/// plan carrying only a state has nowhere to put changed requirements. The
/// headcount was silently dropped and the next capacity check validated against
/// the old one.
///
/// So the fix is not "remember to set the fields" — it is making the complete
/// value the only thing a transition can produce.
///
/// `id` is here for a second reason. Evidence must be bound to *this resource*
/// (ADR-012), and only the authoritatively loaded aggregate can establish which
/// resource that is. Binding against a caller-supplied id would compare two
/// values from the same source and prove nothing.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Booking {
    pub id: BookingId,
    pub state: BookingState,
    pub requirements: BookingRequirements,
    pub selected_venue: Option<SelectedVenueRef>,
    pub availability: Option<VenueFacts>,
    pub booking_ref: Option<CouncilBookingRef>,
    pub active_effect: Option<EffectIntentId>,
}

/// A booking that contradicts itself.
///
/// Three facts are recorded twice: the effect an in-flight state waits on is
/// also `active_effect`, the venue a state names is also `selected_venue`, and
/// the council reference a state names is also `booking_ref`. The duplication is
/// deliberate — the outer fields are what recovery and queries read without
/// destructuring the state — but two copies of one fact can disagree.
///
/// A disagreement is **not** repaired. The transition that produced it was
/// wrong, and silently reconciling the two copies would launder that away; the
/// next reader would see a consistent booking and never learn one was written
/// incorrectly.
#[derive(Clone, Debug, PartialEq, Eq, Error)]
pub enum IncoherentBooking {
    #[error("state {state} waits on {state_says:?}, but active_effect is {aggregate_says:?}")]
    EffectIdentity {
        state: &'static str,
        state_says: Option<EffectIntentId>,
        aggregate_says: Option<EffectIntentId>,
    },
    #[error("state {state} names venue {state_says:?}, but selected_venue is {aggregate_says:?}")]
    Selection {
        state: &'static str,
        state_says: Box<SelectedVenueRef>,
        aggregate_says: Option<Box<SelectedVenueRef>>,
    },
    #[error("state {state} names booking {state_says}, but booking_ref is {aggregate_says:?}")]
    CouncilReference {
        state: &'static str,
        state_says: CouncilBookingRef,
        aggregate_says: Option<CouncilBookingRef>,
    },
    #[error("state {state} cannot know a council reference, but booking_ref is {aggregate_says}")]
    PhantomReference {
        state: &'static str,
        aggregate_says: CouncilBookingRef,
    },
}

impl Booking {
    /// Whether the booking's two copies of each duplicated fact agree.
    ///
    /// The domain owns this because *which fields are copies of which* is domain
    /// knowledge — the repository must not learn that `BookingInProgress`
    /// carries the same identity as `active_effect` (ADR-013). The repository
    /// asks; it does not know why.
    ///
    /// Checked on write and on read, so an incoherent booking can neither be
    /// persisted nor loaded. That is what lets every transition arm carry these
    /// fields through rather than defensively re-deriving them in each of eight
    /// places — one enforced invariant beats a discipline eight arms must
    /// remember.
    ///
    /// The effect pointer is checked in **both** directions: a state waiting on
    /// nothing must not have an `active_effect` either, or recovery would chase
    /// an effect no state is expecting. Selection and reference are checked only
    /// where the state names one, because a terminal state legitimately retains
    /// an outer value it no longer names — `Cancelled` keeps the reference of
    /// the booking it cancelled.
    ///
    /// # Errors
    /// One [`IncoherentBooking`] per disagreement, first found.
    pub fn coherent(&self) -> Result<(), IncoherentBooking> {
        let state = self.state.name();

        if self.state.effect_intent_id() != self.active_effect.as_ref() {
            return Err(IncoherentBooking::EffectIdentity {
                state,
                state_says: self.state.effect_intent_id().cloned(),
                aggregate_says: self.active_effect.clone(),
            });
        }

        if let Some(named) = self.state.selection()
            && self.selected_venue.as_ref() != Some(&named)
        {
            return Err(IncoherentBooking::Selection {
                state,
                state_says: Box::new(named),
                aggregate_says: self.selected_venue.clone().map(Box::new),
            });
        }

        if let Some(named) = self.state.council_booking_ref()
            && self.booking_ref.as_ref() != Some(named)
        {
            return Err(IncoherentBooking::CouncilReference {
                state,
                state_says: named.clone(),
                aggregate_says: self.booking_ref.clone(),
            });
        }

        // The reference rule is per-state, not merely forward-only. B3b's
        // review found the gap: a forward-only rule let `BookingInProgress`
        // carry a phantom `booking_ref`, which the fact door then silently
        // cleared — a bad write laundered into a clean transition. A state
        // whose booking has never been confirmed cannot honestly know a
        // council reference, so carrying one is a contradiction:
        //
        //   must be None      Draft, VenueSelected, NeedsRevalidation,
        //                     AwaitingBooking, BookingInProgress,
        //                     CancellationRequested
        //   must match state  Booked, CancellingBooking (checked above)
        //   unconstrained     Cancelled (kept, or never existed) and
        //                     NeedsHuman (frozen with whatever was known)
        let cannot_know_a_reference = matches!(
            self.state,
            BookingState::Draft(_)
                | BookingState::VenueSelected(_)
                | BookingState::NeedsRevalidation(_)
                | BookingState::AwaitingBooking(_)
                | BookingState::BookingInProgress(_)
                | BookingState::CancellationRequested(_)
        );
        if cannot_know_a_reference && let Some(phantom) = self.booking_ref.as_ref() {
            return Err(IncoherentBooking::PhantomReference {
                state,
                aggregate_says: phantom.clone(),
            });
        }

        Ok(())
    }
}

impl From<&BookingAggregate> for Booking {
    fn from(value: &BookingAggregate) -> Self {
        Self {
            id: value.id.clone(),
            state: value.state.clone(),
            requirements: value.requirements.clone(),
            selected_venue: value.selected_venue.clone(),
            availability: value.availability.clone(),
            booking_ref: value.booking_ref.clone(),
            active_effect: value.active_effect.clone(),
        }
    }
}

/// Which external consequence an effect intent represents.
///
/// Part of the uniqueness key, because a booking and its cancellation are two
/// effects with two identities. Reusing one id for both would make "has this
/// effect completed?" unanswerable.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub enum OperationKind {
    Book,
    Cancel,
}

impl OperationKind {
    #[must_use]
    pub const fn name(self) -> &'static str {
        match self {
            Self::Book => "Book",
            Self::Cancel => "Cancel",
        }
    }

    /// Parse the persisted discriminator.
    ///
    /// # Errors
    /// Returns the unrecognised text if it is not a known operation kind.
    pub fn parse(text: &str) -> Result<Self, String> {
        match text {
            "Book" => Ok(Self::Book),
            "Cancel" => Ok(Self::Cancel),
            other => Err(other.to_owned()),
        }
    }
}

/// Lifecycle of an intended external consequence.
///
/// `Prepared` means the intent is durable but nothing external has been
/// attempted. Everything after that is set by evidence, never by optimism.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub enum EffectStatus {
    /// Persisted; the capability has not been called.
    Prepared,
    /// The capability was called and the outcome is not yet known. Timeout
    /// lands here, because timeout is neither success nor failure.
    Unknown,
    /// Verified evidence confirmed the effect happened.
    Confirmed,
    /// The provider authoritatively refused, durably.
    Rejected,
    /// The council tombstoned the intent: it never happened and never can.
    Absent,
}

impl EffectStatus {
    #[must_use]
    pub const fn name(self) -> &'static str {
        match self {
            Self::Prepared => "Prepared",
            Self::Unknown => "Unknown",
            Self::Confirmed => "Confirmed",
            Self::Rejected => "Rejected",
            Self::Absent => "Absent",
        }
    }

    /// Whether this outcome is settled and can never change.
    #[must_use]
    pub const fn is_terminal(self) -> bool {
        matches!(self, Self::Confirmed | Self::Rejected | Self::Absent)
    }

    /// Parse the persisted discriminator.
    ///
    /// # Errors
    /// Returns the unrecognised text if it is not a known status.
    pub fn parse(text: &str) -> Result<Self, String> {
        match text {
            "Prepared" => Ok(Self::Prepared),
            "Unknown" => Ok(Self::Unknown),
            "Confirmed" => Ok(Self::Confirmed),
            "Rejected" => Ok(Self::Rejected),
            "Absent" => Ok(Self::Absent),
            other => Err(other.to_owned()),
        }
    }
}

/// A durable record of one intended external consequence.
///
/// Persisted in the same transaction as the state transition that creates it,
/// and always before the capability is called (ADR-014).
///
/// No serde derives, and they were never load-bearing: the repository builds this
/// field by field from a row and serialises `canonical_plan` on its own, so
/// nothing ever round-tripped the struct whole. Dropping them is what lets
/// `outcome_detail` be a [`BoundedString`], which deliberately has no serde impls
/// because it also lives inside [`VerifiedProviderFact`] — and deserialising
/// verified evidence is the forgery ADR-012 exists to prevent.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct EffectIntent {
    pub effect_intent_id: EffectIntentId,
    pub booking_id: BookingId,
    pub operation_kind: OperationKind,
    /// The aggregate version this effect was derived from. Part of the
    /// uniqueness key.
    pub source_version: u64,
    pub canonical_plan: BookingEffect,
    pub status: EffectStatus,
    /// ADR-016. Sent to the council on create and on lookup; absence is only
    /// definitive once the council has tombstoned the intent past this.
    pub expires_at_ms: i64,
    pub provider_reference: Option<CouncilBookingRef>,
    /// Why, where the outcome had a reason worth keeping — a rejection's text
    /// lands here. Without it two `Rejected` intents are indistinguishable:
    /// both terminal, both referenceless.
    pub outcome_detail: Option<BoundedString>,
    /// What this effect replaced, for the one transition that hands off rather
    /// than ends — `CancellationRequested + BookingExists` finalises the booking
    /// intent and creates the cancellation in one transaction. The successor's
    /// uniqueness key names only the successor, so without this a replay cannot
    /// tell "this exact handoff already happened" from "a different predecessor
    /// produced a same-key successor".
    pub supersedes: Option<EffectIntentId>,
    pub created_at_ms: i64,
    pub updated_at_ms: i64,
}

/// What the coordinator must supply that the booking itself cannot know.
///
/// Deliberately small. It carried `booking_id` and `requirements` until B3a,
/// and both were removed rather than tidied: they duplicated authoritative
/// fields on [`Booking`], and a duplicated field is one a guard can read the
/// stale copy of. `bind_facts` did exactly that — it validated against
/// `context.requirements`, so patching the aggregate's requirements would have
/// fixed the `UpdateRequirements` bug in one place and left it live in another.
#[derive(Clone, Debug)]
pub struct BookingContext {
    /// Facts loaded by a capability. Never authoritative on their own: every
    /// behaviour that consumes them must first bind them to what the user
    /// actually chose, which lives in the *booking*, not here.
    pub selected_facts: Option<VenueFacts>,
    /// The effect identity the coordinator derived for this turn.
    ///
    /// The domain cannot derive it: the repository owns effect identity because
    /// it holds the uniqueness key. So the coordinator derives it with the
    /// repository's own function and supplies it here, and the repository then
    /// verifies that the state the domain produced carries the same value —
    /// trust-but-verify rather than trust.
    ///
    /// `None` on any turn that cannot produce an external effect. A behaviour
    /// that needs one and finds this absent is `Denied`, never a guess.
    pub pending_effect: Option<EffectIntentId>,
}

/// An intended external consequence, derived by the boundary.
///
/// This is the canonical plan that gets persisted before the capability is
/// invoked, and that later provider evidence is bound against.
///
/// It deliberately does **not** carry an `EffectIntentId`. The repository owns
/// effect identity — it holds the uniqueness key — so a plan carrying its own id
/// would be a second place for that value to live and drift. Slice A already
/// had to add a guard against exactly that; removing the field removes the need
/// for it here.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub enum BookingEffect {
    /// Book the verified venue for the verified fee.
    Book {
        principal: PrincipalId,
        /// The headcount this booking was authorised for. `facts.capacity` is
        /// the *room's* limit, not the party's size — without this field the
        /// one number the capacity guard actually checked would be unbindable
        /// when provider evidence comes back, exactly where ADR-012 says fee
        /// and headcount are not optional.
        attendees: u16,
        facts: VenueFacts,
    },
    /// Cancel a council booking that is known to exist.
    CancelBooking { booking_ref: CouncilBookingRef },
}

impl BookingEffect {
    /// The provider reference this plan operates on, where it operates on one.
    ///
    /// `Book` creates something that does not exist yet, so it names nothing.
    /// `CancelBooking` acts on a booking the council already made.
    ///
    /// This is what lets the repository check a handoff without knowing what a
    /// booking is: a successor effect exists *because* its predecessor succeeded,
    /// so the successor must act on the reference the predecessor produced. The
    /// domain decides what "acts on" means per variant; the repository only
    /// compares.
    #[must_use]
    pub const fn acts_on(&self) -> Option<&CouncilBookingRef> {
        match self {
            Self::Book { .. } => None,
            Self::CancelBooking { booking_ref } => Some(booking_ref),
        }
    }

    /// Which kind of consequence this is. Part of the effect uniqueness key: a
    /// booking and its cancellation are two effects with two identities.
    #[must_use]
    pub const fn operation_kind(&self) -> OperationKind {
        match self {
            Self::Book { .. } => OperationKind::Book,
            Self::CancelBooking { .. } => OperationKind::Cancel,
        }
    }
}

#[derive(Clone, Debug, Error, PartialEq, Eq)]
pub enum BookingError {
    #[error("booking authority is required")]
    BookingAuthorityRequired,
    #[error("cancellation authority is required")]
    CancellationAuthorityRequired,
    #[error("venue facts are missing")]
    VenueFactsMissing,
    #[error("slot is unavailable")]
    SlotUnavailable,
    #[error("venue capacity {capacity} is below required {required}")]
    CapacityInsufficient { capacity: u16, required: u16 },
    #[error("venue is not wheelchair accessible")]
    AccessibilityRequired,
    #[error("venue fee exceeds effective maximum")]
    FeeExceeded,
    #[error("no effect identity was supplied for a transition that needs one")]
    EffectIdentityMissing,
    #[error("a state that participates in the fact door was given no intent to bind against")]
    EffectPlanMissing,
    #[error("the evidence does not concern the effect this booking is running")]
    EffectMismatch,
    #[error("the evidence's kind and the effect's kind disagree")]
    EffectKindMismatch,
    #[error("the evidence's {field} disagrees with the persisted canonical plan")]
    EffectPlanMismatch { field: &'static str },
    #[error("one effect identity has resolved to two different provider references")]
    DuplicateProviderEffect,
    #[error("the evidence contradicts a durable determination already recorded")]
    ContradictoryProviderFact,
    #[error("the aggregate's state and its active_effect disagree about what is in flight")]
    InconsistentEffectIdentity,
    #[error("the aggregate contradicts itself: {0}")]
    IncoherentAggregate(IncoherentBooking),
    #[error("the effect intent contradicts itself: {0}")]
    IncoherentIntent(IncoherentIntent),
}

/// Externally verified reality. State-neutral: the verifier establishes *what
/// is true*, the domain decides *what it means here* — one `EffectAbsent` fact
/// means three different things at three different states (ADR-012).
///
/// Canonical definition lives in ADR-012; this is its executable form. Every
/// consequential field is bound against the persisted canonical plan before any
/// meaning is derived.
///
/// **No `Serialize`, no `Deserialize`** — a fact enters the system through a
/// verifier or not at all. Rust cannot make enum variant fields private, so
/// unlike [`bld_kernel::Verified`] the protection here is the missing serde
/// impls plus the crate graph: the untrusted half cannot name this type.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum VerifiedProviderFact {
    /// A booking exists at the council for this intent.
    BookingExists {
        effect_intent_id: EffectIntentId,
        booking_ref: CouncilBookingRef,
        venue_id: VenueId,
        slot_id: SlotId,
        attendees: u16,
        fee: Money,
        principal: PrincipalId,
    },
    /// Nothing was created for this intent, and nothing ever can be.
    ///
    /// Deliberately kind-agnostic: absence carries only the identity, so one
    /// variant covers a booking intent and a cancellation intent alike. Which
    /// it means comes from the persisted intent and the current state.
    ///
    /// Admissible only from the council's definitive-absence response, which
    /// tombstones the intent (ADR-016). Anything weaker is `Unknown` and
    /// drives nothing.
    EffectAbsent { effect_intent_id: EffectIntentId },
    /// A cancellation exists at the council for this intent.
    CancellationExists {
        effect_intent_id: EffectIntentId,
        booking_ref: CouncilBookingRef,
    },
    /// The provider authoritatively and durably refused this intent.
    ProviderRejected {
        effect_intent_id: EffectIntentId,
        reason: BoundedString,
    },
}

impl VerifiedProviderFact {
    #[must_use]
    pub const fn name(&self) -> &'static str {
        match self {
            Self::BookingExists { .. } => "BookingExists",
            Self::EffectAbsent { .. } => "EffectAbsent",
            Self::CancellationExists { .. } => "CancellationExists",
            Self::ProviderRejected { .. } => "ProviderRejected",
        }
    }

    /// The effect identity this fact claims to be about.
    #[must_use]
    pub const fn effect_intent_id(&self) -> &EffectIntentId {
        match self {
            Self::BookingExists {
                effect_intent_id, ..
            }
            | Self::EffectAbsent {
                effect_intent_id, ..
            }
            | Self::CancellationExists {
                effect_intent_id, ..
            }
            | Self::ProviderRejected {
                effect_intent_id, ..
            } => effect_intent_id,
        }
    }

    /// The operation kind this fact implies, where it implies one.
    ///
    /// `EffectAbsent` and `ProviderRejected` are deliberately `None`: absence
    /// and refusal carry no kind of their own and take it from the persisted
    /// intent — which is exactly why the state-vs-intent kind check must run
    /// separately, or a cancellation intent could answer a booking state.
    #[must_use]
    pub const fn implied_kind(&self) -> Option<OperationKind> {
        match self {
            Self::BookingExists { .. } => Some(OperationKind::Book),
            Self::CancellationExists { .. } => Some(OperationKind::Cancel),
            Self::EffectAbsent { .. } | Self::ProviderRejected { .. } => None,
        }
    }

    /// The provider reference this fact carries, if it names one.
    #[must_use]
    pub const fn provider_reference(&self) -> Option<&CouncilBookingRef> {
        match self {
            Self::BookingExists { booking_ref, .. }
            | Self::CancellationExists { booking_ref, .. } => Some(booking_ref),
            Self::EffectAbsent { .. } | Self::ProviderRejected { .. } => None,
        }
    }

    /// Whether this fact asserts the effect happened, as opposed to asserting
    /// it did not and never will.
    #[must_use]
    pub const fn asserts_existence(&self) -> bool {
        matches!(
            self,
            Self::BookingExists { .. } | Self::CancellationExists { .. }
        )
    }
}

/// A deterministic runtime fact. Neither intent nor external truth: the
/// council cannot tell us our own retry budget is exhausted (ADR-012).
///
/// Exactly one variant in M4. Deriving it from durable retry accounting is
/// slice E's work; this door exists so `NeedsHuman` is reachable at all.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum SystemEvent {
    ReconciliationExhausted { effect_intent_id: EffectIntentId },
}

// The three provenance impls. Each type's class is fixed here and nowhere else,
// so an audit record cannot be mislabelled by whoever assembles it — see
// `TransitionDriver`.

impl TransitionDriver for BookingProposal {
    fn provenance(&self) -> Provenance {
        Provenance::Proposal
    }

    fn driver_name(&self) -> &'static str {
        self.name()
    }
}

impl TransitionDriver for VerifiedProviderFact {
    fn provenance(&self) -> Provenance {
        Provenance::Fact
    }

    fn driver_name(&self) -> &'static str {
        self.name()
    }
}

impl TransitionDriver for SystemEvent {
    fn provenance(&self) -> Provenance {
        Provenance::SystemEvent
    }

    fn driver_name(&self) -> &'static str {
        match self {
            Self::ReconciliationExhausted { .. } => "ReconciliationExhausted",
        }
    }
}

/// What the coordinator supplies for fact binding.
///
/// Deliberately not [`BookingContext`]: that carries `selected_facts`, loaded
/// by a capability, and the fact door must never bind against those — it binds
/// against the *persisted* canonical plan. A context that cannot even name
/// capability-loaded facts makes that structural rather than a discipline.
/// What a verified fact establishes about the effect it concerns.
///
/// The three fields travel together so they cannot be mismatched. Deriving them
/// from the fact — rather than letting a coordinator assemble them — means the
/// status and its reference always agree, so a shape
/// [`EffectIntent::coherent`] would refuse cannot be constructed on this path at
/// all.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct EstablishedOutcome {
    pub status: EffectStatus,
    pub provider_reference: Option<CouncilBookingRef>,
    pub detail: Option<BoundedString>,
}

impl VerifiedProviderFact {
    /// The terminal outcome this fact establishes for its effect.
    #[must_use]
    pub fn establishes(&self) -> EstablishedOutcome {
        match self {
            Self::BookingExists { booking_ref, .. }
            | Self::CancellationExists { booking_ref, .. } => EstablishedOutcome {
                status: EffectStatus::Confirmed,
                provider_reference: Some(booking_ref.clone()),
                detail: None,
            },
            Self::EffectAbsent { .. } => EstablishedOutcome {
                status: EffectStatus::Absent,
                provider_reference: None,
                detail: None,
            },
            Self::ProviderRejected { reason, .. } => EstablishedOutcome {
                status: EffectStatus::Rejected,
                provider_reference: None,
                detail: Some(reason.clone()),
            },
        }
    }
}

/// An effect intent that contradicts itself.
///
/// The same treatment [`IncoherentBooking`] gets, for the same reason. Slice C1
/// gave the booking one definition of self-consistency and gated it on read and
/// write; the intent was skipped, and the gap showed up as a symptom — the fact
/// door hand-rolled the shape rule and the kind-versus-plan comparison, in three
/// places, precisely because nothing guaranteed a loaded row was sound.
///
/// One definition, called from wherever a row arrives, is the fix. Multiple call
/// sites of one function is the pattern; two implementations of one rule is the
/// defect.
#[derive(Clone, Debug, PartialEq, Eq, Error)]
pub enum IncoherentIntent {
    #[error("status {status} may not carry {}a provider reference", if *.has_reference { "" } else { "no " })]
    OutcomeShape {
        status: &'static str,
        has_reference: bool,
    },
    #[error("a Prepared effect has attempted nothing, so it can have nothing to explain")]
    PrematureDetail,
    #[error("the intent is recorded as {column} but its plan is a {plan}")]
    KindDisagreesWithPlan {
        column: &'static str,
        plan: &'static str,
    },
    #[error("effect {0} supersedes itself")]
    SupersedesItself(EffectIntentId),
}

impl EffectIntent {
    /// Whether this intent's own fields agree with each other.
    ///
    /// # Errors
    /// One [`IncoherentIntent`] per disagreement, first found.
    pub fn coherent(&self) -> Result<(), IncoherentIntent> {
        // A confirmed effect exists, so the provider named it. Nothing else does
        // — a reference on any other status names an effect that officially never
        // happened, and a `Confirmed` without one would converge against *any*
        // reference the fact door was shown.
        let has_reference = self.provider_reference.is_some();
        let shape_ok = match self.status {
            EffectStatus::Confirmed => has_reference,
            EffectStatus::Prepared
            | EffectStatus::Unknown
            | EffectStatus::Absent
            | EffectStatus::Rejected => !has_reference,
        };
        if !shape_ok {
            return Err(IncoherentIntent::OutcomeShape {
                status: self.status.name(),
                has_reference,
            });
        }

        // `Prepared` means the capability has not been called. `Unknown` means it
        // has and the answer was ambiguous — there a detail is genuinely useful,
        // so only the first is constrained.
        if self.status == EffectStatus::Prepared && self.outcome_detail.is_some() {
            return Err(IncoherentIntent::PrematureDetail);
        }

        // Two copies of one fact: the persisted discriminator and the plan it
        // describes. `PrepareEffect` derives the column *from* the plan, so they
        // agree by construction on the way in — this is what catches a row where
        // they no longer do.
        let plan_kind = self.canonical_plan.operation_kind();
        if self.operation_kind != plan_kind {
            return Err(IncoherentIntent::KindDisagreesWithPlan {
                column: self.operation_kind.name(),
                plan: plan_kind.name(),
            });
        }

        if self.supersedes.as_ref() == Some(&self.effect_intent_id) {
            return Err(IncoherentIntent::SupersedesItself(
                self.effect_intent_id.clone(),
            ));
        }

        Ok(())
    }
}

#[derive(Clone, Debug)]
pub struct FactContext {
    /// The persisted effect intent this fact is about, loaded by the fact's
    /// identity. The binding source of truth. `None` is refused, never guessed.
    pub intent: Option<EffectIntent>,
    /// A fresh identity for the one fact-driven transition that starts a NEW
    /// external effect: `CancellationRequested + BookingExists` mints the
    /// cancellation. `None` on any turn that cannot start one.
    pub pending_effect: Option<EffectIntentId>,
}

/// Which of the fact door's three categories a state falls into.
///
/// Decided by the state alone, before any guard runs or any context is read —
/// `Absent` must return `Undefined` without consulting anything, or irrelevant
/// context could manufacture behaviour in a state that has none (ADR-012's
/// `BookingExists + Draft -> Undefined`).
enum FactCategory {
    /// An effect is in flight; a fact may answer it.
    Waiting,
    /// A fact-driven edge lands here; an arriving fact must already be
    /// reflected, or it contradicts how we got here.
    Settled,
    /// Neither in flight nor fact-reachable. No fact behaviour exists.
    Absent,
}

const fn fact_category(state: &BookingState) -> FactCategory {
    match state {
        BookingState::BookingInProgress(_)
        | BookingState::CancellationRequested(_)
        | BookingState::CancellingBooking(_) => FactCategory::Waiting,
        BookingState::AwaitingBooking(_) | BookingState::Booked(_) | BookingState::Cancelled(_) => {
            FactCategory::Settled
        }
        BookingState::Draft(_)
        | BookingState::VenueSelected(_)
        | BookingState::NeedsRevalidation(_)
        | BookingState::NeedsHuman(_) => FactCategory::Absent,
    }
}

#[derive(Clone, Copy, Debug, Default)]
pub struct TownHallDomain;

impl TownHallDomain {
    /// Which kind of effect a proposal would set in motion here, if any.
    ///
    /// A coordinator needs this *before* classifying, because the effect identity
    /// is an input to classification and the identity's derivation includes the
    /// operation kind. So the question has to be answerable one step early.
    ///
    /// It restates two facts the transition graph already knows, which is exactly
    /// the duplication this project keeps finding bugs in — so it is pinned by a
    /// sweep over all seventy cells asserting it agrees with `resolve_proposal`
    /// about which cells are external and what kind they carry. Stating a fact
    /// twice is survivable when the agreement is proved; it is the unproved kind
    /// that rots.
    #[must_use]
    pub const fn intended_effect_kind(
        state: &BookingState,
        proposal: &BookingProposal,
    ) -> Option<OperationKind> {
        match (state, proposal) {
            (BookingState::AwaitingBooking(_), BookingProposal::Book) => Some(OperationKind::Book),
            (BookingState::Booked(_), BookingProposal::Cancel { .. }) => {
                Some(OperationKind::Cancel)
            }
            _ => None,
        }
    }

    /// The same question for the fact door, which has exactly one external edge.
    #[must_use]
    pub const fn fact_intended_effect_kind(
        state: &BookingState,
        fact: &VerifiedProviderFact,
    ) -> Option<OperationKind> {
        match (state, fact) {
            (
                BookingState::CancellationRequested(_),
                VerifiedProviderFact::BookingExists { .. },
            ) => Some(OperationKind::Cancel),
            _ => None,
        }
    }
}

impl TownHallDomain {
    fn effective_max_fee(
        requirements: &BookingRequirements,
        authority: &VerifiedAuthority,
    ) -> Money {
        Money::from_pence(requirements.max_fee.pence().min(authority.max_fee.pence()))
    }

    fn validate_facts(
        facts: &VenueFacts,
        requirements: &BookingRequirements,
        authority: &VerifiedAuthority,
    ) -> Result<(), BookingError> {
        if !facts.available {
            return Err(BookingError::SlotUnavailable);
        }
        if facts.capacity < requirements.attendees {
            return Err(BookingError::CapacityInsufficient {
                capacity: facts.capacity,
                required: requirements.attendees,
            });
        }
        if requirements.wheelchair_accessible && !facts.wheelchair_accessible {
            return Err(BookingError::AccessibilityRequired);
        }
        if facts.fee > Self::effective_max_fee(requirements, authority) {
            return Err(BookingError::FeeExceeded);
        }
        Ok(())
    }
}

/// Where a fact arrived from, for the one judgement that differs by origin.
///
/// A fact that binds cleanly but fits no edge means different things at the two
/// categories: at a Waiting state it is evidence about the wrong effect
/// (`EffectMismatch`); at a Settled state it contradicts how the state was
/// reached (`ContradictoryProviderFact`). Same fallthrough, different honest
/// reason.
#[derive(Clone, Copy)]
enum FactOrigin {
    Waiting,
    Settled,
}

impl TownHallDomain {
    /// Steps I and B: everything checkable without asking what the state means.
    ///
    /// Runs after the category test (a state with no fact behaviour must return
    /// `Undefined` before any of this is consulted) and before any
    /// state-relative meaning is derived, so a contradiction is caught no
    /// matter which state the fact lands in — the ADR-016 hazard is a
    /// `BookingExists` arriving *after* its intent was tombstoned, and the
    /// state it lands in is unpredictable by construction.
    fn bind_fact<'a>(
        booking: &Booking,
        fact: &VerifiedProviderFact,
        context: &'a FactContext,
    ) -> Result<&'a EffectIntent, BookingError> {
        // I. Waiting and Settled states both require the persisted intent.
        let Some(intent) = context.intent.as_ref() else {
            return Err(BookingError::EffectPlanMissing);
        };

        // B1. The supplied intent is the fact's.
        if intent.effect_intent_id != *fact.effect_intent_id() {
            return Err(BookingError::EffectMismatch);
        }
        // B2. And this booking's — against the authoritative loaded aggregate,
        // not a caller-supplied id, which is why `Booking` carries `id`.
        if intent.booking_id != booking.id {
            return Err(BookingError::EffectMismatch);
        }
        // B3. A kind-specific fact must agree with the intent's kind. Vacuous
        // for the two kind-agnostic facts — the state-vs-intent check in the
        // dispatch covers those.
        if let Some(kind) = fact.implied_kind()
            && kind != intent.operation_kind
        {
            return Err(BookingError::EffectKindMismatch);
        }
        // B4. The intent must not contradict itself before it is compared against
        // anything — a malformed record is not a wildcard. This used to be a
        // hand-rolled shape check here, and the kind-versus-plan comparison was
        // hand-rolled twice more below; all three were compensating for the
        // absence of a read gate on the intent. One definition now, called here
        // as well as by the repository, because this door must not assume its
        // caller went through the repository.
        if let Err(why) = intent.coherent() {
            return Err(BookingError::IncoherentIntent(why));
        }
        // B5. The intent's durable outcome must not contradict the fact.
        // `Prepared`/`Unknown` contradict nothing — that is the ordinary happy
        // path, where the fact arrives before anything recorded the outcome.
        let contradiction = match intent.status {
            EffectStatus::Absent | EffectStatus::Rejected => fact.asserts_existence(),
            EffectStatus::Confirmed => !fact.asserts_existence(),
            EffectStatus::Prepared | EffectStatus::Unknown => false,
        };
        if contradiction {
            return Err(BookingError::ContradictoryProviderFact);
        }
        if let (Some(stored), Some(claimed)) = (
            intent.provider_reference.as_ref(),
            fact.provider_reference(),
        ) && stored != claimed
        {
            // One identity, two provider references: duplication, corruption
            // or broken idempotency. Never silent convergence.
            return Err(BookingError::DuplicateProviderEffect);
        }
        // B6. Every consequential field the fact carries must match the
        // persisted canonical plan. Fee and headcount are not optional in this
        // list: a council booking at a different price or party size is what
        // the fee ceiling and capacity guards exist to prevent, and this
        // binding is the only place it is detectable (ADR-012).
        match (fact, &intent.canonical_plan) {
            (
                VerifiedProviderFact::BookingExists {
                    venue_id,
                    slot_id,
                    attendees,
                    fee,
                    principal,
                    ..
                },
                BookingEffect::Book {
                    principal: plan_principal,
                    attendees: plan_attendees,
                    facts,
                },
            ) => {
                if *venue_id != facts.venue_id {
                    return Err(BookingError::EffectPlanMismatch { field: "venue_id" });
                }
                if *slot_id != facts.slot_id {
                    return Err(BookingError::EffectPlanMismatch { field: "slot_id" });
                }
                if *attendees != *plan_attendees {
                    return Err(BookingError::EffectPlanMismatch { field: "attendees" });
                }
                if *fee != facts.fee {
                    return Err(BookingError::EffectPlanMismatch { field: "fee" });
                }
                if *principal != *plan_principal {
                    return Err(BookingError::EffectPlanMismatch { field: "principal" });
                }
            }
            (
                VerifiedProviderFact::CancellationExists { booking_ref, .. },
                BookingEffect::CancelBooking {
                    booking_ref: plan_ref,
                },
            ) => {
                if booking_ref != plan_ref {
                    return Err(BookingError::EffectPlanMismatch {
                        field: "booking_ref",
                    });
                }
            }
            // Unreachable: B4 proved the column matches the plan and B3 proved
            // the fact's kind matches the column, so a kind-specific fact and a
            // mismatched plan cannot both survive to here. Refused rather than
            // unreachable!() because a boundary that is wrong should say no, not
            // abort the process.
            (VerifiedProviderFact::BookingExists { .. }, BookingEffect::CancelBooking { .. })
            | (VerifiedProviderFact::CancellationExists { .. }, BookingEffect::Book { .. }) => {
                return Err(BookingError::EffectPlanMismatch {
                    field: "operation_kind",
                });
            }
            (
                VerifiedProviderFact::EffectAbsent { .. }
                | VerifiedProviderFact::ProviderRejected { .. },
                _,
            ) => {}
        }

        Ok(intent)
    }

    /// Step D's convergence half: is this state the destination of this fact's
    /// edge, and does everything on record agree?
    ///
    /// `Converged` requires the *state*, the *plan*, and the intent's *durable
    /// outcome* to line up — state alone was revision 1's unsoundness (one
    /// identity, two council bookings, reported healthy), status alone would
    /// trust a repository that wrote a status without its state. Where the fact
    /// carries no reference to compare (`EffectAbsent`, `ProviderRejected`),
    /// the state is compared against the plan instead: that is what the
    /// state's own data leaves available.
    ///
    /// Error vocabulary: a reference disagreement involving what the provider
    /// said → `DuplicateProviderEffect` (one identity, two bookings); a
    /// disagreement among our own records, or a live intent at a state that
    /// claims the outcome already happened → `ContradictoryProviderFact`.
    // One arm per reflection-table row, deliberately over the line budget:
    // this match IS the convergence table in `docs/state-machine.md`, and
    // splitting rows into helpers would scatter the one place the table can be
    // reviewed against the doc.
    #[allow(clippy::too_many_lines)]
    fn converge(
        booking: &Booking,
        fact: &VerifiedProviderFact,
        intent: &EffectIntent,
        origin: FactOrigin,
    ) -> FactResolution<TransitionPlan<Booking, BookingEffect>, BookingError> {
        let fallthrough = match origin {
            FactOrigin::Waiting => BookingError::EffectMismatch,
            FactOrigin::Settled => BookingError::ContradictoryProviderFact,
        };

        // The intent's status must be terminal and in the fact's direction. A
        // live (`Prepared`/`Unknown`) intent at a destination state is not
        // convergence — the repository commits state and status in one
        // transaction, so this shape is inconsistent partial finalisation, and
        // calling it converged would launder a broken atomicity guarantee into
        // an idempotent repair path.
        let terminal_in_direction = if fact.asserts_existence() {
            intent.status == EffectStatus::Confirmed
        } else {
            matches!(intent.status, EffectStatus::Absent | EffectStatus::Rejected)
        };

        match (&booking.state, fact, &intent.canonical_plan) {
            // The booking intent failed; AwaitingBooking is where that lands.
            // No reference exists anywhere, so the state's own venue, slot and
            // verified fee are compared against the plan.
            (
                BookingState::AwaitingBooking(waiting),
                VerifiedProviderFact::EffectAbsent { .. }
                | VerifiedProviderFact::ProviderRejected { .. },
                BookingEffect::Book { facts, .. },
            ) => {
                if !terminal_in_direction {
                    return FactResolution::Denied(BookingError::ContradictoryProviderFact);
                }
                if facts.venue_id == waiting.venue_id
                    && facts.slot_id == waiting.slot_id
                    && facts.fee == waiting.verified_fee
                {
                    FactResolution::Converged
                } else {
                    FactResolution::Denied(BookingError::ContradictoryProviderFact)
                }
            }
            // The booking succeeded; Booked is where that lands. Four copies of
            // the reference must agree: the fact's, the state's, the outer
            // aggregate's — and B5 already matched the intent's against the
            // fact's.
            (
                BookingState::Booked(booked),
                VerifiedProviderFact::BookingExists { booking_ref, .. },
                BookingEffect::Book { .. },
            ) => {
                if !terminal_in_direction {
                    return FactResolution::Denied(BookingError::ContradictoryProviderFact);
                }
                if booked.booking_ref == *booking_ref
                    && booking.booking_ref.as_ref() == Some(booking_ref)
                {
                    FactResolution::Converged
                } else {
                    FactResolution::Denied(BookingError::DuplicateProviderEffect)
                }
            }
            // The cancellation failed; still Booked. The fact carries nothing,
            // so the state's reference is compared against the cancel plan's.
            (
                BookingState::Booked(booked),
                VerifiedProviderFact::EffectAbsent { .. }
                | VerifiedProviderFact::ProviderRejected { .. },
                BookingEffect::CancelBooking {
                    booking_ref: plan_ref,
                },
            ) => {
                if !terminal_in_direction {
                    return FactResolution::Denied(BookingError::ContradictoryProviderFact);
                }
                if booked.booking_ref == *plan_ref && booking.booking_ref.as_ref() == Some(plan_ref)
                {
                    FactResolution::Converged
                } else {
                    FactResolution::Denied(BookingError::ContradictoryProviderFact)
                }
            }
            // The cancellation succeeded; Cancelled. The state is a unit
            // struct, so the aggregate's retained reference stands in for it.
            (
                BookingState::Cancelled(_),
                VerifiedProviderFact::CancellationExists { booking_ref, .. },
                BookingEffect::CancelBooking { .. },
            ) => {
                if !terminal_in_direction {
                    return FactResolution::Denied(BookingError::ContradictoryProviderFact);
                }
                if booking.booking_ref.as_ref() == Some(booking_ref) {
                    FactResolution::Converged
                } else {
                    FactResolution::Denied(BookingError::DuplicateProviderEffect)
                }
            }
            // The booking intent failed while a cancellation was wanted;
            // Cancelled with nothing to show for it. The booking never
            // happened, so no reference can exist anywhere.
            (
                BookingState::Cancelled(_),
                VerifiedProviderFact::EffectAbsent { .. }
                | VerifiedProviderFact::ProviderRejected { .. },
                BookingEffect::Book { .. },
            ) => {
                if !terminal_in_direction {
                    return FactResolution::Denied(BookingError::ContradictoryProviderFact);
                }
                if booking.booking_ref.is_none() {
                    FactResolution::Converged
                } else {
                    FactResolution::Denied(BookingError::ContradictoryProviderFact)
                }
            }
            // The one Waiting state that is also a destination: the old book
            // intent's confirmation re-arriving at CancellingBooking, which is
            // how this state was reached in the first place.
            (
                BookingState::CancellingBooking(cancelling),
                VerifiedProviderFact::BookingExists { booking_ref, .. },
                BookingEffect::Book { .. },
            ) => {
                if !terminal_in_direction {
                    return FactResolution::Denied(BookingError::ContradictoryProviderFact);
                }
                if cancelling.booking_ref == *booking_ref
                    && booking.booking_ref.as_ref() == Some(booking_ref)
                {
                    FactResolution::Converged
                } else {
                    FactResolution::Denied(BookingError::DuplicateProviderEffect)
                }
            }
            // Not this fact's destination at all.
            _ => FactResolution::Denied(fallthrough),
        }
    }

    /// Step D's waiting half: the fact answers the effect this state has in
    /// flight, and its state-relative meaning is derived.
    fn resolve_fact_waiting(
        booking: &Booking,
        fact: &VerifiedProviderFact,
        intent: &EffectIntent,
        context: &FactContext,
    ) -> FactResolution<TransitionPlan<Booking, BookingEffect>, BookingError> {
        // Not the identity this state waits on. The fact may still be the OLD
        // intent's outcome re-arriving at its destination (CancellingBooking is
        // both waiting and a destination), so it gets the convergence reading —
        // with `EffectMismatch` as the honest fallthrough here.
        if booking.active_effect.as_ref() != Some(fact.effect_intent_id()) {
            return Self::converge(booking, fact, intent, FactOrigin::Waiting);
        }

        // The intent's kind must be the kind this state is waiting on — for
        // EVERY fact, kind-agnostic ones included. B3 is vacuous for
        // `EffectAbsent`/`ProviderRejected`, and without this check a
        // cancellation intent could answer a booking state: "the cancellation
        // never happened" read as "the booking never happened".
        if Some(intent.operation_kind) != booking.state.in_flight_kind() {
            return FactResolution::Denied(BookingError::EffectKindMismatch);
        }

        let ready =
            |next: Booking| FactResolution::Ready(TransitionPlan::Local { next_state: next });

        match (&booking.state, fact) {
            // Booking confirmed.
            (
                BookingState::BookingInProgress(_),
                VerifiedProviderFact::BookingExists { booking_ref, .. },
            ) => ready(Booking {
                state: BookingState::Booked(Booked {
                    booking_ref: booking_ref.clone(),
                }),
                booking_ref: Some(booking_ref.clone()),
                active_effect: None,
                ..booking.clone()
            }),
            // The booking never happened, or was refused: back to the state
            // that can try again. Venue, slot and fee come from the persisted
            // plan — the binding source of truth — never reconstructed from
            // anywhere else.
            (
                BookingState::BookingInProgress(_),
                VerifiedProviderFact::EffectAbsent { .. }
                | VerifiedProviderFact::ProviderRejected { .. },
            ) => {
                let BookingEffect::Book { facts, .. } = &intent.canonical_plan else {
                    // Kind said Book; the plan says otherwise. Corrupt row.
                    return FactResolution::Denied(BookingError::EffectPlanMismatch {
                        field: "operation_kind",
                    });
                };
                ready(Booking {
                    state: BookingState::AwaitingBooking(AwaitingBooking {
                        venue_id: facts.venue_id.clone(),
                        slot_id: facts.slot_id.clone(),
                        verified_fee: facts.fee,
                    }),
                    // The tombstone says nothing exists, so nothing may claim
                    // a reference.
                    booking_ref: None,
                    active_effect: None,
                    ..booking.clone()
                })
            }
            // The booking we wanted to cancel turns out to exist: NOW there is
            // something to cancel. The one fact-driven external effect — it
            // targets an in-flight state, so ADR-014 applies and it consumes a
            // FRESH identity from the coordinator.
            (
                BookingState::CancellationRequested(_),
                VerifiedProviderFact::BookingExists { booking_ref, .. },
            ) => {
                let Some(cancel_id) = context.pending_effect.clone() else {
                    return FactResolution::Denied(BookingError::EffectIdentityMissing);
                };
                // A plan whose old and new effects share one identity is
                // structurally invalid; the boundary must not emit it even
                // though the store would refuse it later.
                if cancel_id == *fact.effect_intent_id() {
                    return FactResolution::Denied(BookingError::EffectMismatch);
                }
                FactResolution::Ready(TransitionPlan::ExternalEffect {
                    next_state: Booking {
                        state: BookingState::CancellingBooking(CancellingBooking {
                            booking_ref: booking_ref.clone(),
                            effect_intent_id: cancel_id.clone(),
                        }),
                        booking_ref: Some(booking_ref.clone()),
                        active_effect: Some(cancel_id),
                        ..booking.clone()
                    },
                    effect: BookingEffect::CancelBooking {
                        booking_ref: booking_ref.clone(),
                    },
                })
            }
            // The booking never happened: there is nothing to cancel, which is
            // everything the cancellation wanted.
            (
                BookingState::CancellationRequested(_),
                VerifiedProviderFact::EffectAbsent { .. }
                | VerifiedProviderFact::ProviderRejected { .. },
            ) => ready(Booking {
                state: BookingState::Cancelled(Cancelled),
                booking_ref: None,
                active_effect: None,
                ..booking.clone()
            }),
            // Cancellation confirmed. The reference is kept: it records what
            // was cancelled, and the convergence reading of this very state
            // depends on it.
            (
                BookingState::CancellingBooking(_),
                VerifiedProviderFact::CancellationExists { .. },
            ) => ready(Booking {
                state: BookingState::Cancelled(Cancelled),
                active_effect: None,
                ..booking.clone()
            }),
            // The cancellation never happened, or was refused: still booked.
            (
                BookingState::CancellingBooking(cancelling),
                VerifiedProviderFact::EffectAbsent { .. }
                | VerifiedProviderFact::ProviderRejected { .. },
            ) => ready(Booking {
                state: BookingState::Booked(Booked {
                    booking_ref: cancelling.booking_ref.clone(),
                }),
                booking_ref: Some(cancelling.booking_ref.clone()),
                active_effect: None,
                ..booking.clone()
            }),
            // Kind-specific facts whose kind disagrees with the state were
            // refused above (B3 against the intent, then intent against the
            // state), so these arms are unreachable — but a boundary refuses
            // rather than panics, in case a future edit breaks that chain.
            _ => FactResolution::Denied(BookingError::EffectKindMismatch),
        }
    }

    // `clippy::pedantic` flags the ChangeVenue/UpdateRequirements/Cancel arms as
    // having identical bodies and wants them merged into `(A, X) | (B, X) => ..`.
    // We deliberately keep one arm per (state, proposal) pair, grouped by state.
    //
    // This match IS the state x proposal topology, and the topology is the
    // security surface (implementation guide sections 5 and 19, step 4). Reading it
    // state-by-state answers "which behaviours does VenueSelected have?" directly
    // and mirrors docs/state-machine.md. Merging by body would scatter each
    // state's behaviour set across the match and make an accidentally-added or
    // accidentally-removed pair harder to spot in review.
    #[allow(clippy::match_same_arms)]
    fn resolve_proposal_cell(
        booking: &Booking,
        proposal: BookingProposal,
        authority: &VerifiedAuthority,
        context: &BookingContext,
    ) -> Resolution<TransitionPlan<Booking, BookingEffect>, BookingError> {
        // Every arm produces a complete `Booking` via struct-update syntax, so
        // what each transition *changes* is what you read, and a field nobody
        // mentions is carried rather than quietly defaulted.
        match (&booking.state, proposal) {
            (BookingState::Draft(_), BookingProposal::SelectVenue { venue_id, slot_id }) => {
                let selection = SelectedVenueRef {
                    venue_id: venue_id.clone(),
                    slot_id: slot_id.clone(),
                };
                local(Booking {
                    selected_venue: Some(selection),
                    ..Self::resettled(
                        booking,
                        BookingState::VenueSelected(VenueSelected { venue_id, slot_id }),
                    )
                })
            }
            (BookingState::Draft(_), BookingProposal::Cancel { .. }) => cancel(booking),
            (BookingState::VenueSelected(selected), BookingProposal::VerifySlot) => {
                match Self::bind_facts(
                    booking,
                    context,
                    &selected.venue_id,
                    &selected.slot_id,
                    authority,
                ) {
                    Ok(facts) => local(Booking {
                        state: BookingState::AwaitingBooking(AwaitingBooking {
                            venue_id: facts.venue_id.clone(),
                            slot_id: facts.slot_id.clone(),
                            verified_fee: facts.fee,
                        }),
                        availability: Some(facts.clone()),
                        ..booking.clone()
                    }),
                    Err(error) => Resolution::Denied(error),
                }
            }
            (BookingState::VenueSelected(_), BookingProposal::ChangeVenue) => change_venue(booking),
            (
                BookingState::VenueSelected(selected),
                BookingProposal::UpdateRequirements { attendees },
            ) => update_requirements(
                booking,
                SelectedVenueRef {
                    venue_id: selected.venue_id.clone(),
                    slot_id: selected.slot_id.clone(),
                },
                attendees,
            ),
            (BookingState::VenueSelected(_), BookingProposal::Cancel { .. }) => cancel(booking),
            (BookingState::NeedsRevalidation(pending), BookingProposal::RevalidateVenue) => {
                // The binding target is state data, not context, so this holds
                // without trusting whoever assembled the context. Without it, an
                // ordinary `UpdateRequirements` is enough to launder any venue
                // into the booking.
                let Some(selected) = pending.selected.as_ref() else {
                    return Resolution::Denied(BookingError::VenueFactsMissing);
                };
                match Self::bind_facts(
                    booking,
                    context,
                    &selected.venue_id,
                    &selected.slot_id,
                    authority,
                ) {
                    Ok(facts) => local(Booking {
                        state: BookingState::VenueSelected(VenueSelected {
                            venue_id: facts.venue_id.clone(),
                            slot_id: facts.slot_id.clone(),
                        }),
                        availability: Some(facts.clone()),
                        ..booking.clone()
                    }),
                    Err(error) => Resolution::Denied(error),
                }
            }
            (BookingState::NeedsRevalidation(_), BookingProposal::ChangeVenue) => {
                change_venue(booking)
            }
            (BookingState::NeedsRevalidation(_), BookingProposal::Cancel { .. }) => cancel(booking),
            (BookingState::AwaitingBooking(waiting), BookingProposal::Book) => {
                Self::resolve_book(booking, waiting, authority, context)
            }
            (BookingState::AwaitingBooking(_), BookingProposal::ChangeVenue) => {
                change_venue(booking)
            }
            (
                BookingState::AwaitingBooking(waiting),
                BookingProposal::UpdateRequirements { attendees },
            ) => update_requirements(
                booking,
                SelectedVenueRef {
                    venue_id: waiting.venue_id.clone(),
                    slot_id: waiting.slot_id.clone(),
                },
                attendees,
            ),
            (BookingState::AwaitingBooking(_), BookingProposal::Cancel { .. }) => cancel(booking),
            (BookingState::Booked(booked), BookingProposal::Cancel { .. }) => {
                Self::resolve_cancel_booked(booking, booked, authority, context)
            }
            _ => Resolution::Undefined,
        }
    }
}

/// Shorthand for a transition that reaches nothing external.
fn local(next: Booking) -> Resolution<TransitionPlan<Booking, BookingEffect>, BookingError> {
    Resolution::Ready(TransitionPlan::Local { next_state: next })
}

impl TownHallDomain {
    /// Load the context's facts and bind them to the venue the user actually
    /// chose, then check them against requirements and authority.
    ///
    /// The binding is the point. Loaded facts are never authoritative on their
    /// own — every per-venue guard passes for a venue the user never selected,
    /// so only comparing against the selection catches a substitution.
    ///
    /// Requirements come from `booking`, never from `context`. That is the whole
    /// reason `BookingContext` no longer carries a copy: reading the context's
    /// would validate against whatever the coordinator happened to assemble
    /// rather than against what was committed.
    fn bind_facts<'a>(
        booking: &Booking,
        context: &'a BookingContext,
        venue_id: &VenueId,
        slot_id: &SlotId,
        authority: &VerifiedAuthority,
    ) -> Result<&'a VenueFacts, BookingError> {
        let facts = context
            .selected_facts
            .as_ref()
            .ok_or(BookingError::VenueFactsMissing)?;
        if facts.venue_id != *venue_id || facts.slot_id != *slot_id {
            return Err(BookingError::VenueFactsMissing);
        }
        Self::validate_facts(facts, &booking.requirements, authority)?;
        Ok(facts)
    }

    /// Apply an `UpdateRequirements` patch. `None` means "leave unchanged".
    ///
    /// Before B3a this did not exist and the patch was discarded — see
    /// [`Booking`].
    fn patch_requirements(
        current: &BookingRequirements,
        attendees: Option<u16>,
    ) -> BookingRequirements {
        BookingRequirements {
            attendees: attendees.unwrap_or(current.attendees),
            ..current.clone()
        }
    }

    /// A booking whose selection changed, so any loaded availability is stale.
    ///
    /// Loaded facts describe a venue-slot pair *under a set of requirements*.
    /// Change either and the facts no longer describe anything current. Leaving
    /// them is how an under-capacity or substituted venue survives a
    /// revalidation.
    fn resettled(booking: &Booking, state: BookingState) -> Booking {
        Booking {
            state,
            availability: None,
            ..booking.clone()
        }
    }

    /// `Book` no longer books. It commits the intent to book.
    ///
    /// The transition stops at `BookingInProgress`, which is committed *before*
    /// the council is called (ADR-014). Previously this faked a synchronous
    /// confirmation and jumped straight to `Booked` — fine against an in-process
    /// fake, and the reason a lost response could leave no record that an
    /// external consequence might exist.
    fn resolve_book(
        booking: &Booking,
        waiting: &AwaitingBooking,
        authority: &VerifiedAuthority,
        context: &BookingContext,
    ) -> Resolution<TransitionPlan<Booking, BookingEffect>, BookingError> {
        if !authority.may_book {
            return Resolution::Denied(BookingError::BookingAuthorityRequired);
        }
        let facts = match Self::bind_facts(
            booking,
            context,
            &waiting.venue_id,
            &waiting.slot_id,
            authority,
        ) {
            Ok(facts) => facts,
            Err(error) => return Resolution::Denied(error),
        };
        // The fee verified at `VerifySlot` is carried on the state precisely so a
        // fee that moved since then is detectable here.
        if facts.fee != waiting.verified_fee {
            return Resolution::Denied(BookingError::VenueFactsMissing);
        }
        let Some(effect_intent_id) = context.pending_effect.clone() else {
            return Resolution::Denied(BookingError::EffectIdentityMissing);
        };

        Resolution::Ready(TransitionPlan::ExternalEffect {
            next_state: Booking {
                state: BookingState::BookingInProgress(BookingInProgress {
                    effect_intent_id: effect_intent_id.clone(),
                }),
                active_effect: Some(effect_intent_id),
                ..booking.clone()
            },
            effect: BookingEffect::Book {
                principal: authority.principal.clone(),
                attendees: booking.requirements.attendees,
                facts: facts.clone(),
            },
        })
    }

    /// Cancelling a confirmed booking is an external effect, not a local one.
    ///
    /// This is the *ordinary* cancellation path, and it has to be external from
    /// here rather than from slice F: if it stayed local, an ordinary cancel
    /// would commit `Cancelled` while the council booking stayed live for every
    /// slice between the coordinator landing and in-flight cancellation.
    fn resolve_cancel_booked(
        booking: &Booking,
        booked: &Booked,
        authority: &VerifiedAuthority,
        context: &BookingContext,
    ) -> Resolution<TransitionPlan<Booking, BookingEffect>, BookingError> {
        if !authority.may_cancel {
            return Resolution::Denied(BookingError::CancellationAuthorityRequired);
        }
        let Some(effect_intent_id) = context.pending_effect.clone() else {
            return Resolution::Denied(BookingError::EffectIdentityMissing);
        };

        Resolution::Ready(TransitionPlan::ExternalEffect {
            next_state: Booking {
                state: BookingState::CancellingBooking(CancellingBooking {
                    booking_ref: booked.booking_ref.clone(),
                    effect_intent_id: effect_intent_id.clone(),
                }),
                active_effect: Some(effect_intent_id),
                ..booking.clone()
            },
            effect: BookingEffect::CancelBooking {
                booking_ref: booked.booking_ref.clone(),
            },
        })
    }
}

#[async_trait]
impl BoundaryDomain for TownHallDomain {
    type State = Booking;
    type Proposal = BookingProposal;
    type Effect = BookingEffect;
    type Authority = VerifiedAuthority;
    type Context = BookingContext;
    type ProviderFact = VerifiedProviderFact;
    type SystemEvent = SystemEvent;
    type FactContext = FactContext;
    type Error = BookingError;

    async fn resolve_proposal(
        &self,
        booking: &Self::State,
        proposal: Self::Proposal,
        authority: &Self::Authority,
        context: &Self::Context,
    ) -> Resolution<TransitionPlan<Self::State, Self::Effect>, Self::Error> {
        let resolution = Self::resolve_proposal_cell(booking, proposal, authority, context);

        // Whether a behaviour EXISTS depends on (state, proposal) alone, so an
        // Undefined cell stays Undefined no matter what the aggregate carries.
        if matches!(resolution, Resolution::Undefined) {
            return Resolution::Undefined;
        }

        // For a cell that does exist: the same self-consistency step as the
        // other two doors, and for the same reason. Review found this gap after
        // it was closed on the fact door — an incoherent VenueSelected carrying
        // a phantom reference could take Cancel into Cancelled, whose reference
        // is legitimately unconstrained, and the phantom became terminal
        // history indistinguishable from a real cancellation. The cell's own
        // result is computed first and discarded: classification is pure, so
        // evaluating it costs nothing and launders nothing.
        if let Err(why) = booking.coherent() {
            return Resolution::Denied(match why {
                IncoherentBooking::EffectIdentity { .. } => {
                    BookingError::InconsistentEffectIdentity
                }
                other => BookingError::IncoherentAggregate(other),
            });
        }

        resolution
    }

    /// The fact door. Four outcomes; see `docs/state-machine.md` for the full
    /// 40-cell matrix this implements.
    ///
    /// The decision procedure, in order:
    ///
    /// ```text
    /// S. Category of the state. Absent -> Undefined, before anything else is
    ///    consulted — no guard runs, no context is read.
    /// I. Waiting and Settled require the persisted intent.
    /// B. Intent binding — everything checkable without asking what the state
    ///    means: identity, resource, kind, status/reference shape, durable
    ///    contradiction, canonical-plan fields.
    /// C. Aggregate self-consistency: the state's own effect id and
    ///    active_effect must agree.
    /// D. Dispatch: Waiting derives the state-relative meaning; Settled asks
    ///    whether the state already reflects the fact.
    /// ```
    async fn resolve_fact(
        &self,
        booking: &Self::State,
        fact: Verified<Self::ProviderFact>,
        context: &Self::FactContext,
    ) -> FactResolution<TransitionPlan<Self::State, Self::Effect>, Self::Error> {
        // S. Nothing else is consulted for an Absent state: irrelevant context
        // must not manufacture behaviour where none exists.
        let category = fact_category(&booking.state);
        if matches!(category, FactCategory::Absent) {
            return FactResolution::Undefined;
        }

        // The provenance wrapper has done its job by existing: only a verifier
        // (or trusted-half test code, greppably) could have produced it.
        let fact = fact.into_inner();

        // I + B.
        let intent = match Self::bind_fact(booking, &fact, context) {
            Ok(intent) => intent,
            Err(error) => return FactResolution::Denied(error),
        };

        // C. The aggregate must not contradict itself — the WHOLE invariant,
        // not just the effect pointer. The store refuses to persist or load a
        // disagreement, but this door must not assume its caller went through
        // the store; review of this slice found that checking only the effect
        // ids let a phantom booking_ref at BookingInProgress be silently
        // cleared by the very transitions below.
        if let Err(why) = booking.coherent() {
            return FactResolution::Denied(match why {
                IncoherentBooking::EffectIdentity { .. } => {
                    BookingError::InconsistentEffectIdentity
                }
                other => BookingError::IncoherentAggregate(other),
            });
        }

        // D.
        match category {
            FactCategory::Waiting => Self::resolve_fact_waiting(booking, &fact, intent, context),
            FactCategory::Settled => Self::converge(booking, &fact, intent, FactOrigin::Settled),
            FactCategory::Absent => FactResolution::Undefined,
        }
    }

    /// The system-event door. One variant, three edges, no context.
    ///
    /// `NeedsHuman` is reachable only through here — neither a proposer nor a
    /// provider fact can conclude that *our own* retry budget is exhausted.
    async fn resolve_system_event(
        &self,
        booking: &Self::State,
        event: Self::SystemEvent,
    ) -> Resolution<TransitionPlan<Self::State, Self::Effect>, Self::Error> {
        let SystemEvent::ReconciliationExhausted { effect_intent_id } = event;

        // Only a state with an effect in flight has a retry budget to exhaust.
        let Some(waiting_on) = booking.state.effect_intent_id() else {
            return Resolution::Undefined;
        };

        // The same self-consistency check as the fact door. Note the freeze
        // itself would also be refused by the store — `admissible` runs
        // `coherent()` on every write — so refusing here with a precise reason
        // beats an opaque refusal later; either way an aggregate this broken
        // needs an operator below the domain, not automation above it.
        if let Err(why) = booking.coherent() {
            return Resolution::Denied(match why {
                IncoherentBooking::EffectIdentity { .. } => {
                    BookingError::InconsistentEffectIdentity
                }
                other => BookingError::IncoherentAggregate(other),
            });
        }

        // Exhaustion of some OTHER effect says nothing about this state.
        if *waiting_on != effect_intent_id {
            return Resolution::Denied(BookingError::EffectMismatch);
        }

        // Give up: no automation will act on this effect again, so the pointer
        // is cleared — a NeedsHuman still carrying an active_effect would
        // invite a reconciler to keep chasing what it just abandoned.
        Resolution::Ready(TransitionPlan::Local {
            next_state: Booking {
                state: BookingState::NeedsHuman(NeedsHuman),
                active_effect: None,
                ..booking.clone()
            },
        })
    }
}

/// Abandon the booking. Selection and availability are kept deliberately: they
/// record what was being attempted, and `Cancelled` has no outbound behaviour
/// that could consume them, so retaining them costs nothing and keeps the audit
/// trail intact.
fn cancel(booking: &Booking) -> Resolution<TransitionPlan<Booking, BookingEffect>, BookingError> {
    local(Booking {
        state: BookingState::Cancelled(Cancelled),
        ..booking.clone()
    })
}

/// Start over. The selection is abandoned, so loaded facts describe nothing.
fn change_venue(
    booking: &Booking,
) -> Resolution<TransitionPlan<Booking, BookingEffect>, BookingError> {
    local(Booking {
        selected_venue: None,
        ..TownHallDomain::resettled(booking, BookingState::Draft(Draft))
    })
}

/// Change what is being asked for, carrying the selection so it can be
/// revalidated against the new requirements.
///
/// The patch is applied here. Before B3a it was destructured away and lost.
fn update_requirements(
    booking: &Booking,
    selected: SelectedVenueRef,
    attendees: Option<u16>,
) -> Resolution<TransitionPlan<Booking, BookingEffect>, BookingError> {
    local(Booking {
        requirements: TownHallDomain::patch_requirements(&booking.requirements, attendees),
        ..TownHallDomain::resettled(
            booking,
            BookingState::NeedsRevalidation(NeedsRevalidation {
                selected: Some(selected),
            }),
        )
    })
}

/// The state × proposal topology, pinned.
///
/// Spec §7 draws arrows in two vocabularies, and only one of them is a
/// `BookingProposal`. These are: `SelectVenue`, `VerifySlot`, `ChangeVenue`,
/// `UpdateRequirements`, `RevalidateVenue`, `Book`, `Cancel`.
/// These are **not** — they are evidence or read outcomes, and no agent can
/// submit them: `booking_confirmed`, `booking_failed`, `no_booking_found`,
/// `booking_found`, `reconciliation_failed`, `cancellation_confirmed`,
/// `cancellation_failed`, `view_booking`.
///
/// Counting proposal arrows only, the spec defines 15 cells and this code
/// implements 14. The single difference is recorded in [`PENDING`].
///
/// The spec's §7.1 vocabulary also lists `Reconcile`, but draws it on **no
/// arrow anywhere**. B2 removed the variant entirely rather than keep a
/// proposal that is `Undefined` everywhere: recovery is runtime machinery and
/// must run when the model is offline, hostile or absent (ADR-012). So the
/// matrix is 10 states x 7 proposals = 70 cells, not 80.
#[cfg(test)]
mod topology {
    use super::*;
    use bld_kernel::Resolution;
    use bld_types::{BookingRequirements, Money, SlotId, TimeWindow, VenueId};

    const STATE_COUNT: usize = 10;
    const PROPOSAL_COUNT: usize = 7;

    /// Exhaustive by construction: adding a `BookingState` variant stops this
    /// compiling, and the out-of-range index then trips
    /// `every_state_variant_has_a_representative`. Together they make it
    /// impossible to add a state that the sweep silently ignores.
    fn state_index(state: &BookingState) -> usize {
        match state {
            BookingState::Draft(_) => 0,
            BookingState::VenueSelected(_) => 1,
            BookingState::NeedsRevalidation(_) => 2,
            BookingState::AwaitingBooking(_) => 3,
            BookingState::BookingInProgress(_) => 4,
            BookingState::CancellationRequested(_) => 5,
            BookingState::Booked(_) => 6,
            BookingState::CancellingBooking(_) => 7,
            BookingState::Cancelled(_) => 8,
            BookingState::NeedsHuman(_) => 9,
        }
    }

    fn proposal_index(proposal: &BookingProposal) -> usize {
        match proposal {
            BookingProposal::SelectVenue { .. } => 0,
            BookingProposal::VerifySlot => 1,
            BookingProposal::ChangeVenue => 2,
            BookingProposal::UpdateRequirements { .. } => 3,
            BookingProposal::RevalidateVenue => 4,
            BookingProposal::Book => 5,
            BookingProposal::Cancel { .. } => 6,
        }
    }

    fn selection() -> SelectedVenueRef {
        SelectedVenueRef {
            venue_id: VenueId::new("TH-A"),
            slot_id: SlotId::new("SLOT-A"),
        }
    }

    fn all_states() -> Vec<BookingState> {
        vec![
            BookingState::Draft(Draft),
            BookingState::VenueSelected(VenueSelected {
                venue_id: VenueId::new("TH-A"),
                slot_id: SlotId::new("SLOT-A"),
            }),
            BookingState::NeedsRevalidation(NeedsRevalidation {
                selected: Some(selection()),
            }),
            BookingState::AwaitingBooking(AwaitingBooking {
                venue_id: VenueId::new("TH-A"),
                slot_id: SlotId::new("SLOT-A"),
                verified_fee: Money::from_pence(4_500),
            }),
            BookingState::BookingInProgress(BookingInProgress {
                effect_intent_id: EffectIntentId::new("BOOK-BKG-1001-1"),
            }),
            BookingState::CancellationRequested(CancellationRequested {
                effect_intent_id: EffectIntentId::new("EFF-BKG-1001-BOOK-0"),
            }),
            BookingState::Booked(Booked {
                booking_ref: CouncilBookingRef::new("TH-92718"),
            }),
            BookingState::CancellingBooking(CancellingBooking {
                booking_ref: CouncilBookingRef::new("TH-92718"),
                effect_intent_id: EffectIntentId::new("EFF-BKG-1001-CANCEL-0"),
            }),
            BookingState::Cancelled(Cancelled),
            BookingState::NeedsHuman(NeedsHuman),
        ]
    }

    fn all_proposals() -> Vec<BookingProposal> {
        vec![
            BookingProposal::SelectVenue {
                venue_id: VenueId::new("TH-A"),
                slot_id: SlotId::new("SLOT-A"),
            },
            BookingProposal::VerifySlot,
            BookingProposal::ChangeVenue,
            BookingProposal::UpdateRequirements {
                attendees: Some(25),
            },
            BookingProposal::RevalidateVenue,
            BookingProposal::Book,
            BookingProposal::Cancel {
                reason: "user_cancelled".to_owned(),
            },
        ]
    }

    #[test]
    fn every_state_variant_has_a_representative() {
        let mut seen = [false; STATE_COUNT];
        for state in all_states() {
            seen[state_index(&state)] = true;
        }
        assert!(
            seen.iter().all(|hit| *hit),
            "all_states() is missing a BookingState variant; the topology sweep would silently skip it"
        );
    }

    #[test]
    fn every_proposal_variant_has_a_representative() {
        let mut seen = [false; PROPOSAL_COUNT];
        for proposal in all_proposals() {
            seen[proposal_index(&proposal)] = true;
        }
        assert!(
            seen.iter().all(|hit| *hit),
            "all_proposals() is missing a BookingProposal variant"
        );
    }

    /// Spec-grounded. **A diff to this table means the legal transition graph
    /// changed and needs an ADR** — it is a review stop-sign, not routine
    /// maintenance.
    const LOCKED: &[(&str, &[&str])] = &[
        ("Draft", &["SelectVenue", "Cancel"]),
        (
            "VenueSelected",
            &["VerifySlot", "ChangeVenue", "UpdateRequirements", "Cancel"],
        ),
        (
            "NeedsRevalidation",
            &["RevalidateVenue", "ChangeVenue", "Cancel"],
        ),
        (
            "AwaitingBooking",
            &["Book", "ChangeVenue", "UpdateRequirements", "Cancel"],
        ),
        ("BookingInProgress", &[]),
        ("CancellationRequested", &[]),
        ("Booked", &["Cancel"]),
        ("CancellingBooking", &[]),
        ("Cancelled", &[]),
        ("NeedsHuman", &[]),
    ];

    /// Cells the spec draws that this code deliberately does not implement yet.
    ///
    /// Editing this table during M4 is *expected*; editing [`LOCKED`] is not.
    const PENDING: &[(&str, &str, &str)] = &[(
        "BookingInProgress",
        "Cancel",
        "Spec §7 L364 draws this and `Cancel` is a real proposal, so it is a genuine gap. \
         Deferred because `CancellationRequested` has zero outbound behaviours and no \
         reconciliation ingress: committing an accepted cancellation the system cannot \
         fulfil is worse than refusing honestly. Lands with the M4 slice that can consume it.",
    )];

    fn permissive_authority() -> VerifiedAuthority {
        VerifiedAuthority {
            principal: PrincipalId::new("lucy"),
            actor: ActorId::new("townhall-agent"),
            max_fee: Money::from_pence(5_000),
            may_book: true,
            may_cancel: true,
        }
    }

    fn permissive_requirements() -> BookingRequirements {
        BookingRequirements {
            purpose: "meeting".to_owned(),
            requested_date: "2026-08-20".to_owned(),
            time_window: TimeWindow {
                from: "13:00".to_owned(),
                to: "17:00".to_owned(),
            },
            attendees: 20,
            wheelchair_accessible: true,
            max_fee: Money::from_pence(5_000),
        }
    }

    /// Wrap a state in a complete booking so the sweep can classify it.
    ///
    /// The other fields are deliberately uniform: this suite asks only whether a
    /// behaviour *exists*, and existence must not depend on any of them.
    fn booking_of(state: BookingState, requirements: BookingRequirements) -> Booking {
        // The outer copies are derived from the state, so the wrapper is
        // coherent for every variant — the sweep asks whether behaviours
        // exist, and an incoherent wrapper would be refused before the
        // topology was even consulted.
        Booking {
            id: BookingId::new("BKG-1001"),
            booking_ref: state.council_booking_ref().cloned(),
            active_effect: state.effect_intent_id().cloned(),
            state,
            requirements,
            selected_venue: Some(selection()),
            availability: None,
        }
    }

    fn permissive_context() -> BookingContext {
        BookingContext {
            selected_facts: Some(VenueFacts {
                venue_id: VenueId::new("TH-A"),
                slot_id: SlotId::new("SLOT-A"),
                capacity: 30,
                wheelchair_accessible: true,
                fee: Money::from_pence(4_500),
                available: true,
            }),
            pending_effect: Some(EffectIntentId::new("EFF-BKG-1001-BOOK-0")),
        }
    }

    /// Every guard fails. Used to prove that whether a behaviour *exists* does
    /// not depend on authority or context — only on `(state, proposal)`.
    fn hostile_authority() -> VerifiedAuthority {
        VerifiedAuthority {
            may_book: false,
            may_cancel: false,
            max_fee: Money::from_pence(0),
            ..permissive_authority()
        }
    }

    fn hostile_context() -> BookingContext {
        BookingContext {
            selected_facts: None,
            ..permissive_context()
        }
    }

    fn expected_defined(state: &str, proposal: &str) -> bool {
        LOCKED.iter().find(|(name, _)| *name == state).map_or_else(
            || panic!("state {state} missing from LOCKED"),
            |(_, allowed)| allowed.contains(&proposal),
        )
    }

    async fn sweep(
        label: &str,
        authority: &VerifiedAuthority,
        context: &BookingContext,
        requirements: &BookingRequirements,
    ) {
        let domain = TownHallDomain;
        let mut checked = 0_usize;

        for state in all_states() {
            for proposal in all_proposals() {
                let state_name = state.name();
                let proposal_name = proposal.name();
                let want_defined = expected_defined(state_name, proposal_name);
                let booking = booking_of(state.clone(), requirements.clone());

                let got = domain
                    .resolve_proposal(&booking, proposal.clone(), authority, context)
                    .await;
                let is_undefined = matches!(got, Resolution::Undefined);

                assert_eq!(
                    !is_undefined,
                    want_defined,
                    "[{label}] {state_name} + {proposal_name}: expected {}, resolve returned {}",
                    if want_defined {
                        "a behaviour"
                    } else {
                        "Undefined"
                    },
                    if is_undefined {
                        "Undefined"
                    } else {
                        "a behaviour"
                    }
                );
                checked += 1;
            }
        }

        assert_eq!(checked, all_states().len() * all_proposals().len());
    }

    /// The whole matrix, under a fixture where every guard passes.
    #[tokio::test]
    async fn topology_matches_the_pinned_matrix() {
        sweep(
            "permissive",
            &permissive_authority(),
            &permissive_context(),
            &permissive_requirements(),
        )
        .await;
    }

    /// The same matrix under a fixture where every guard fails. A behaviour
    /// that exists must still exist — it just gets `Denied` instead of `Ready`.
    ///
    /// This is what catches the guide's Mistake 13: collapsing `Undefined` into
    /// `Denied` would light up all 66 impossible cells at once.
    #[tokio::test]
    async fn topology_does_not_depend_on_authority_or_context() {
        sweep(
            "hostile",
            &hostile_authority(),
            &hostile_context(),
            &permissive_requirements(),
        )
        .await;
    }

    /// And it must not depend on the booking's own requirements either.
    ///
    /// B3a moved `requirements` out of the context and into the state, so the
    /// sweep above no longer varies them — a guard reading them is now reading
    /// *state*. Requirements decide whether an existing behaviour is permitted,
    /// never whether it exists, so an unsatisfiable set must change nothing here.
    #[tokio::test]
    async fn topology_does_not_depend_on_requirements() {
        let unsatisfiable = BookingRequirements {
            attendees: 9_999,
            max_fee: Money::from_pence(0),
            ..permissive_requirements()
        };
        sweep(
            "unsatisfiable-requirements",
            &permissive_authority(),
            &permissive_context(),
            &unsatisfiable,
        )
        .await;
    }

    /// Under the permissive fixture a legal cell must actually reach `Ready`.
    ///
    /// Without this, a future guard change could make every cell `Denied` and
    /// both sweeps above would still pass while proving nothing.
    #[tokio::test]
    async fn permissive_fixture_reaches_ready_on_legal_cells() {
        let domain = TownHallDomain;
        let authority = permissive_authority();
        let context = permissive_context();

        for state in all_states() {
            for proposal in all_proposals() {
                if !expected_defined(state.name(), proposal.name()) {
                    continue;
                }
                let booking = booking_of(state.clone(), permissive_requirements());
                let got = domain
                    .resolve_proposal(&booking, proposal.clone(), &authority, &context)
                    .await;
                assert!(
                    matches!(got, Resolution::Ready(_)),
                    "permissive fixture no longer reaches Ready on {} + {}; the opposed-fixture \
                     sweep would degrade to comparing two Denied results",
                    state.name(),
                    proposal.name()
                );
            }
        }
    }

    #[test]
    fn pending_cells_are_absent_from_locked() {
        for (state, proposal, why) in PENDING {
            assert!(
                !expected_defined(state, proposal),
                "{state} + {proposal} is in both LOCKED and PENDING ({why})"
            );
        }
    }
}

/// The domain's behaviour, pinned cell by cell.
///
/// Written in B1 against the pre-B2 contract, and migrated here. What it pins
/// is unchanged — the exact next state for every legal cell and the exact error
/// for every denial. Only the wrapper moved, from
/// `BoundaryOutcome::Committed(state)` to
/// `Resolution::Ready(TransitionPlan::Local { next_state })`, because the kernel
/// no longer commits: it classifies, and the coordinator commits (ADR-013).
///
/// That migration is the point. A characterization suite whose value evaporates
/// the moment the signature changes is not a safety net, and this one survived
/// with its teeth: after B2, mutating `Book` back into a `Local` transition, or
/// dropping the venue-selection carry-forward, still fails here.
///
/// # One defect per fixture
///
/// Denial fixtures start from `good_facts()` and break exactly **one** thing. A
/// fixture with two defects pins whichever guard the code happens to check
/// first, so a safety-neutral reorder would turn it red for no reason — verified
/// by swapping the `may_book` and `selected_facts` checks and watching
/// everything stay green.
///
/// # Two cells changed deliberately
///
/// `Book` now stops at `BookingInProgress` and `Booked + Cancel` at
/// `CancellingBooking`, both as `ExternalEffect`. B1 pinned those as `#[ignore]`d
/// expectations; B2 unignored them and deleted the tripwires that had asserted
/// the old behaviour.
#[cfg(test)]
mod characterization {
    use super::*;
    use bld_kernel::{Resolution, TransitionPlan};
    use bld_types::{BookingRequirements, Money, TimeWindow};

    fn authority() -> VerifiedAuthority {
        VerifiedAuthority {
            principal: PrincipalId::new("lucy"),
            actor: ActorId::new("townhall-agent"),
            max_fee: Money::from_pence(5_000),
            may_book: true,
            may_cancel: true,
        }
    }

    fn requirements() -> BookingRequirements {
        BookingRequirements {
            purpose: "meeting".to_owned(),
            requested_date: "2026-08-20".to_owned(),
            time_window: TimeWindow {
                from: "13:00".to_owned(),
                to: "17:00".to_owned(),
            },
            attendees: 20,
            wheelchair_accessible: true,
            max_fee: Money::from_pence(5_000),
        }
    }

    /// Facts that satisfy every guard. Denial fixtures start from this and
    /// break exactly one thing.
    fn good_facts() -> VenueFacts {
        VenueFacts {
            venue_id: VenueId::new("TH-A"),
            slot_id: SlotId::new("SLOT-A"),
            capacity: 30,
            wheelchair_accessible: true,
            fee: Money::from_pence(4_500),
            available: true,
        }
    }

    fn context() -> BookingContext {
        BookingContext {
            selected_facts: Some(good_facts()),
            pending_effect: Some(EffectIntentId::new("EFF-BKG-1001-BOOK-0")),
        }
    }

    fn selection() -> SelectedVenueRef {
        SelectedVenueRef {
            venue_id: VenueId::new("TH-A"),
            slot_id: SlotId::new("SLOT-A"),
        }
    }

    // --------------------------------------------------------------- fixtures
    //
    // Each returns a complete, *realistic* `Booking`: a state paired with the
    // other business fields as they would actually be at that point in the
    // lifecycle. Realism matters here in a way it does not in the topology
    // sweep, because these tests assert the complete next value — so a fixture
    // that carried, say, `availability: None` at `AwaitingBooking` would let a
    // transition that wrongly cleared it pass unnoticed.

    fn draft() -> Booking {
        Booking {
            id: BookingId::new("BKG-1001"),
            state: BookingState::Draft(Draft),
            requirements: requirements(),
            selected_venue: None,
            availability: None,
            booking_ref: None,
            active_effect: None,
        }
    }

    fn venue_selected() -> Booking {
        Booking {
            state: BookingState::VenueSelected(VenueSelected {
                venue_id: VenueId::new("TH-A"),
                slot_id: SlotId::new("SLOT-A"),
            }),
            selected_venue: Some(selection()),
            ..draft()
        }
    }

    fn needs_revalidation() -> Booking {
        Booking {
            state: BookingState::NeedsRevalidation(NeedsRevalidation {
                selected: Some(selection()),
            }),
            ..venue_selected()
        }
    }

    fn awaiting_booking() -> Booking {
        Booking {
            state: BookingState::AwaitingBooking(AwaitingBooking {
                venue_id: VenueId::new("TH-A"),
                slot_id: SlotId::new("SLOT-A"),
                verified_fee: Money::from_pence(4_500),
            }),
            availability: Some(good_facts()),
            ..venue_selected()
        }
    }

    fn booked() -> Booking {
        Booking {
            state: BookingState::Booked(Booked {
                booking_ref: CouncilBookingRef::new("TH-92718"),
            }),
            booking_ref: Some(CouncilBookingRef::new("TH-92718")),
            ..awaiting_booking()
        }
    }

    /// Classify one proposal and return the resolution.
    ///
    /// B2 changed what a turn *is*: the kernel classifies and the coordinator
    /// commits, so there is no longer a single call that both decides and
    /// mutates. B3a changed what a turn *produces*: the complete next booking
    /// rather than the state discriminator, which is what lets these tests pin
    /// every business field instead of one enum tag.
    async fn turn(
        booking: Booking,
        proposal: BookingProposal,
        authority: &VerifiedAuthority,
        context: &BookingContext,
    ) -> Resolution<TransitionPlan<Booking, BookingEffect>, BookingError> {
        TownHallDomain
            .resolve_proposal(&booking, proposal, authority, context)
            .await
    }

    /// A local transition to `next`, which is what most cells produce.
    fn committed_local(
        next: Booking,
    ) -> Resolution<TransitionPlan<Booking, BookingEffect>, BookingError> {
        Resolution::Ready(TransitionPlan::Local { next_state: next })
    }

    /// The bug B3a fixes, end to end.
    ///
    /// Lucy raises her party from 20 to 25, then the venue is revalidated
    /// against a room that holds 22. That must be refused.
    ///
    /// Before B3a it was accepted. `UpdateRequirements` could not carry its own
    /// patch — a plan holding only a state discriminator had nowhere to put
    /// changed requirements — so `RevalidateVenue` validated 22 against the
    /// stale 20 and passed. The two steps are threaded here exactly as a
    /// coordinator would thread them: the second turn starts from what the
    /// first one produced, which is the only way the patch can be observed at
    /// all.
    #[tokio::test]
    async fn a_raised_headcount_is_revalidated_against_the_new_number() {
        let Resolution::Ready(first) = turn(
            awaiting_booking(),
            BookingProposal::UpdateRequirements {
                attendees: Some(25),
            },
            &authority(),
            &context(),
        )
        .await
        else {
            panic!("UpdateRequirements must be legal at AwaitingBooking");
        };
        let after_patch = first.next_state().clone();
        assert_eq!(
            after_patch.requirements.attendees, 25,
            "the patch must reach the committed booking, or nothing downstream can honour it"
        );

        let too_small = VenueFacts {
            capacity: 22,
            ..good_facts()
        };
        let got = turn(
            after_patch,
            BookingProposal::RevalidateVenue,
            &authority(),
            &BookingContext {
                selected_facts: Some(too_small),
                ..context()
            },
        )
        .await;

        assert_eq!(
            got,
            Resolution::Denied(BookingError::CapacityInsufficient {
                capacity: 22,
                required: 25,
            }),
            "a room holding 22 must be refused for 25 people"
        );
    }

    /// `None` means "leave it alone", not "reset it".
    #[tokio::test]
    async fn an_empty_requirements_patch_changes_nothing() {
        let got = turn(
            venue_selected(),
            BookingProposal::UpdateRequirements { attendees: None },
            &authority(),
            &context(),
        )
        .await;
        assert_eq!(got, committed_local(needs_revalidation()));
    }

    /// `active_effect` is a pointer to work in flight. Only the two external
    /// transitions may set it, and every other legal cell must pass it through
    /// untouched.
    ///
    /// Swept rather than sampled: this is the field a future arm is most likely
    /// to clobber by rebuilding a `Booking` from scratch instead of carrying it.
    #[tokio::test]
    async fn only_external_transitions_touch_the_effect_pointer() {
        let external = [("AwaitingBooking", "Book"), ("Booked", "Cancel")];

        for source in [
            draft(),
            venue_selected(),
            needs_revalidation(),
            awaiting_booking(),
            booked(),
        ] {
            for proposal in [
                BookingProposal::SelectVenue {
                    venue_id: VenueId::new("TH-A"),
                    slot_id: SlotId::new("SLOT-A"),
                },
                BookingProposal::VerifySlot,
                BookingProposal::ChangeVenue,
                BookingProposal::UpdateRequirements {
                    attendees: Some(25),
                },
                BookingProposal::RevalidateVenue,
                BookingProposal::Book,
                BookingProposal::Cancel {
                    reason: "changed mind".to_owned(),
                },
            ] {
                let cell = (source.state.name(), proposal.name());
                let Resolution::Ready(plan) =
                    turn(source.clone(), proposal, &authority(), &context()).await
                else {
                    continue;
                };
                let next = plan.next_state();

                if external.contains(&cell) {
                    assert_eq!(
                        next.active_effect.as_ref(),
                        context().pending_effect.as_ref(),
                        "{} + {} is external and must adopt the pending identity",
                        cell.0,
                        cell.1
                    );
                    assert_eq!(
                        next.active_effect.as_ref(),
                        next.state.effect_intent_id(),
                        "{} + {}: the two copies of the effect identity disagree",
                        cell.0,
                        cell.1
                    );
                } else {
                    assert_eq!(
                        next.active_effect, source.active_effect,
                        "{} + {} is local and must carry active_effect through unchanged",
                        cell.0, cell.1
                    );
                }
            }
        }
    }

    /// Every plan the domain produces must be self-consistent, or the store
    /// will refuse to persist it. Swept, because this is the property each new
    /// transition arm has to uphold and none of them is reminded to.
    #[tokio::test]
    async fn every_transition_produces_a_coherent_booking() {
        for source in [
            draft(),
            venue_selected(),
            needs_revalidation(),
            awaiting_booking(),
            booked(),
        ] {
            source
                .coherent()
                .expect("the fixtures themselves must be coherent");

            for proposal in [
                BookingProposal::SelectVenue {
                    venue_id: VenueId::new("TH-B"),
                    slot_id: SlotId::new("SLOT-B"),
                },
                BookingProposal::VerifySlot,
                BookingProposal::ChangeVenue,
                BookingProposal::UpdateRequirements {
                    attendees: Some(25),
                },
                BookingProposal::RevalidateVenue,
                BookingProposal::Book,
                BookingProposal::Cancel {
                    reason: "changed mind".to_owned(),
                },
            ] {
                let cell = format!("{} + {}", source.state.name(), proposal.name());
                if let Resolution::Ready(plan) =
                    turn(source.clone(), proposal, &authority(), &context()).await
                {
                    plan.next_state().coherent().unwrap_or_else(|why| {
                        panic!("{cell} produced an incoherent booking: {why}")
                    });
                }
            }
        }
    }

    /// The effect pointer is checked in both directions. A state waiting on
    /// nothing must not carry an `active_effect` either — recovery would chase
    /// an effect no state expects, and nothing would ever resolve it.
    #[test]
    fn an_effect_pointer_with_no_state_waiting_on_it_is_incoherent() {
        let orphaned = Booking {
            active_effect: Some(EffectIntentId::new("EFF-ORPHAN")),
            ..awaiting_booking()
        };
        assert!(matches!(
            orphaned.coherent(),
            Err(IncoherentBooking::EffectIdentity { .. })
        ));
    }

    #[test]
    fn a_state_naming_a_venue_the_aggregate_does_not_is_incoherent() {
        let mismatched = Booking {
            selected_venue: Some(SelectedVenueRef {
                venue_id: VenueId::new("TH-Z"),
                slot_id: SlotId::new("SLOT-A"),
            }),
            ..venue_selected()
        };
        assert!(matches!(
            mismatched.coherent(),
            Err(IncoherentBooking::Selection { .. })
        ));
    }

    #[test]
    fn a_state_naming_a_council_reference_the_aggregate_does_not_is_incoherent() {
        let mismatched = Booking {
            booking_ref: Some(CouncilBookingRef::new("TH-00000")),
            ..booked()
        };
        assert!(matches!(
            mismatched.coherent(),
            Err(IncoherentBooking::CouncilReference { .. })
        ));
    }

    /// The state B3b's completeness matrix requires must be representable: a
    /// cancellation asked for while the booking is still in flight, still
    /// pointing at the booking effect it is waiting on.
    ///
    /// The both-directions effect rule would have forbidden this while
    /// `CancellationRequested` was a unit struct — an aggregate could not name
    /// the effect it was waiting for, so `active_effect` had to be `None` and
    /// recovery would have had nothing to reconcile. Caught in review of this
    /// slice rather than after the invariant shipped.
    #[test]
    fn a_cancellation_requested_while_the_booking_is_in_flight_is_coherent() {
        let effect = EffectIntentId::new("EFF-BKG-1001-BOOK-0");
        let awaiting_the_council = Booking {
            state: BookingState::CancellationRequested(CancellationRequested {
                effect_intent_id: effect.clone(),
            }),
            active_effect: Some(effect),
            ..awaiting_booking()
        };
        awaiting_the_council
            .coherent()
            .expect("a cancellation requested mid-booking must be representable");
        assert_eq!(
            awaiting_the_council.state.in_flight_kind(),
            Some(OperationKind::Book),
            "the effect in flight is the booking's, not a cancellation's"
        );
    }

    /// The old unit payload is refused rather than defaulted.
    ///
    /// `CancellationRequested` is unreachable today — `BookingInProgress` has no
    /// outbound behaviours — so no known row carries it, and the only
    /// construction sites in the tree are fixtures. A defaulting deserializer
    /// would invent a legacy that does not exist, and would have to invent an
    /// effect identity out of nothing to do it. Failing loudly is the right
    /// direction; this pins that it does.
    ///
    /// Contrast `legacy_null_state_payload_still_decodes`, which pins a shape
    /// that *is* deliberately supported. Both are wire-format assertions and
    /// neither is subsumed by a transition test.
    #[test]
    fn the_old_cancellation_requested_payload_is_deliberately_rejected() {
        let legacy = r#"{"state":"CancellationRequested","data":null}"#;
        let decoded = serde_json::from_str::<BookingState>(legacy);
        assert!(
            decoded.is_err(),
            "the old unit payload must fail closed, not default an effect identity"
        );
    }

    // ---------------------------------------------------- effect coherence
    //
    // C1 gave the booking one definition of self-consistency and gated it on read
    // and write. The intent was skipped, and the gap showed as a symptom: the
    // fact door hand-rolled the same rules in three places. These pin the single
    // definition that replaced them.

    fn sound_intent() -> EffectIntent {
        EffectIntent {
            effect_intent_id: EffectIntentId::new("EFF-BKG-1001-BOOK-0"),
            booking_id: BookingId::new("BKG-1001"),
            operation_kind: OperationKind::Book,
            source_version: 0,
            canonical_plan: BookingEffect::Book {
                principal: PrincipalId::new("lucy"),
                attendees: 20,
                facts: good_facts(),
            },
            status: EffectStatus::Prepared,
            expires_at_ms: 1_000_030_000,
            provider_reference: None,
            outcome_detail: None,
            supersedes: None,
            created_at_ms: 1_000_000_000,
            updated_at_ms: 1_000_000_000,
        }
    }

    #[test]
    fn a_sound_intent_is_coherent() {
        sound_intent()
            .coherent()
            .expect("the fixture must be sound, or every test below proves nothing");
    }

    /// A `Confirmed` effect without its reference would converge against *any*
    /// reference the fact door was shown. Every other status carrying one names an
    /// effect that officially never happened.
    #[test]
    fn only_a_confirmed_effect_may_name_a_provider_reference() {
        let confirmed_without = EffectIntent {
            status: EffectStatus::Confirmed,
            provider_reference: None,
            ..sound_intent()
        };
        assert!(matches!(
            confirmed_without.coherent(),
            Err(IncoherentIntent::OutcomeShape { .. })
        ));

        for status in [
            EffectStatus::Prepared,
            EffectStatus::Unknown,
            EffectStatus::Absent,
            EffectStatus::Rejected,
        ] {
            let carrying = EffectIntent {
                status,
                provider_reference: Some(CouncilBookingRef::new("TH-92718")),
                ..sound_intent()
            };
            assert!(
                matches!(
                    carrying.coherent(),
                    Err(IncoherentIntent::OutcomeShape { .. })
                ),
                "{status:?} must not carry a reference"
            );
        }

        let confirmed_with = EffectIntent {
            status: EffectStatus::Confirmed,
            provider_reference: Some(CouncilBookingRef::new("TH-92718")),
            ..sound_intent()
        };
        confirmed_with
            .coherent()
            .expect("Confirmed with its reference is the sound shape");
    }

    /// `Prepared` means the capability has not been called, so there is nothing
    /// to explain. `Unknown` means it has and the answer was ambiguous — a detail
    /// is genuinely useful there, so only the first is constrained.
    #[test]
    fn a_prepared_effect_has_nothing_to_explain_yet() {
        let premature = EffectIntent {
            outcome_detail: Some(BoundedString::truncating("hall closed")),
            ..sound_intent()
        };
        assert!(matches!(
            premature.coherent(),
            Err(IncoherentIntent::PrematureDetail)
        ));

        let ambiguous = EffectIntent {
            status: EffectStatus::Unknown,
            outcome_detail: Some(BoundedString::truncating("timed out after 30s")),
            ..sound_intent()
        };
        ambiguous
            .coherent()
            .expect("an Unknown outcome may record why it is unknown");
    }

    /// The persisted discriminator and the plan it describes are two copies of one
    /// fact. `PrepareEffect` derives the column from the plan, so they agree by
    /// construction on the way in; this catches a row where they no longer do.
    #[test]
    fn the_recorded_kind_must_match_the_plan_it_describes() {
        let disagreeing = EffectIntent {
            operation_kind: OperationKind::Cancel,
            ..sound_intent()
        };
        assert!(matches!(
            disagreeing.coherent(),
            Err(IncoherentIntent::KindDisagreesWithPlan {
                column: "Cancel",
                plan: "Book"
            })
        ));
    }

    #[test]
    fn an_effect_cannot_supersede_itself() {
        let ouroboros = EffectIntent {
            supersedes: Some(EffectIntentId::new("EFF-BKG-1001-BOOK-0")),
            ..sound_intent()
        };
        assert!(matches!(
            ouroboros.coherent(),
            Err(IncoherentIntent::SupersedesItself(_))
        ));
    }

    /// A state whose booking has never been confirmed cannot know a council
    /// reference. Forward-only checking left this open, and B3b's review found
    /// the consequence: the fact door silently cleared a phantom reference on
    /// its way through a transition.
    #[test]
    fn a_state_that_cannot_know_a_reference_must_not_carry_one() {
        let phantom = Booking {
            booking_ref: Some(CouncilBookingRef::new("TH-PHANTOM")),
            ..awaiting_booking()
        };
        assert!(matches!(
            phantom.coherent(),
            Err(IncoherentBooking::PhantomReference { .. })
        ));
    }

    /// The freeze state keeps whatever was known when automation gave up —
    /// a `NeedsHuman` reached from `CancellingBooking` legitimately carries the
    /// reference of the booking it was trying to cancel.
    #[test]
    fn needs_human_may_retain_the_reference_it_froze_with() {
        let frozen = Booking {
            state: BookingState::NeedsHuman(NeedsHuman),
            active_effect: None,
            ..booked()
        };
        assert!(
            frozen.coherent().is_ok(),
            "NeedsHuman must be allowed to keep the reference it froze with"
        );
    }

    /// Selection and reference are checked only where the state names one, so a
    /// terminal state may keep an outer value it no longer mentions. B3b's fact
    /// door relies on this: convergence at `Cancelled` compares the reference of
    /// the booking that was cancelled, which only the aggregate still holds.
    #[test]
    fn a_terminal_state_may_retain_a_reference_it_no_longer_names() {
        let cancelled = Booking {
            state: BookingState::Cancelled(Cancelled),
            ..booked()
        };
        assert!(
            cancelled.coherent().is_ok(),
            "Cancelled must be allowed to keep the reference it cancelled"
        );
    }

    /// Round two of the laundering hunt: the PROPOSAL door was the third door
    /// without the self-consistency step. An incoherent `VenueSelected` carrying
    /// a phantom reference could take Cancel into Cancelled — whose reference
    /// is legitimately unconstrained — and the phantom became terminal history
    /// indistinguishable from a real cancellation.
    #[tokio::test]
    async fn the_proposal_door_refuses_an_incoherent_booking_too() {
        // A phantom reference laundered through Cancel.
        let phantom = Booking {
            booking_ref: Some(CouncilBookingRef::new("TH-PHANTOM")),
            ..venue_selected()
        };
        let got = turn(
            phantom,
            BookingProposal::Cancel {
                reason: "changed mind".to_owned(),
            },
            &authority(),
            &context(),
        )
        .await;
        assert!(
            matches!(
                got,
                Resolution::Denied(BookingError::IncoherentAggregate(
                    IncoherentBooking::PhantomReference { .. }
                ))
            ),
            "a phantom reference must not become terminal history, got {got:?}"
        );

        // A selection disagreement laundered through ChangeVenue, which clears
        // both copies and would erase the evidence.
        let mismatched = Booking {
            selected_venue: Some(SelectedVenueRef {
                venue_id: VenueId::new("TH-Z"),
                slot_id: SlotId::new("SLOT-A"),
            }),
            ..venue_selected()
        };
        let got = turn(
            mismatched,
            BookingProposal::ChangeVenue,
            &authority(),
            &context(),
        )
        .await;
        assert!(
            matches!(
                got,
                Resolution::Denied(BookingError::IncoherentAggregate(
                    IncoherentBooking::Selection { .. }
                ))
            ),
            "a selection disagreement must not be cleared away, got {got:?}"
        );

        // An effect-pointer disagreement keeps its own, older name.
        let contradictory = Booking {
            active_effect: Some(EffectIntentId::new("EFF-SOMETHING-ELSE")),
            ..booked()
        };
        let got = turn(
            contradictory,
            BookingProposal::Cancel {
                reason: "changed mind".to_owned(),
            },
            &authority(),
            &context(),
        )
        .await;
        assert_eq!(
            got,
            Resolution::Denied(BookingError::InconsistentEffectIdentity)
        );
    }

    /// The ordering half of the same rule: whether a behaviour EXISTS depends
    /// on (state, proposal) alone, so an Undefined cell stays Undefined no
    /// matter how broken the aggregate is. Coherence turns Ready into Denied;
    /// it must never turn Undefined into anything.
    #[tokio::test]
    async fn incoherence_cannot_make_an_undefined_cell_exist() {
        let phantom = Booking {
            booking_ref: Some(CouncilBookingRef::new("TH-PHANTOM")),
            ..draft()
        };
        let got = turn(phantom, BookingProposal::Book, &authority(), &context()).await;
        assert!(
            matches!(got, Resolution::Undefined),
            "Draft has no book; a phantom reference must not conjure one, got {got:?}"
        );
    }

    /// `intended_effect_kind` restates what the transition graph already knows, so
    /// the agreement is proved rather than assumed: across every legal cell it must
    /// be `Some` exactly where the plan is an `ExternalEffect`, and must name the
    /// same kind the effect carries.
    ///
    /// This is the discipline that makes the duplication survivable. Without it,
    /// adding a third external edge would leave the coordinator deriving a
    /// `Book` identity for a `Cancel` effect — and the store would refuse it,
    /// correctly but confusingly, one layer away from the mistake.
    #[tokio::test]
    async fn the_intended_effect_kind_agrees_with_the_topology() {
        let mut external_cells = 0_usize;
        for source in [
            draft(),
            venue_selected(),
            needs_revalidation(),
            awaiting_booking(),
            booked(),
        ] {
            for proposal in [
                BookingProposal::SelectVenue {
                    venue_id: VenueId::new("TH-A"),
                    slot_id: SlotId::new("SLOT-A"),
                },
                BookingProposal::VerifySlot,
                BookingProposal::ChangeVenue,
                BookingProposal::UpdateRequirements { attendees: None },
                BookingProposal::RevalidateVenue,
                BookingProposal::Book,
                BookingProposal::Cancel {
                    reason: "changed mind".to_owned(),
                },
            ] {
                let cell = format!("{} + {}", source.state.name(), proposal.name());
                let predicted = TownHallDomain::intended_effect_kind(&source.state, &proposal);
                let got = turn(source.clone(), proposal, &authority(), &context()).await;

                match got {
                    Resolution::Ready(TransitionPlan::ExternalEffect { effect, .. }) => {
                        external_cells += 1;
                        assert_eq!(
                            predicted,
                            Some(effect.operation_kind()),
                            "{cell} is external; the prediction must name its kind"
                        );
                    }
                    _ => assert_eq!(
                        predicted, None,
                        "{cell} is not external; the prediction must say so"
                    ),
                }
            }
        }
        assert_eq!(
            external_cells, 2,
            "the proposal door has exactly two external edges; a change needs an ADR"
        );
    }

    /// `establishes` keeps the status and its reference together, so the shape
    /// `EffectIntent::coherent` requires holds by construction rather than by the
    /// caller assembling it correctly.
    #[test]
    fn what_a_fact_establishes_is_always_a_coherent_shape() {
        let effect = EffectIntentId::new("EFF-BKG-1001-BOOK-0");
        let facts = [
            VerifiedProviderFact::BookingExists {
                effect_intent_id: effect.clone(),
                booking_ref: CouncilBookingRef::new("TH-92718"),
                venue_id: VenueId::new("TH-A"),
                slot_id: SlotId::new("SLOT-A"),
                attendees: 20,
                fee: Money::from_pence(4_500),
                principal: PrincipalId::new("lucy"),
            },
            VerifiedProviderFact::CancellationExists {
                effect_intent_id: effect.clone(),
                booking_ref: CouncilBookingRef::new("TH-92718"),
            },
            VerifiedProviderFact::EffectAbsent {
                effect_intent_id: effect.clone(),
            },
            VerifiedProviderFact::ProviderRejected {
                effect_intent_id: effect.clone(),
                reason: BoundedString::truncating("hall closed"),
            },
        ];

        for fact in facts {
            let name = fact.name();
            let outcome = fact.establishes();
            assert!(
                outcome.status.is_terminal(),
                "{name} must establish a terminal outcome"
            );
            // Project it onto an intent and check the shared definition accepts it.
            let projected = EffectIntent {
                status: outcome.status,
                provider_reference: outcome.provider_reference.clone(),
                outcome_detail: outcome.detail.clone(),
                ..sound_intent()
            };
            projected.coherent().unwrap_or_else(|why| {
                panic!("{name} establishes a shape the read gate would refuse: {why}")
            });
        }
    }

    /// A transition changes a booking; it never changes which booking.
    #[tokio::test]
    async fn no_transition_changes_the_identity() {
        for source in [
            draft(),
            venue_selected(),
            needs_revalidation(),
            awaiting_booking(),
            booked(),
        ] {
            for proposal in [
                BookingProposal::SelectVenue {
                    venue_id: VenueId::new("TH-A"),
                    slot_id: SlotId::new("SLOT-A"),
                },
                BookingProposal::VerifySlot,
                BookingProposal::ChangeVenue,
                BookingProposal::UpdateRequirements { attendees: None },
                BookingProposal::RevalidateVenue,
                BookingProposal::Book,
                BookingProposal::Cancel {
                    reason: "changed mind".to_owned(),
                },
            ] {
                let name = proposal.name();
                let state_name = source.state.name();
                if let Resolution::Ready(plan) =
                    turn(source.clone(), proposal, &authority(), &context()).await
                {
                    assert_eq!(
                        plan.next_state().id,
                        source.id,
                        "{state_name} + {name} changed the booking identity"
                    );
                }
            }
        }
    }

    // ------------------------------------------------ preserved local cells
    //
    // These twelve must produce byte-identical outcomes after slice B. They are
    // the regression surface for the refactor.

    #[tokio::test]
    async fn draft_select_venue() {
        let got = turn(
            draft(),
            BookingProposal::SelectVenue {
                venue_id: VenueId::new("TH-A"),
                slot_id: SlotId::new("SLOT-A"),
            },
            &authority(),
            &context(),
        )
        .await;
        assert_eq!(got, committed_local(venue_selected()));
    }

    #[tokio::test]
    async fn draft_cancel() {
        let got = turn(
            draft(),
            BookingProposal::Cancel {
                reason: "changed mind".to_owned(),
            },
            &authority(),
            &context(),
        )
        .await;
        assert_eq!(
            got,
            committed_local(Booking {
                state: BookingState::Cancelled(Cancelled),
                ..draft()
            })
        );
    }

    #[tokio::test]
    async fn venue_selected_verify_slot() {
        let got = turn(
            venue_selected(),
            BookingProposal::VerifySlot,
            &authority(),
            &context(),
        )
        .await;
        // Verifying a slot records the facts it verified. Without that, the
        // aggregate would claim a verified fee with nothing behind it.
        assert_eq!(got, committed_local(awaiting_booking()));
    }

    #[tokio::test]
    async fn venue_selected_change_venue() {
        let got = turn(
            venue_selected(),
            BookingProposal::ChangeVenue,
            &authority(),
            &context(),
        )
        .await;
        // Starting over abandons the selection, so the loaded facts describe a
        // venue nobody has chosen. Both must go, or a later revalidation could
        // bind to them.
        assert_eq!(
            got,
            committed_local(Booking {
                state: BookingState::Draft(Draft),
                selected_venue: None,
                availability: None,
                ..venue_selected()
            })
        );
    }

    /// The selection must be carried forward — this is the field that closed
    /// the venue-substitution bug, so the refactor must not drop it.
    ///
    /// B3a adds the other half: the *patch* must be carried forward too. Before
    /// B3a the 25 was destructured away, so the next capacity check validated
    /// against the old headcount.
    #[tokio::test]
    async fn venue_selected_update_requirements_carries_the_selection_and_the_patch() {
        let got = turn(
            venue_selected(),
            BookingProposal::UpdateRequirements {
                attendees: Some(25),
            },
            &authority(),
            &context(),
        )
        .await;
        assert_eq!(
            got,
            committed_local(Booking {
                requirements: BookingRequirements {
                    attendees: 25,
                    ..requirements()
                },
                ..needs_revalidation()
            })
        );
    }

    #[tokio::test]
    async fn venue_selected_cancel() {
        let got = turn(
            venue_selected(),
            BookingProposal::Cancel {
                reason: "changed mind".to_owned(),
            },
            &authority(),
            &context(),
        )
        .await;
        // Abandoning keeps the selection: it records what was being attempted,
        // and `Cancelled` has no behaviour that could act on it.
        assert_eq!(
            got,
            committed_local(Booking {
                state: BookingState::Cancelled(Cancelled),
                ..venue_selected()
            })
        );
    }

    #[tokio::test]
    async fn needs_revalidation_revalidate_venue() {
        let got = turn(
            needs_revalidation(),
            BookingProposal::RevalidateVenue,
            &authority(),
            &context(),
        )
        .await;
        // Revalidation records the facts it just checked, exactly as
        // `VerifySlot` does.
        assert_eq!(
            got,
            committed_local(Booking {
                availability: Some(good_facts()),
                ..venue_selected()
            })
        );
    }

    #[tokio::test]
    async fn needs_revalidation_change_venue() {
        let got = turn(
            needs_revalidation(),
            BookingProposal::ChangeVenue,
            &authority(),
            &context(),
        )
        .await;
        assert_eq!(
            got,
            committed_local(Booking {
                state: BookingState::Draft(Draft),
                selected_venue: None,
                availability: None,
                ..needs_revalidation()
            })
        );
    }

    #[tokio::test]
    async fn needs_revalidation_cancel() {
        let got = turn(
            needs_revalidation(),
            BookingProposal::Cancel {
                reason: "changed mind".to_owned(),
            },
            &authority(),
            &context(),
        )
        .await;
        assert_eq!(
            got,
            committed_local(Booking {
                state: BookingState::Cancelled(Cancelled),
                ..needs_revalidation()
            })
        );
    }

    #[tokio::test]
    async fn awaiting_booking_change_venue() {
        let got = turn(
            awaiting_booking(),
            BookingProposal::ChangeVenue,
            &authority(),
            &context(),
        )
        .await;
        // Starting over abandons the selection, so the loaded facts describe a
        // venue nobody has chosen. Both must go, or a later revalidation could
        // bind to them.
        assert_eq!(
            got,
            committed_local(Booking {
                state: BookingState::Draft(Draft),
                selected_venue: None,
                availability: None,
                ..awaiting_booking()
            })
        );
    }

    #[tokio::test]
    async fn awaiting_booking_update_requirements_carries_the_selection_and_the_patch() {
        let got = turn(
            awaiting_booking(),
            BookingProposal::UpdateRequirements {
                attendees: Some(25),
            },
            &authority(),
            &context(),
        )
        .await;
        // `availability` was verified against 20 people; it says nothing about
        // 25, so it must not survive the change.
        assert_eq!(
            got,
            committed_local(Booking {
                requirements: BookingRequirements {
                    attendees: 25,
                    ..requirements()
                },
                ..needs_revalidation()
            })
        );
    }

    /// `CancellingBooking` gained a required `effect_intent_id` in B2, which
    /// would break existing rows — except none can exist.
    ///
    /// Verified against history: **no production path ever constructed it**.
    /// `townhall-store` has zero construction sites on `main`, and outside
    /// `#[cfg(test)]` the domain has none either — `validate` mapped a
    /// cancellation straight to `Cancelled`, so the state was declared but
    /// never reached. Test fixtures did construct it, which is why the claim is
    /// scoped to production paths rather than "anywhere"; an earlier draft said
    /// "anywhere" and that was wrong.
    ///
    /// This pins the new shape. If a legacy row ever *did* turn up, it would
    /// fail loudly here rather than silently — which is the right direction.
    #[test]
    fn cancelling_booking_round_trips_in_its_new_shape() {
        let state = BookingState::CancellingBooking(CancellingBooking {
            booking_ref: CouncilBookingRef::new("TH-92718"),
            effect_intent_id: EffectIntentId::new("EFF-BKG-1001-CANCEL-1"),
        });
        let json = serde_json::to_string(&state).expect("serialize");
        assert_eq!(
            serde_json::from_str::<BookingState>(&json).expect("round trip"),
            state
        );
    }

    /// A row persisted before `NeedsRevalidation` carried a selection must still
    /// decode, or M3's restart-survival gate breaks for every existing row.
    ///
    /// Carried over from the legacy `tests` module deleted in B2. Review caught
    /// that I had dropped it — the one piece of coverage `characterization` did
    /// not subsume, because it tests the wire format rather than a transition.
    #[test]
    fn legacy_null_state_payload_still_decodes() {
        let legacy = r#"{"state":"NeedsRevalidation","data":null}"#;
        let decoded: BookingState =
            serde_json::from_str(legacy).expect("legacy NeedsRevalidation row must still load");
        assert_eq!(
            decoded,
            BookingState::NeedsRevalidation(NeedsRevalidation { selected: None })
        );
    }

    // ------------------------------------------------ denials, one defect each
    //
    // Every fixture below starts from `good_facts()` and breaks exactly one
    // thing, so the expected error is forced by meaning rather than by which
    // guard the implementation happens to check first.

    #[tokio::test]
    async fn verify_slot_denies_when_no_facts_were_loaded() {
        let mut ctx = context();
        ctx.selected_facts = None;
        let got = turn(
            venue_selected(),
            BookingProposal::VerifySlot,
            &authority(),
            &ctx,
        )
        .await;
        assert_eq!(got, Resolution::Denied(BookingError::VenueFactsMissing));
    }

    #[tokio::test]
    async fn verify_slot_denies_facts_for_a_different_venue() {
        let mut ctx = context();
        ctx.selected_facts = Some(VenueFacts {
            venue_id: VenueId::new("TH-B"),
            ..good_facts()
        });
        let got = turn(
            venue_selected(),
            BookingProposal::VerifySlot,
            &authority(),
            &ctx,
        )
        .await;
        assert_eq!(got, Resolution::Denied(BookingError::VenueFactsMissing));
    }

    #[tokio::test]
    async fn verify_slot_denies_an_unavailable_slot() {
        let mut ctx = context();
        ctx.selected_facts = Some(VenueFacts {
            available: false,
            ..good_facts()
        });
        let got = turn(
            venue_selected(),
            BookingProposal::VerifySlot,
            &authority(),
            &ctx,
        )
        .await;
        assert_eq!(got, Resolution::Denied(BookingError::SlotUnavailable));
    }

    #[tokio::test]
    async fn verify_slot_denies_insufficient_capacity() {
        let mut ctx = context();
        ctx.selected_facts = Some(VenueFacts {
            capacity: 12,
            ..good_facts()
        });
        let got = turn(
            venue_selected(),
            BookingProposal::VerifySlot,
            &authority(),
            &ctx,
        )
        .await;
        assert_eq!(
            got,
            Resolution::Denied(BookingError::CapacityInsufficient {
                capacity: 12,
                required: 20
            })
        );
    }

    #[tokio::test]
    async fn verify_slot_denies_an_inaccessible_venue() {
        let mut ctx = context();
        ctx.selected_facts = Some(VenueFacts {
            wheelchair_accessible: false,
            ..good_facts()
        });
        let got = turn(
            venue_selected(),
            BookingProposal::VerifySlot,
            &authority(),
            &ctx,
        )
        .await;
        assert_eq!(got, Resolution::Denied(BookingError::AccessibilityRequired));
    }

    /// The £45 / £50 / £90 case from the spec: the effective ceiling is the
    /// tighter of the user's requirement and the delegated authority.
    #[tokio::test]
    async fn verify_slot_denies_a_fee_over_the_ceiling() {
        let mut ctx = context();
        ctx.selected_facts = Some(VenueFacts {
            fee: Money::from_pence(9_000),
            ..good_facts()
        });
        let got = turn(
            venue_selected(),
            BookingProposal::VerifySlot,
            &authority(),
            &ctx,
        )
        .await;
        assert_eq!(got, Resolution::Denied(BookingError::FeeExceeded));
    }

    /// A legacy row decoded with no selection cannot revalidate — fail closed
    /// rather than trust whatever the context happens to carry.
    #[tokio::test]
    async fn revalidate_denies_when_the_state_carries_no_selection() {
        let legacy = Booking {
            state: BookingState::NeedsRevalidation(NeedsRevalidation { selected: None }),
            ..needs_revalidation()
        };
        let got = turn(
            legacy,
            BookingProposal::RevalidateVenue,
            &authority(),
            &context(),
        )
        .await;
        assert_eq!(got, Resolution::Denied(BookingError::VenueFactsMissing));
    }

    #[tokio::test]
    async fn revalidate_denies_facts_for_a_different_venue() {
        let mut ctx = context();
        ctx.selected_facts = Some(VenueFacts {
            venue_id: VenueId::new("TH-B"),
            ..good_facts()
        });
        let got = turn(
            needs_revalidation(),
            BookingProposal::RevalidateVenue,
            &authority(),
            &ctx,
        )
        .await;
        assert_eq!(got, Resolution::Denied(BookingError::VenueFactsMissing));
    }

    /// Exactly one defect: booking authority is absent, and everything else is
    /// valid. A fixture that also blanked the facts would pin whichever guard
    /// runs first.
    #[tokio::test]
    async fn book_denies_without_booking_authority() {
        let auth = VerifiedAuthority {
            may_book: false,
            ..authority()
        };
        let got = turn(awaiting_booking(), BookingProposal::Book, &auth, &context()).await;
        assert_eq!(
            got,
            Resolution::Denied(BookingError::BookingAuthorityRequired)
        );
    }

    /// The fee changed between verification and booking. `AwaitingBooking`
    /// carries the fee it verified precisely so this is detectable.
    #[tokio::test]
    async fn book_denies_when_the_fee_moved_since_verification() {
        let mut ctx = context();
        ctx.selected_facts = Some(VenueFacts {
            fee: Money::from_pence(4_600),
            ..good_facts()
        });
        let got = turn(
            awaiting_booking(),
            BookingProposal::Book,
            &authority(),
            &ctx,
        )
        .await;
        assert_eq!(got, Resolution::Denied(BookingError::VenueFactsMissing));
    }

    #[tokio::test]
    async fn cancel_denies_a_booked_resource_without_cancellation_authority() {
        let auth = VerifiedAuthority {
            may_cancel: false,
            ..authority()
        };
        let got = turn(
            booked(),
            BookingProposal::Cancel {
                reason: "changed mind".to_owned(),
            },
            &auth,
            &context(),
        )
        .await;
        assert_eq!(
            got,
            Resolution::Denied(BookingError::CancellationAuthorityRequired)
        );
    }

    // ------------------------------------------- the two external-effect cells
    //
    // These two changed deliberately in B2, which is why "tests unchanged" could
    // not be its gate. B1 pinned each of them twice — the old behaviour as an
    // ordinary test, and the intended new behaviour `#[ignore]`d — so that
    // whoever made the change would be forced to confront both. That worked: the
    // old assertions failed the moment B2 landed, and were deleted alongside
    // unignoring these. What remains below is simply what the cells do now.

    /// `Book` stops at `BookingInProgress`: the effect intent is committed
    /// before the council is called (ADR-014).
    #[tokio::test]
    async fn book_stops_at_booking_in_progress_with_an_effect_to_persist() {
        let got = turn(
            awaiting_booking(),
            BookingProposal::Book,
            &authority(),
            &context(),
        )
        .await;
        let Resolution::Ready(plan) = &got else {
            panic!("Book must resolve to a plan, got {got:?}");
        };
        let next = plan.next_state();
        let BookingState::BookingInProgress(in_progress) = &next.state else {
            panic!("Book must stop at BookingInProgress, got {:?}", next.state);
        };
        // The aggregate's own pointer must agree with the state's. They are two
        // copies of one fact, and B3a is what created the second one.
        assert_eq!(
            next.active_effect.as_ref(),
            Some(&in_progress.effect_intent_id),
            "active_effect must name the same effect the state is waiting on"
        );
        // And it must be an ExternalEffect, not a Local one — that distinction is
        // what forces the intent to be persisted before the council is called.
        assert!(
            matches!(plan, TransitionPlan::ExternalEffect { .. }),
            "Book must be an ExternalEffect so its intent is persisted first"
        );

        // Matching the variant is not enough: B2 could produce the right state
        // with the wrong effect identity and this would still pass.
        //
        // What this pins is *adoption*: the domain takes the identity the
        // coordinator supplied and does not invent, alter or drop one. It does
        // not pin determinism — the domain cannot derive an effect identity,
        // because the repository holds the uniqueness key. Repeating the turn
        // with the same context proves the domain is a pure function of its
        // inputs, not that those inputs are stable; that is
        // `derive_effect_intent_id`'s contract and it is tested in
        // `townhall-store`. An earlier version of this comment claimed the
        // stronger property, which the assertion below cannot reach.
        assert_eq!(
            Some(&in_progress.effect_intent_id),
            context().pending_effect.as_ref(),
            "Book must adopt the supplied identity unchanged"
        );
        let again = turn(
            awaiting_booking(),
            BookingProposal::Book,
            &authority(),
            &context(),
        )
        .await;
        assert_eq!(got, again, "classification must be a pure function");
    }

    /// `Booked + Cancel` stops at `CancellingBooking`. This is the *ordinary*
    /// cancellation path, not the in-flight one — had it stayed local, an
    /// ordinary cancel would commit `Cancelled` while the council booking
    /// stayed live for every slice between the coordinator landing and F.
    #[tokio::test]
    async fn booked_cancel_stops_at_cancelling_booking_with_an_effect() {
        // A cancellation is its own effect with its own identity — reusing the
        // booking's id would make "has this effect completed?" unanswerable.
        let ctx = BookingContext {
            pending_effect: Some(EffectIntentId::new("EFF-BKG-1001-CANCEL-1")),
            ..context()
        };
        let got = turn(
            booked(),
            BookingProposal::Cancel {
                reason: "changed mind".to_owned(),
            },
            &authority(),
            &ctx,
        )
        .await;
        // Full equality, not just the variant. The reference must be carried
        // through from `Booked` — cancelling the wrong council booking is
        // exactly what this state exists to make impossible.
        assert_eq!(
            got,
            Resolution::Ready(TransitionPlan::ExternalEffect {
                next_state: Booking {
                    state: BookingState::CancellingBooking(CancellingBooking {
                        booking_ref: CouncilBookingRef::new("TH-92718"),
                        effect_intent_id: EffectIntentId::new("EFF-BKG-1001-CANCEL-1"),
                    }),
                    active_effect: Some(EffectIntentId::new("EFF-BKG-1001-CANCEL-1")),
                    ..booked()
                },
                effect: BookingEffect::CancelBooking {
                    booking_ref: CouncilBookingRef::new("TH-92718"),
                },
            }),
            "Booked + Cancel must be an ExternalEffect stopping at CancellingBooking, \
             carrying the same reference"
        );
    }

    // `reconcile_is_undefined_everywhere_today` lived here. Its doc said "when
    // this stops compiling, that is the change landing" — and B2 removed the
    // variant, so it did. Recovery is runtime machinery now, reached through the
    // verified-fact door in B3, never proposed.

    #[tokio::test]
    async fn awaiting_booking_cancel() {
        let got = turn(
            awaiting_booking(),
            BookingProposal::Cancel {
                reason: "changed mind".to_owned(),
            },
            &authority(),
            &context(),
        )
        .await;
        assert_eq!(
            got,
            committed_local(Booking {
                state: BookingState::Cancelled(Cancelled),
                ..awaiting_booking()
            })
        );
    }
}

/// The state × fact topology, pinned — 10 states × 4 facts = 40 cells.
///
/// Mirrors `docs/state-machine.md`'s completeness matrix and its three
/// meanings of absence. The three Waiting rows are that document's rows
/// verbatim; the Settled rows encode "a fact landing at its own destination
/// must already be reflected"; the Absent rows are `Undefined` before any
/// guard runs.
///
/// Fixture convention ("fully bound"): the fact's identity is the one the
/// state waits on where it waits on something, else the edge intent that lands
/// at that state; the intent's kind is the one the fact implies where the fact
/// is kind-specific, else the state's in-flight or edge kind; status and
/// reference are shape-valid and consistent — `(Unknown, None)` at Waiting
/// states, `(Confirmed, Some)` / `(Absent, None)` at Settled ones.
#[cfg(test)]
mod fact_topology {
    use super::*;
    use bld_types::{BookingRequirements, Money, TimeWindow};

    const BOOK_ID: &str = "EFF-BKG-1001-BOOK-2";
    const CANCEL_ID: &str = "EFF-BKG-1001-CANCEL-5";
    /// A fresh identity the coordinator would mint for the fact-driven
    /// cancellation. Deliberately distinct from both in-flight ids.
    const FRESH_CANCEL_ID: &str = "EFF-BKG-1001-CANCEL-9";
    const REF: &str = "TH-92718";

    fn requirements() -> BookingRequirements {
        BookingRequirements {
            purpose: "meeting".to_owned(),
            requested_date: "2026-08-20".to_owned(),
            time_window: TimeWindow {
                from: "13:00".to_owned(),
                to: "17:00".to_owned(),
            },
            attendees: 20,
            wheelchair_accessible: true,
            max_fee: Money::from_pence(5_000),
        }
    }

    fn good_facts() -> VenueFacts {
        VenueFacts {
            venue_id: VenueId::new("TH-A"),
            slot_id: SlotId::new("SLOT-A"),
            capacity: 30,
            wheelchair_accessible: true,
            fee: Money::from_pence(4_500),
            available: true,
        }
    }

    fn book_plan() -> BookingEffect {
        BookingEffect::Book {
            principal: PrincipalId::new("lucy"),
            attendees: 20,
            facts: good_facts(),
        }
    }

    fn cancel_plan() -> BookingEffect {
        BookingEffect::CancelBooking {
            booking_ref: CouncilBookingRef::new(REF),
        }
    }

    fn intent(
        id: &str,
        kind: OperationKind,
        status: EffectStatus,
        provider_reference: Option<&str>,
    ) -> EffectIntent {
        EffectIntent {
            effect_intent_id: EffectIntentId::new(id),
            booking_id: BookingId::new("BKG-1001"),
            operation_kind: kind,
            source_version: 2,
            canonical_plan: match kind {
                OperationKind::Book => book_plan(),
                OperationKind::Cancel => cancel_plan(),
            },
            status,
            expires_at_ms: 1_000_030_000,
            provider_reference: provider_reference.map(CouncilBookingRef::new),
            outcome_detail: None,
            supersedes: None,
            created_at_ms: 1_000_000_000,
            updated_at_ms: 1_000_000_000,
        }
    }

    /// A complete, coherent booking around `state`. `booking_ref` and
    /// `active_effect` are the two fields whose right value depends on the
    /// state, so they are explicit.
    fn booking_at(
        state: BookingState,
        booking_ref: Option<&str>,
        active_effect: Option<&str>,
    ) -> Booking {
        let booking = Booking {
            id: BookingId::new("BKG-1001"),
            state,
            requirements: requirements(),
            selected_venue: Some(SelectedVenueRef {
                venue_id: VenueId::new("TH-A"),
                slot_id: SlotId::new("SLOT-A"),
            }),
            availability: Some(good_facts()),
            booking_ref: booking_ref.map(CouncilBookingRef::new),
            active_effect: active_effect.map(EffectIntentId::new),
        };
        booking
            .coherent()
            .expect("every fixture must be coherent, or the sweep tests nothing");
        booking
    }

    fn booking_in_progress() -> Booking {
        booking_at(
            BookingState::BookingInProgress(BookingInProgress {
                effect_intent_id: EffectIntentId::new(BOOK_ID),
            }),
            None,
            Some(BOOK_ID),
        )
    }

    fn cancellation_requested() -> Booking {
        booking_at(
            BookingState::CancellationRequested(CancellationRequested {
                effect_intent_id: EffectIntentId::new(BOOK_ID),
            }),
            None,
            Some(BOOK_ID),
        )
    }

    fn cancelling_booking() -> Booking {
        booking_at(
            BookingState::CancellingBooking(CancellingBooking {
                booking_ref: CouncilBookingRef::new(REF),
                effect_intent_id: EffectIntentId::new(CANCEL_ID),
            }),
            Some(REF),
            Some(CANCEL_ID),
        )
    }

    fn awaiting_booking() -> Booking {
        booking_at(
            BookingState::AwaitingBooking(AwaitingBooking {
                venue_id: VenueId::new("TH-A"),
                slot_id: SlotId::new("SLOT-A"),
                verified_fee: Money::from_pence(4_500),
            }),
            None,
            None,
        )
    }

    fn booked() -> Booking {
        booking_at(
            BookingState::Booked(Booked {
                booking_ref: CouncilBookingRef::new(REF),
            }),
            Some(REF),
            None,
        )
    }

    /// `Cancelled` has two legitimate histories with different references: a
    /// cancellation that completed (keeps the reference of what it cancelled)
    /// and a booking that never happened (no reference can exist).
    fn cancelled_after_cancellation() -> Booking {
        booking_at(BookingState::Cancelled(Cancelled), Some(REF), None)
    }

    fn cancelled_never_booked() -> Booking {
        booking_at(BookingState::Cancelled(Cancelled), None, None)
    }

    const FACT_COUNT: usize = 4;

    fn fact_index(fact: &VerifiedProviderFact) -> usize {
        match fact {
            VerifiedProviderFact::BookingExists { .. } => 0,
            VerifiedProviderFact::CancellationExists { .. } => 1,
            VerifiedProviderFact::EffectAbsent { .. } => 2,
            VerifiedProviderFact::ProviderRejected { .. } => 3,
        }
    }

    fn fact_of(index: usize, id: &str) -> VerifiedProviderFact {
        match index {
            0 => VerifiedProviderFact::BookingExists {
                effect_intent_id: EffectIntentId::new(id),
                booking_ref: CouncilBookingRef::new(REF),
                venue_id: VenueId::new("TH-A"),
                slot_id: SlotId::new("SLOT-A"),
                attendees: 20,
                fee: Money::from_pence(4_500),
                principal: PrincipalId::new("lucy"),
            },
            1 => VerifiedProviderFact::CancellationExists {
                effect_intent_id: EffectIntentId::new(id),
                booking_ref: CouncilBookingRef::new(REF),
            },
            2 => VerifiedProviderFact::EffectAbsent {
                effect_intent_id: EffectIntentId::new(id),
            },
            3 => VerifiedProviderFact::ProviderRejected {
                effect_intent_id: EffectIntentId::new(id),
                reason: BoundedString::truncating("hall closed for maintenance"),
            },
            _ => panic!("no fact at index {index}"),
        }
    }

    #[test]
    fn every_fact_variant_has_a_representative() {
        let mut seen = [false; FACT_COUNT];
        for index in 0..FACT_COUNT {
            seen[fact_index(&fact_of(index, BOOK_ID))] = true;
        }
        assert!(
            seen.iter().all(|hit| *hit),
            "fact_of() is missing a VerifiedProviderFact variant; the sweep would skip it"
        );
    }

    /// The expected outcome of one cell, by shape.
    enum Cell {
        U,
        C,
        R(&'static str),
        D(&'static str),
    }

    fn error_name(error: &BookingError) -> &'static str {
        match error {
            BookingError::BookingAuthorityRequired => "BookingAuthorityRequired",
            BookingError::CancellationAuthorityRequired => "CancellationAuthorityRequired",
            BookingError::VenueFactsMissing => "VenueFactsMissing",
            BookingError::SlotUnavailable => "SlotUnavailable",
            BookingError::CapacityInsufficient { .. } => "CapacityInsufficient",
            BookingError::AccessibilityRequired => "AccessibilityRequired",
            BookingError::FeeExceeded => "FeeExceeded",
            BookingError::EffectIdentityMissing => "EffectIdentityMissing",
            BookingError::EffectPlanMissing => "EffectPlanMissing",
            BookingError::EffectMismatch => "EffectMismatch",
            BookingError::EffectKindMismatch => "EffectKindMismatch",
            BookingError::EffectPlanMismatch { .. } => "EffectPlanMismatch",
            BookingError::DuplicateProviderEffect => "DuplicateProviderEffect",
            BookingError::ContradictoryProviderFact => "ContradictoryProviderFact",
            BookingError::InconsistentEffectIdentity => "InconsistentEffectIdentity",
            BookingError::IncoherentAggregate(_) => "IncoherentAggregate",
            BookingError::IncoherentIntent(_) => "IncoherentIntent",
        }
    }

    /// Spec-grounded. **A diff to this table means the fact-driven transition
    /// graph changed and needs an ADR** — the same review stop-sign as the
    /// proposal door's `LOCKED`.
    ///
    /// Columns: `BookingExists`, `CancellationExists`, `EffectAbsent`,
    /// `ProviderRejected`.
    const LOCKED_FACTS: &[(&str, [Cell; FACT_COUNT])] = &[
        ("Draft", [Cell::U, Cell::U, Cell::U, Cell::U]),
        ("VenueSelected", [Cell::U, Cell::U, Cell::U, Cell::U]),
        ("NeedsRevalidation", [Cell::U, Cell::U, Cell::U, Cell::U]),
        ("NeedsHuman", [Cell::U, Cell::U, Cell::U, Cell::U]),
        (
            "AwaitingBooking",
            [
                Cell::D("ContradictoryProviderFact"),
                Cell::D("ContradictoryProviderFact"),
                Cell::C,
                Cell::C,
            ],
        ),
        (
            "Booked",
            [
                Cell::C,
                Cell::D("ContradictoryProviderFact"),
                Cell::C,
                Cell::C,
            ],
        ),
        (
            "Cancelled",
            [
                Cell::D("ContradictoryProviderFact"),
                Cell::C,
                Cell::C,
                Cell::C,
            ],
        ),
        (
            "BookingInProgress",
            [
                Cell::R("Booked"),
                Cell::D("EffectKindMismatch"),
                Cell::R("AwaitingBooking"),
                Cell::R("AwaitingBooking"),
            ],
        ),
        (
            "CancellationRequested",
            [
                Cell::R("CancellingBooking"),
                Cell::D("EffectKindMismatch"),
                Cell::R("Cancelled"),
                Cell::R("Cancelled"),
            ],
        ),
        (
            "CancellingBooking",
            [
                Cell::D("EffectKindMismatch"),
                Cell::R("Cancelled"),
                Cell::R("Booked"),
                Cell::R("Booked"),
            ],
        ),
    ];

    /// Build the fully-bound fixture for one cell: the booking, the fact, and
    /// the context whose intent follows the convention in the module docs.
    // One arm per state, mirroring the matrix rows; length is the fixture
    // convention written out, not incidental complexity.
    #[allow(clippy::too_many_lines)]
    fn bound_cell(
        state_name: &str,
        fact_ix: usize,
    ) -> (Booking, VerifiedProviderFact, FactContext) {
        // Which intent id, kind, and status this cell binds.
        let (booking, id, kind) = match state_name {
            "Draft" => (
                booking_at(BookingState::Draft(Draft), None, None),
                BOOK_ID,
                OperationKind::Book,
            ),
            "VenueSelected" => (
                booking_at(
                    BookingState::VenueSelected(VenueSelected {
                        venue_id: VenueId::new("TH-A"),
                        slot_id: SlotId::new("SLOT-A"),
                    }),
                    None,
                    None,
                ),
                BOOK_ID,
                OperationKind::Book,
            ),
            "NeedsRevalidation" => (
                booking_at(
                    BookingState::NeedsRevalidation(NeedsRevalidation {
                        selected: Some(SelectedVenueRef {
                            venue_id: VenueId::new("TH-A"),
                            slot_id: SlotId::new("SLOT-A"),
                        }),
                    }),
                    None,
                    None,
                ),
                BOOK_ID,
                OperationKind::Book,
            ),
            "NeedsHuman" => (
                booking_at(BookingState::NeedsHuman(NeedsHuman), None, None),
                BOOK_ID,
                OperationKind::Book,
            ),
            // Waiting rows: the fact names the id the state waits on; the
            // intent's kind follows the fact where kind-specific.
            "BookingInProgress" => (
                booking_in_progress(),
                BOOK_ID,
                match fact_ix {
                    1 => OperationKind::Cancel, // CancellationExists implies it
                    _ => OperationKind::Book,
                },
            ),
            "CancellationRequested" => (
                cancellation_requested(),
                BOOK_ID,
                match fact_ix {
                    1 => OperationKind::Cancel,
                    _ => OperationKind::Book,
                },
            ),
            "CancellingBooking" => (
                cancelling_booking(),
                CANCEL_ID,
                match fact_ix {
                    0 => OperationKind::Book, // BookingExists implies it
                    _ => OperationKind::Cancel,
                },
            ),
            // Settled rows: the intent is the edge that lands here for the
            // fact's direction.
            "AwaitingBooking" => (
                awaiting_booking(),
                match fact_ix {
                    1 => CANCEL_ID,
                    _ => BOOK_ID,
                },
                match fact_ix {
                    1 => OperationKind::Cancel,
                    _ => OperationKind::Book,
                },
            ),
            "Booked" => (
                booked(),
                match fact_ix {
                    0 => BOOK_ID,
                    _ => CANCEL_ID,
                },
                match fact_ix {
                    0 => OperationKind::Book,
                    _ => OperationKind::Cancel,
                },
            ),
            "Cancelled" => (
                match fact_ix {
                    1 => cancelled_after_cancellation(),
                    _ => cancelled_never_booked(),
                },
                match fact_ix {
                    1 => CANCEL_ID,
                    _ => BOOK_ID,
                },
                match fact_ix {
                    1 => OperationKind::Cancel,
                    _ => OperationKind::Book,
                },
            ),
            other => panic!("no fixture for state {other}"),
        };

        let waiting = matches!(fact_category(&booking.state), super::FactCategory::Waiting);
        let (status, reference) = if waiting {
            (EffectStatus::Unknown, None)
        } else if fact_ix <= 1 {
            (EffectStatus::Confirmed, Some(REF))
        } else {
            (EffectStatus::Absent, None)
        };

        let context = FactContext {
            intent: Some(intent(id, kind, status, reference)),
            pending_effect: Some(EffectIntentId::new(FRESH_CANCEL_ID)),
        };
        (booking, fact_of(fact_ix, id), context)
    }

    async fn classify(
        booking: &Booking,
        fact: VerifiedProviderFact,
        context: &FactContext,
    ) -> FactResolution<TransitionPlan<Booking, BookingEffect>, BookingError> {
        TownHallDomain
            .resolve_fact(booking, Verified::assert_verified(fact), context)
            .await
    }

    /// The whole matrix under the fully-bound fixture. Every `Ready` output is
    /// also checked coherent — an incoherent plan could never be committed, so
    /// producing one would make the cell a lie.
    #[tokio::test]
    async fn fact_topology_matches_the_pinned_matrix() {
        let mut checked = 0_usize;
        for (state_name, row) in LOCKED_FACTS {
            for (fact_ix, expected) in row.iter().enumerate() {
                let (booking, fact, context) = bound_cell(state_name, fact_ix);
                let fact_name = fact.name();
                let got = classify(&booking, fact, &context).await;

                // U and C arms are identical bodies by design: one row per
                // outcome pairing keeps the table readable.
                #[allow(clippy::match_same_arms)]
                let ok = match (expected, &got) {
                    (Cell::U, FactResolution::Undefined) => true,
                    (Cell::C, FactResolution::Converged) => true,
                    (Cell::R(next), FactResolution::Ready(plan)) => {
                        plan.next_state().coherent().unwrap_or_else(|why| {
                            panic!(
                                "{state_name} + {fact_name} produced an incoherent booking: {why}"
                            )
                        });
                        plan.next_state().state.name() == *next
                    }
                    (Cell::D(reason), FactResolution::Denied(error)) => {
                        error_name(error) == *reason
                    }
                    _ => false,
                };
                assert!(
                    ok,
                    "{state_name} + {fact_name}: expected {}, got {got:?}",
                    match expected {
                        Cell::U => "Undefined".to_owned(),
                        Cell::C => "Converged".to_owned(),
                        Cell::R(next) => format!("Ready({next})"),
                        Cell::D(reason) => format!("Denied({reason})"),
                    }
                );
                checked += 1;
            }
        }
        assert_eq!(checked, LOCKED_FACTS.len() * FACT_COUNT);
        assert_eq!(checked, 40, "the matrix must cover every cell");
    }

    /// `LOCKED_FACTS` must name every state exactly once, or the sweep silently
    /// shrinks. Guarded the same way the proposal matrix is: through the
    /// compile-checked state index.
    #[test]
    fn every_state_has_exactly_one_matrix_row() {
        let mut seen = [false; 10];
        for (state_name, _) in LOCKED_FACTS {
            let (booking, _, _) = bound_cell(state_name, 0);
            let index = match booking.state {
                BookingState::Draft(_) => 0,
                BookingState::VenueSelected(_) => 1,
                BookingState::NeedsRevalidation(_) => 2,
                BookingState::AwaitingBooking(_) => 3,
                BookingState::BookingInProgress(_) => 4,
                BookingState::CancellationRequested(_) => 5,
                BookingState::Booked(_) => 6,
                BookingState::CancellingBooking(_) => 7,
                BookingState::Cancelled(_) => 8,
                BookingState::NeedsHuman(_) => 9,
            };
            assert!(!seen[index], "duplicate row for {state_name}");
            seen[index] = true;
        }
        assert!(
            seen.iter().all(|hit| *hit),
            "a BookingState variant is missing from LOCKED_FACTS"
        );
    }

    // ------------------------------------------------- the nine Ready cells,
    // asserted as complete bookings
    //
    // The sweep proves the state discriminator; these prove every business
    // field — which is the entire point of B3a's contract, and what revision 1
    // of the plan could not even express.

    fn ready_of(
        got: FactResolution<TransitionPlan<Booking, BookingEffect>, BookingError>,
    ) -> TransitionPlan<Booking, BookingEffect> {
        let FactResolution::Ready(plan) = got else {
            panic!("expected Ready, got {got:?}");
        };
        plan
    }

    /// Booking confirmed: `Booked` carries the council's reference in both
    /// copies, the effect pointer clears, and nothing else moves.
    #[tokio::test]
    async fn a_confirmed_booking_commits_the_reference_and_clears_the_pointer() {
        let (booking, fact, context) = bound_cell("BookingInProgress", 0);
        let plan = ready_of(classify(&booking, fact, &context).await);
        assert_eq!(
            plan,
            TransitionPlan::Local {
                next_state: Booking {
                    state: BookingState::Booked(Booked {
                        booking_ref: CouncilBookingRef::new(REF),
                    }),
                    booking_ref: Some(CouncilBookingRef::new(REF)),
                    active_effect: None,
                    ..booking
                },
            }
        );
    }

    /// The booking never happened: back to `AwaitingBooking`, rebuilt from the
    /// persisted plan — and the reference slot is empty, because a tombstoned
    /// intent has nothing to refer to.
    #[tokio::test]
    async fn an_absent_booking_returns_to_awaiting_from_the_persisted_plan() {
        for fact_ix in [2, 3] {
            let (booking, fact, context) = bound_cell("BookingInProgress", fact_ix);
            let fact_name = fact.name();
            let plan = ready_of(classify(&booking, fact, &context).await);
            assert_eq!(
                plan,
                TransitionPlan::Local {
                    next_state: Booking {
                        state: BookingState::AwaitingBooking(AwaitingBooking {
                            venue_id: VenueId::new("TH-A"),
                            slot_id: SlotId::new("SLOT-A"),
                            verified_fee: Money::from_pence(4_500),
                        }),
                        booking_ref: None,
                        active_effect: None,
                        ..booking
                    },
                },
                "under {fact_name}"
            );
        }
    }

    /// The one fact-driven external effect: the booking we wanted rid of turns
    /// out to exist, so a cancellation is planned against it — carrying the
    /// council reference into both copies, adopting the FRESH identity in both
    /// copies, and shipping a `CancelBooking` bound to the fact's reference.
    #[tokio::test]
    async fn a_found_booking_under_cancellation_plans_the_cancel_effect() {
        let (booking, fact, context) = bound_cell("CancellationRequested", 0);
        let plan = ready_of(classify(&booking, fact, &context).await);
        assert_eq!(
            plan,
            TransitionPlan::ExternalEffect {
                next_state: Booking {
                    state: BookingState::CancellingBooking(CancellingBooking {
                        booking_ref: CouncilBookingRef::new(REF),
                        effect_intent_id: EffectIntentId::new(FRESH_CANCEL_ID),
                    }),
                    booking_ref: Some(CouncilBookingRef::new(REF)),
                    active_effect: Some(EffectIntentId::new(FRESH_CANCEL_ID)),
                    ..booking
                },
                effect: BookingEffect::CancelBooking {
                    booking_ref: CouncilBookingRef::new(REF),
                },
            }
        );
    }

    /// The booking never happened while a cancellation was wanted: `Cancelled`
    /// with nothing to show for it — no reference anywhere, pointer cleared.
    #[tokio::test]
    async fn an_absent_booking_under_cancellation_is_simply_cancelled() {
        for fact_ix in [2, 3] {
            let (booking, fact, context) = bound_cell("CancellationRequested", fact_ix);
            let fact_name = fact.name();
            let plan = ready_of(classify(&booking, fact, &context).await);
            assert_eq!(
                plan,
                TransitionPlan::Local {
                    next_state: Booking {
                        state: BookingState::Cancelled(Cancelled),
                        booking_ref: None,
                        active_effect: None,
                        ..booking
                    },
                },
                "under {fact_name}"
            );
        }
    }

    /// Cancellation confirmed: `Cancelled`, keeping the reference of what was
    /// cancelled — the convergence reading of this very state depends on it.
    #[tokio::test]
    async fn a_confirmed_cancellation_keeps_the_reference_it_cancelled() {
        let (booking, fact, context) = bound_cell("CancellingBooking", 1);
        let plan = ready_of(classify(&booking, fact, &context).await);
        assert_eq!(
            plan,
            TransitionPlan::Local {
                next_state: Booking {
                    state: BookingState::Cancelled(Cancelled),
                    active_effect: None,
                    ..booking
                },
            }
        );
    }

    /// The cancellation never happened: still booked, both reference copies
    /// intact, pointer cleared.
    #[tokio::test]
    async fn an_absent_cancellation_returns_to_booked() {
        for fact_ix in [2, 3] {
            let (booking, fact, context) = bound_cell("CancellingBooking", fact_ix);
            let fact_name = fact.name();
            let plan = ready_of(classify(&booking, fact, &context).await);
            assert_eq!(
                plan,
                TransitionPlan::Local {
                    next_state: Booking {
                        state: BookingState::Booked(Booked {
                            booking_ref: CouncilBookingRef::new(REF),
                        }),
                        booking_ref: Some(CouncilBookingRef::new(REF)),
                        active_effect: None,
                        ..booking
                    },
                },
                "under {fact_name}"
            );
        }
    }

    // ---------------------------------------------- identity and binding

    /// The four identities varied independently: the state's, the aggregate's
    /// `active_effect`, the fact's, and the supplied intent's. Lockstep
    /// fixtures cannot tell which comparison actually fired.
    #[tokio::test]
    async fn a_fact_about_some_other_effect_is_refused() {
        // Fact and intent agree with each other (bind cleanly) but name an
        // effect this state is not waiting on.
        for state_name in ["BookingInProgress", "CancellationRequested"] {
            let (booking, _, _) = bound_cell(state_name, 0);
            let stranger = "EFF-BKG-1001-BOOK-7";
            let context = FactContext {
                intent: Some(intent(
                    stranger,
                    OperationKind::Book,
                    EffectStatus::Unknown,
                    None,
                )),
                pending_effect: Some(EffectIntentId::new(FRESH_CANCEL_ID)),
            };
            let got = classify(&booking, fact_of(0, stranger), &context).await;
            assert_eq!(
                got,
                FactResolution::Denied(BookingError::EffectMismatch),
                "at {state_name}"
            );
        }
        // And at CancellingBooking, via a kind-consistent cancellation fact.
        let (booking, _, _) = bound_cell("CancellingBooking", 1);
        let stranger = "EFF-BKG-1001-CANCEL-7";
        let context = FactContext {
            intent: Some(intent(
                stranger,
                OperationKind::Cancel,
                EffectStatus::Unknown,
                None,
            )),
            pending_effect: Some(EffectIntentId::new(FRESH_CANCEL_ID)),
        };
        let got = classify(&booking, fact_of(1, stranger), &context).await;
        assert_eq!(got, FactResolution::Denied(BookingError::EffectMismatch));
    }

    /// The supplied intent is not the fact's — caught before anything else is
    /// trusted about it.
    #[tokio::test]
    async fn an_intent_that_is_not_the_facts_is_refused() {
        let (booking, fact, _) = bound_cell("BookingInProgress", 0);
        let context = FactContext {
            intent: Some(intent(
                CANCEL_ID, // a real intent, the wrong one
                OperationKind::Book,
                EffectStatus::Unknown,
                None,
            )),
            pending_effect: None,
        };
        let got = classify(&booking, fact, &context).await;
        assert_eq!(got, FactResolution::Denied(BookingError::EffectMismatch));
    }

    /// The intent belongs to a different booking. The comparison target is the
    /// authoritative loaded aggregate's id — which is why `Booking` carries it.
    #[tokio::test]
    async fn an_intent_for_another_booking_is_refused() {
        let (booking, fact, mut context) = bound_cell("BookingInProgress", 0);
        if let Some(intent) = context.intent.as_mut() {
            intent.booking_id = BookingId::new("BKG-9999");
        }
        let got = classify(&booking, fact, &context).await;
        assert_eq!(got, FactResolution::Denied(BookingError::EffectMismatch));
    }

    /// C1 on the fact door: the state's copy and `active_effect` disagreeing is
    /// refused before any meaning is derived. The store cannot produce this
    /// shape, but this door must not assume its caller went through the store.
    #[tokio::test]
    async fn a_self_contradictory_aggregate_is_refused_by_the_fact_door() {
        let (mut booking, fact, context) = bound_cell("BookingInProgress", 0);
        booking.active_effect = Some(EffectIntentId::new("EFF-SOMETHING-ELSE"));
        let got = classify(&booking, fact, &context).await;
        assert_eq!(
            got,
            FactResolution::Denied(BookingError::InconsistentEffectIdentity)
        );
    }

    /// The check the kind-agnostic facts cannot get from B3: an intent of the
    /// WRONG KIND carrying the right identity. "The cancellation never
    /// happened" must never be read as "the booking never happened".
    #[tokio::test]
    async fn an_absence_of_the_wrong_kind_cannot_answer_a_waiting_state() {
        // BookingInProgress waits on a Book; hand it a Cancel intent under the
        // same id, with an EffectAbsent fact (which implies no kind at all).
        let (booking, _, _) = bound_cell("BookingInProgress", 2);
        let context = FactContext {
            intent: Some(intent(
                BOOK_ID,
                OperationKind::Cancel,
                EffectStatus::Unknown,
                None,
            )),
            pending_effect: None,
        };
        let got = classify(&booking, fact_of(2, BOOK_ID), &context).await;
        assert_eq!(
            got,
            FactResolution::Denied(BookingError::EffectKindMismatch),
            "an EffectAbsent must take its kind from the intent, and the intent's kind \
             must match the state's"
        );

        // The mirror: CancellingBooking waits on a Cancel; a Book intent under
        // its id must not answer it.
        let (booking, _, _) = bound_cell("CancellingBooking", 2);
        let context = FactContext {
            intent: Some(intent(
                CANCEL_ID,
                OperationKind::Book,
                EffectStatus::Unknown,
                None,
            )),
            pending_effect: None,
        };
        let got = classify(&booking, fact_of(2, CANCEL_ID), &context).await;
        assert_eq!(
            got,
            FactResolution::Denied(BookingError::EffectKindMismatch)
        );
    }

    /// B6, one defect per fixture: each consequential field of `BookingExists`
    /// flipped in turn, asserting the SPECIFIC field named in the refusal.
    #[tokio::test]
    async fn every_consequential_field_is_bound_against_the_plan() {
        let (booking, _, context) = bound_cell("BookingInProgress", 0);
        let base = |mutate: &dyn Fn(&mut VerifiedProviderFact)| {
            let mut fact = fact_of(0, BOOK_ID);
            mutate(&mut fact);
            fact
        };

        let cases: Vec<(&str, VerifiedProviderFact)> = vec![
            (
                "venue_id",
                base(&|f| {
                    if let VerifiedProviderFact::BookingExists { venue_id, .. } = f {
                        *venue_id = VenueId::new("TH-B");
                    }
                }),
            ),
            (
                "slot_id",
                base(&|f| {
                    if let VerifiedProviderFact::BookingExists { slot_id, .. } = f {
                        *slot_id = SlotId::new("SLOT-B");
                    }
                }),
            ),
            (
                "attendees",
                base(&|f| {
                    if let VerifiedProviderFact::BookingExists { attendees, .. } = f {
                        *attendees = 25;
                    }
                }),
            ),
            (
                "fee",
                base(&|f| {
                    if let VerifiedProviderFact::BookingExists { fee, .. } = f {
                        *fee = Money::from_pence(5_200);
                    }
                }),
            ),
            (
                "principal",
                base(&|f| {
                    if let VerifiedProviderFact::BookingExists { principal, .. } = f {
                        *principal = PrincipalId::new("mallory");
                    }
                }),
            ),
        ];

        for (field, fact) in cases {
            let got = classify(&booking, fact, &context).await;
            assert_eq!(
                got,
                FactResolution::Denied(BookingError::EffectPlanMismatch { field }),
                "a flipped {field} must be refused by name"
            );
        }
    }

    /// The cancellation half of the binding, which revision 1 left untested:
    /// `CancellationExists.booking_ref` against the persisted cancel plan.
    #[tokio::test]
    async fn a_cancellation_for_a_different_reference_is_refused() {
        let (booking, _, context) = bound_cell("CancellingBooking", 1);
        let fact = VerifiedProviderFact::CancellationExists {
            effect_intent_id: EffectIntentId::new(CANCEL_ID),
            booking_ref: CouncilBookingRef::new("TH-00000"),
        };
        let got = classify(&booking, fact, &context).await;
        assert_eq!(
            got,
            FactResolution::Denied(BookingError::EffectPlanMismatch {
                field: "booking_ref"
            })
        );
    }

    /// The other corner of the kind triangle: the intent's COLUMN disagrees
    /// with both the fact and its own plan. At a settled state nothing after
    /// B3 would notice — the convergence table dispatches on the plan, so
    /// without the column check a corrupt row would happily converge.
    #[tokio::test]
    async fn a_corrupt_intent_column_cannot_converge() {
        let (booking, fact, mut context) = bound_cell("Cancelled", 1);
        if let Some(stored) = context.intent.as_mut() {
            // Plan and fact still agree (CancelBooking, TH-92718); only the
            // column lies.
            stored.operation_kind = OperationKind::Book;
        }
        let got = classify(&booking, fact, &context).await;
        assert_eq!(
            got,
            FactResolution::Denied(BookingError::EffectKindMismatch),
            "a column claiming Book must not converge a cancellation"
        );
    }

    /// An intent whose column and plan disagree with each other is a corrupt
    /// row, refused rather than guessed about — and refused as an *incoherent
    /// intent*, not as a plan mismatch. The distinction is worth the words: the
    /// fact is fine, the record it was compared against is not.
    #[tokio::test]
    async fn an_intent_whose_kind_and_plan_disagree_is_refused() {
        let (booking, fact, mut context) = bound_cell("BookingInProgress", 0);
        if let Some(intent) = context.intent.as_mut() {
            intent.canonical_plan = cancel_plan(); // column says Book
        }
        let got = classify(&booking, fact, &context).await;
        assert_eq!(
            got,
            FactResolution::Denied(BookingError::IncoherentIntent(
                IncoherentIntent::KindDisagreesWithPlan {
                    column: "Book",
                    plan: "Cancel",
                }
            ))
        );
    }

    /// Every participating cell with no intent supplied: refused, never
    /// guessed. Swept, not sampled.
    #[tokio::test]
    async fn no_participating_cell_proceeds_without_the_persisted_intent() {
        for (state_name, _) in LOCKED_FACTS {
            for fact_ix in 0..FACT_COUNT {
                let (booking, fact, _) = bound_cell(state_name, fact_ix);
                if matches!(fact_category(&booking.state), super::FactCategory::Absent) {
                    continue;
                }
                let fact_name = fact.name();
                let context = FactContext {
                    intent: None,
                    pending_effect: Some(EffectIntentId::new(FRESH_CANCEL_ID)),
                };
                let got = classify(&booking, fact, &context).await;
                assert_eq!(
                    got,
                    FactResolution::Denied(BookingError::EffectPlanMissing),
                    "{state_name} + {fact_name} must refuse without an intent"
                );
            }
        }
    }

    /// The regression test for revision 2's over-correction: an Absent state
    /// stays `Undefined` even when the context carries a deliberately
    /// mismatched intent. Irrelevant context must not manufacture behaviour.
    #[tokio::test]
    async fn garbage_context_cannot_manufacture_behaviour_in_an_absent_state() {
        for state_name in ["Draft", "VenueSelected", "NeedsRevalidation", "NeedsHuman"] {
            for fact_ix in 0..FACT_COUNT {
                let (booking, fact, _) = bound_cell(state_name, fact_ix);
                let fact_name = fact.name();
                let garbage = FactContext {
                    // Wrong id, wrong booking, invalid shape — all irrelevant.
                    intent: Some({
                        let mut bad = intent(
                            "EFF-WRONG-EVERYTHING",
                            OperationKind::Cancel,
                            EffectStatus::Confirmed,
                            None, // Confirmed with no reference: invalid shape
                        );
                        bad.booking_id = BookingId::new("BKG-9999");
                        bad
                    }),
                    pending_effect: Some(EffectIntentId::new(FRESH_CANCEL_ID)),
                };
                let got = classify(&booking, fact, &garbage).await;
                assert!(
                    got.is_undefined(),
                    "{state_name} + {fact_name} must be Undefined regardless of context, got {got:?}"
                );
            }
        }
    }

    // ------------------------------------- contradiction and convergence

    /// B4's shape table, every row: a malformed status/reference pair is
    /// refused before it can be used as a wildcard. `Confirmed` without its
    /// reference is the dangerous one — it would converge against anything.
    #[tokio::test]
    async fn a_malformed_intent_record_is_never_a_wildcard() {
        let rows: &[(EffectStatus, Option<&str>)] = &[
            (EffectStatus::Confirmed, None),
            (EffectStatus::Prepared, Some(REF)),
            (EffectStatus::Unknown, Some(REF)),
            (EffectStatus::Absent, Some(REF)),
            (EffectStatus::Rejected, Some(REF)),
        ];
        for (status, reference) in rows {
            let (booking, fact, _) = bound_cell("BookingInProgress", 0);
            let context = FactContext {
                intent: Some(intent(BOOK_ID, OperationKind::Book, *status, *reference)),
                pending_effect: None,
            };
            let got = classify(&booking, fact, &context).await;
            assert!(
                matches!(
                    got,
                    FactResolution::Denied(BookingError::IncoherentIntent(
                        IncoherentIntent::OutcomeShape { .. }
                    ))
                ),
                "status {status:?} with reference {reference:?} must be refused as malformed, \
                 got {got:?}"
            );
        }
    }

    /// B5's contradiction table: a fact that contradicts the intent's durable
    /// outcome is refused loudly no matter which state it lands in — checked at
    /// a Waiting state and at a Settled one, because the dangerous arrival
    /// order (ADR-016's race) does not get to choose its landing state.
    #[tokio::test]
    async fn a_fact_contradicting_the_durable_outcome_is_refused_everywhere() {
        // Tombstoned, then "it exists": the catastrophic case.
        for (state_name, fact_ix, id, kind) in [
            ("BookingInProgress", 0, BOOK_ID, OperationKind::Book),
            ("AwaitingBooking", 0, BOOK_ID, OperationKind::Book),
            ("CancellingBooking", 1, CANCEL_ID, OperationKind::Cancel),
            ("Cancelled", 1, CANCEL_ID, OperationKind::Cancel),
        ] {
            let (booking, fact, _) = bound_cell(state_name, fact_ix);
            let fact_name = fact.name();
            let context = FactContext {
                intent: Some(intent(id, kind, EffectStatus::Absent, None)),
                pending_effect: Some(EffectIntentId::new(FRESH_CANCEL_ID)),
            };
            let got = classify(&booking, fact, &context).await;
            assert_eq!(
                got,
                FactResolution::Denied(BookingError::ContradictoryProviderFact),
                "{fact_name} after a tombstone at {state_name}"
            );
        }
        // Rejected, then "it exists".
        {
            let (booking, fact, _) = bound_cell("BookingInProgress", 0);
            let context = FactContext {
                intent: Some(intent(
                    BOOK_ID,
                    OperationKind::Book,
                    EffectStatus::Rejected,
                    None,
                )),
                pending_effect: None,
            };
            let got = classify(&booking, fact, &context).await;
            assert_eq!(
                got,
                FactResolution::Denied(BookingError::ContradictoryProviderFact)
            );
        }
        // Confirmed, then "it never happened".
        for fact_ix in [2, 3] {
            let (booking, fact, _) = bound_cell("BookingInProgress", fact_ix);
            let fact_name = fact.name();
            let context = FactContext {
                intent: Some(intent(
                    BOOK_ID,
                    OperationKind::Book,
                    EffectStatus::Confirmed,
                    Some(REF),
                )),
                pending_effect: None,
            };
            let got = classify(&booking, fact, &context).await;
            assert_eq!(
                got,
                FactResolution::Denied(BookingError::ContradictoryProviderFact),
                "{fact_name} after a confirmation"
            );
        }
    }

    /// One identity resolving to two provider references, in each of the three
    /// places a reference lives: the intent's record, the state's copy, and the
    /// aggregate's copy. Varied independently — a lockstep fixture proves only
    /// whichever comparison runs first.
    #[tokio::test]
    async fn one_identity_two_references_is_duplication_wherever_it_shows() {
        // Intent recorded TH-00000; the fact claims TH-92718. Caught at B5,
        // so it holds at Waiting and Settled states alike.
        {
            let (booking, fact, _) = bound_cell("Booked", 0);
            let context = FactContext {
                intent: Some(intent(
                    BOOK_ID,
                    OperationKind::Book,
                    EffectStatus::Confirmed,
                    Some("TH-00000"),
                )),
                pending_effect: None,
            };
            let got = classify(&booking, fact, &context).await;
            assert_eq!(
                got,
                FactResolution::Denied(BookingError::DuplicateProviderEffect),
                "intent's reference vs fact's"
            );
        }
        // The state says TH-92718; the fact (and its intent) say TH-99999.
        // The plan's fixture for test 13, verbatim.
        {
            let (booking, _, _) = bound_cell("Booked", 0);
            let fact = VerifiedProviderFact::BookingExists {
                effect_intent_id: EffectIntentId::new(BOOK_ID),
                booking_ref: CouncilBookingRef::new("TH-99999"),
                venue_id: VenueId::new("TH-A"),
                slot_id: SlotId::new("SLOT-A"),
                attendees: 20,
                fee: Money::from_pence(4_500),
                principal: PrincipalId::new("lucy"),
            };
            let context = FactContext {
                intent: Some(intent(
                    BOOK_ID,
                    OperationKind::Book,
                    EffectStatus::Confirmed,
                    Some("TH-99999"),
                )),
                pending_effect: None,
            };
            let got = classify(&booking, fact, &context).await;
            assert_eq!(
                got,
                FactResolution::Denied(BookingError::DuplicateProviderEffect),
                "state's reference vs fact's"
            );
        }
        // The aggregate's outer copy disagrees with everything else. The store
        // would never load this shape, and the door's C-step catches it before
        // any fact comparison runs: the aggregate contradicts ITSELF, which is
        // a more honest refusal than blaming the fact for it.
        {
            let (mut booking, fact, context) = bound_cell("Booked", 0);
            booking.booking_ref = Some(CouncilBookingRef::new("TH-99999"));
            let got = classify(&booking, fact, &context).await;
            assert!(
                matches!(
                    got,
                    FactResolution::Denied(BookingError::IncoherentAggregate(
                        IncoherentBooking::CouncilReference { .. }
                    ))
                ),
                "a self-contradictory aggregate must be refused as such, got {got:?}"
            );
        }
    }

    /// Every convergence cell with the intent kind flipped: what would have
    /// converged is a contradiction once the history it claims is the wrong
    /// operation. This is what makes `Converged` mean "already applied" rather
    /// than "close enough".
    #[tokio::test]
    async fn a_flipped_intent_kind_turns_convergence_into_contradiction() {
        // (state, fact_ix, flipped kind, id under the flipped kind)
        let cells: &[(&str, usize, OperationKind, &str)] = &[
            ("AwaitingBooking", 2, OperationKind::Cancel, CANCEL_ID),
            ("AwaitingBooking", 3, OperationKind::Cancel, CANCEL_ID),
            ("Booked", 0, OperationKind::Cancel, CANCEL_ID),
            ("Booked", 2, OperationKind::Book, BOOK_ID),
            ("Booked", 3, OperationKind::Book, BOOK_ID),
            ("Cancelled", 1, OperationKind::Book, BOOK_ID),
            ("Cancelled", 2, OperationKind::Cancel, CANCEL_ID),
            ("Cancelled", 3, OperationKind::Cancel, CANCEL_ID),
        ];
        for (state_name, fact_ix, kind, id) in cells {
            let (booking, _, _) = bound_cell(state_name, *fact_ix);
            let fact = fact_of(*fact_ix, id);
            let fact_name = fact.name();
            // Keep status/reference shape-valid and non-contradictory so the
            // kind flip is the ONLY defect: existence facts pair with
            // Confirmed, absence with Absent.
            let (status, reference) = if fact.asserts_existence() {
                (EffectStatus::Confirmed, Some(REF))
            } else {
                (EffectStatus::Absent, None)
            };
            let context = FactContext {
                intent: Some(intent(id, *kind, status, reference)),
                pending_effect: Some(EffectIntentId::new(FRESH_CANCEL_ID)),
            };
            let got = classify(&booking, fact, &context).await;
            assert!(
                matches!(
                    got,
                    FactResolution::Denied(
                        BookingError::ContradictoryProviderFact | BookingError::EffectKindMismatch
                    )
                ),
                "{state_name} + {fact_name} with a {} intent must not converge, got {got:?}",
                kind.name()
            );
        }
    }

    /// A live intent at a Settled state is inconsistent partial finalisation,
    /// not convergence: the repository commits state and status in one
    /// transaction, so this shape cannot arise honestly. Calling it converged
    /// would launder a broken atomicity guarantee into a repair path.
    #[tokio::test]
    async fn a_live_intent_at_a_settled_state_is_not_convergence() {
        for status in [EffectStatus::Prepared, EffectStatus::Unknown] {
            let (booking, fact, _) = bound_cell("Booked", 0);
            let context = FactContext {
                intent: Some(intent(BOOK_ID, OperationKind::Book, status, None)),
                pending_effect: None,
            };
            let got = classify(&booking, fact, &context).await;
            assert_eq!(
                got,
                FactResolution::Denied(BookingError::ContradictoryProviderFact),
                "a {status:?} intent must not converge at Booked"
            );
        }
    }

    /// The reflection table, one mutated comparison per fixture — where the
    /// fact carries nothing, the STATE is compared against the PLAN, and each
    /// leg of that comparison must be able to fail alone.
    #[tokio::test]
    async fn reflection_compares_real_data_in_every_row() {
        // AwaitingBooking vs the Book plan: venue, slot, fee — each alone.
        {
            let (_, fact, context) = bound_cell("AwaitingBooking", 2);
            // The booking is internally coherent — state and selection both
            // say TH-B — so the PLAN is the only thing that disagrees, which
            // is exactly the comparison this row exists to make.
            let mismatched = Booking {
                state: BookingState::AwaitingBooking(AwaitingBooking {
                    venue_id: VenueId::new("TH-B"), // plan says TH-A
                    slot_id: SlotId::new("SLOT-A"),
                    verified_fee: Money::from_pence(4_500),
                }),
                selected_venue: Some(SelectedVenueRef {
                    venue_id: VenueId::new("TH-B"),
                    slot_id: SlotId::new("SLOT-A"),
                }),
                ..awaiting_booking()
            };
            mismatched
                .coherent()
                .expect("one defect per fixture: the plan is the mismatch, not the booking");
            let got = classify(&mismatched, fact, &context).await;
            assert_eq!(
                got,
                FactResolution::Denied(BookingError::ContradictoryProviderFact),
                "an absent Book plan for TH-A cannot converge at an AwaitingBooking for TH-B"
            );
        }
        {
            let (_, fact, context) = bound_cell("AwaitingBooking", 2);
            let mismatched = booking_at(
                BookingState::AwaitingBooking(AwaitingBooking {
                    venue_id: VenueId::new("TH-A"),
                    slot_id: SlotId::new("SLOT-A"),
                    verified_fee: Money::from_pence(9_999), // plan says 4500
                }),
                None,
                None,
            );
            let got = classify(&mismatched, fact, &context).await;
            assert_eq!(
                got,
                FactResolution::Denied(BookingError::ContradictoryProviderFact)
            );
        }
        // Booked vs an absent CANCEL plan naming a different booking: the
        // review's own example — EffectAbsent carries no reference, so only
        // the state-vs-plan comparison can catch it.
        {
            let (booking, fact, mut context) = bound_cell("Booked", 2);
            if let Some(stored) = context.intent.as_mut() {
                stored.canonical_plan = BookingEffect::CancelBooking {
                    booking_ref: CouncilBookingRef::new("TH-00000"),
                };
            }
            let got = classify(&booking, fact, &context).await;
            assert_eq!(
                got,
                FactResolution::Denied(BookingError::ContradictoryProviderFact),
                "an absent cancellation of TH-00000 says nothing about a booking of TH-92718"
            );
        }
        // Cancelled + CancellationExists, aggregate reference disagreeing.
        {
            let (_, fact, context) = bound_cell("Cancelled", 1);
            let mismatched = booking_at(BookingState::Cancelled(Cancelled), Some("TH-00000"), None);
            let got = classify(&mismatched, fact, &context).await;
            assert_eq!(
                got,
                FactResolution::Denied(BookingError::DuplicateProviderEffect),
                "the fact's reference disagrees with what this booking cancelled"
            );
        }
        // Cancelled + absent Book intent, but the aggregate CLAIMS a reference:
        // the booking supposedly never happened, yet something referenced it.
        {
            let (_, fact, context) = bound_cell("Cancelled", 2);
            let contradictory = booking_at(BookingState::Cancelled(Cancelled), Some(REF), None);
            let got = classify(&contradictory, fact, &context).await;
            assert_eq!(
                got,
                FactResolution::Denied(BookingError::ContradictoryProviderFact),
                "a tombstoned booking cannot have left a reference behind"
            );
        }
    }

    /// The review's finding, pinned: a phantom reference — a state whose
    /// booking has never been confirmed carrying a `booking_ref` — must be
    /// refused, never silently cleared or overwritten by a transition. A bad
    /// write laundered into a clean state is how the next reader never learns
    /// anything was wrong.
    #[tokio::test]
    async fn a_phantom_reference_is_refused_not_laundered() {
        // Each of these transitions would have cleared or overwritten the
        // phantom: absence clears it, confirmation overwrites it, the found
        // booking under cancellation overwrites it, and the AwaitingBooking
        // convergence would have blessed it.
        let cells: &[(&str, usize)] = &[
            ("BookingInProgress", 2),     // would clear via EffectAbsent
            ("BookingInProgress", 0),     // would overwrite via BookingExists
            ("CancellationRequested", 2), // would clear via EffectAbsent
            ("CancellationRequested", 0), // would overwrite via BookingExists
            ("AwaitingBooking", 2),       // would converge over it
        ];
        for (state_name, fact_ix) in cells {
            let (mut booking, fact, context) = bound_cell(state_name, *fact_ix);
            let fact_name = fact.name();
            booking.booking_ref = Some(CouncilBookingRef::new("TH-PHANTOM"));
            let got = classify(&booking, fact, &context).await;
            assert!(
                matches!(
                    got,
                    FactResolution::Denied(BookingError::IncoherentAggregate(
                        IncoherentBooking::PhantomReference { .. }
                    ))
                ),
                "{state_name} + {fact_name} with a phantom reference must refuse, got {got:?}"
            );
        }
    }

    /// The ‡ cell's other reading: the OLD booking intent's confirmation
    /// re-arriving at `CancellingBooking` — which is how this state was reached —
    /// is convergence, not an answer to the cancellation in flight.
    #[tokio::test]
    async fn the_old_bookings_confirmation_converges_at_cancelling_booking() {
        let (booking, _, _) = bound_cell("CancellingBooking", 0);
        let fact = fact_of(0, BOOK_ID); // the BOOK intent's id, not the cancel's
        let context = FactContext {
            intent: Some(intent(
                BOOK_ID,
                OperationKind::Book,
                EffectStatus::Confirmed,
                Some(REF),
            )),
            pending_effect: None,
        };
        let got = classify(&booking, fact, &context).await;
        assert!(
            got.is_converged(),
            "the fact that created this state must read as already applied, got {got:?}"
        );
    }

    /// The commonest transition in the system, pinned so no future
    /// contradiction or convergence machinery can break it: the fact arrives
    /// while the intent still says Unknown, because nothing has recorded the
    /// outcome yet. That is the ordinary happy path, not an edge case.
    #[tokio::test]
    async fn the_ordinary_happy_path_never_touches_the_contradiction_machinery() {
        let (booking, fact, _) = bound_cell("BookingInProgress", 0);
        let context = FactContext {
            intent: Some(intent(
                BOOK_ID,
                OperationKind::Book,
                EffectStatus::Unknown,
                None,
            )),
            pending_effect: None,
        };
        let got = classify(&booking, fact, &context).await;
        let FactResolution::Ready(plan) = got else {
            panic!("the happy path must be Ready, got {got:?}");
        };
        assert_eq!(plan.next_state().state.name(), "Booked");
    }

    // ------------------------------------------- the external-effect cell

    /// The fact-driven cancellation needs a fresh identity from the
    /// coordinator: absent is refused, and the old identity reused is refused —
    /// a plan whose old and new effects share one identity is structurally
    /// invalid, and the boundary must not emit it even though the store would
    /// catch it later.
    #[tokio::test]
    async fn the_fact_driven_cancellation_demands_a_fresh_identity() {
        let (booking, fact, context) = bound_cell("CancellationRequested", 0);
        let no_identity = FactContext {
            pending_effect: None,
            ..context.clone()
        };
        let got = classify(&booking, fact.clone(), &no_identity).await;
        assert_eq!(
            got,
            FactResolution::Denied(BookingError::EffectIdentityMissing)
        );

        let reused = FactContext {
            pending_effect: Some(EffectIntentId::new(BOOK_ID)),
            ..context
        };
        let got = classify(&booking, fact, &reused).await;
        assert_eq!(got, FactResolution::Denied(BookingError::EffectMismatch));
    }

    // ---------------------------------------------- cross-door and structure

    /// ADR-012's central claim, asserted rather than argued: no proposal in any
    /// state reaches `Booked` or `NeedsHuman` — the model cannot announce its
    /// own success — and no provider fact reaches `NeedsHuman`, because the
    /// council cannot conclude our retry budget is exhausted.
    #[tokio::test]
    async fn no_door_reaches_a_state_that_is_not_its_to_reach() {
        let authority = VerifiedAuthority {
            principal: PrincipalId::new("lucy"),
            actor: ActorId::new("townhall-agent"),
            max_fee: Money::from_pence(5_000),
            may_book: true,
            may_cancel: true,
        };
        let proposal_context = BookingContext {
            selected_facts: Some(good_facts()),
            pending_effect: Some(EffectIntentId::new(BOOK_ID)),
        };
        let proposals = || {
            vec![
                BookingProposal::SelectVenue {
                    venue_id: VenueId::new("TH-A"),
                    slot_id: SlotId::new("SLOT-A"),
                },
                BookingProposal::VerifySlot,
                BookingProposal::ChangeVenue,
                BookingProposal::UpdateRequirements {
                    attendees: Some(25),
                },
                BookingProposal::RevalidateVenue,
                BookingProposal::Book,
                BookingProposal::Cancel {
                    reason: "changed mind".to_owned(),
                },
            ]
        };

        for (state_name, _) in LOCKED_FACTS {
            let (booking, _, _) = bound_cell(state_name, 0);
            for proposal in proposals() {
                let name = proposal.name();
                if let Resolution::Ready(plan) = TownHallDomain
                    .resolve_proposal(&booking, proposal, &authority, &proposal_context)
                    .await
                {
                    let next = plan.next_state().state.name();
                    assert!(
                        next != "Booked" && next != "NeedsHuman",
                        "{state_name} + proposal {name} reached {next}: a proposer announced \
                         an outcome only evidence or the runtime may conclude"
                    );
                }
            }

            for fact_ix in 0..FACT_COUNT {
                let (booking, fact, context) = bound_cell(state_name, fact_ix);
                let fact_name = fact.name();
                if let FactResolution::Ready(plan) = classify(&booking, fact, &context).await {
                    assert_ne!(
                        plan.next_state().state.name(),
                        "NeedsHuman",
                        "{state_name} + fact {fact_name} reached NeedsHuman: a provider \
                         concluded our own retry budget"
                    );
                }
            }
        }
    }

    /// Both new doors classify; they never mutate, and asking twice gives the
    /// same answer — which is what lets a coordinator reload and re-classify
    /// after losing a compare-and-set.
    #[tokio::test]
    async fn both_doors_are_pure_and_repeatable() {
        let (booking, fact, context) = bound_cell("BookingInProgress", 0);
        let before = booking.clone();
        let first = classify(&booking, fact.clone(), &context).await;
        let second = classify(&booking, fact, &context).await;
        assert_eq!(first, second);
        assert_eq!(booking, before, "the caller's booking is untouched");

        let event = || SystemEvent::ReconciliationExhausted {
            effect_intent_id: EffectIntentId::new(BOOK_ID),
        };
        let first = TownHallDomain.resolve_system_event(&booking, event()).await;
        let second = TownHallDomain.resolve_system_event(&booking, event()).await;
        assert_eq!(first, second);
        assert_eq!(booking, before);
    }
}

/// The state × system-event topology — 10 states × 1 event.
///
/// `NeedsHuman` is reachable only through this door: neither a proposer nor a
/// provider fact can conclude that our own retry budget is exhausted.
#[cfg(test)]
mod system_event_topology {
    use super::*;
    use bld_types::{BookingRequirements, Money, TimeWindow};

    const BOOK_ID: &str = "EFF-BKG-1001-BOOK-2";
    const CANCEL_ID: &str = "EFF-BKG-1001-CANCEL-5";
    const REF: &str = "TH-92718";

    fn booking_of(state: BookingState, booking_ref: Option<&str>, active: Option<&str>) -> Booking {
        let booking = Booking {
            id: BookingId::new("BKG-1001"),
            state,
            requirements: BookingRequirements {
                purpose: "meeting".to_owned(),
                requested_date: "2026-08-20".to_owned(),
                time_window: TimeWindow {
                    from: "13:00".to_owned(),
                    to: "17:00".to_owned(),
                },
                attendees: 20,
                wheelchair_accessible: true,
                max_fee: Money::from_pence(5_000),
            },
            selected_venue: Some(SelectedVenueRef {
                venue_id: VenueId::new("TH-A"),
                slot_id: SlotId::new("SLOT-A"),
            }),
            availability: None,
            booking_ref: booking_ref.map(CouncilBookingRef::new),
            active_effect: active.map(EffectIntentId::new),
        };
        booking.coherent().expect("fixtures must be coherent");
        booking
    }

    /// Every state, with the effect id its in-flight variant carries (if any).
    /// Exhaustive by construction: a new `BookingState` variant fails to
    /// compile here.
    fn all_states() -> Vec<Booking> {
        [
            booking_of(BookingState::Draft(Draft), None, None),
            booking_of(
                BookingState::VenueSelected(VenueSelected {
                    venue_id: VenueId::new("TH-A"),
                    slot_id: SlotId::new("SLOT-A"),
                }),
                None,
                None,
            ),
            booking_of(
                BookingState::NeedsRevalidation(NeedsRevalidation {
                    selected: Some(SelectedVenueRef {
                        venue_id: VenueId::new("TH-A"),
                        slot_id: SlotId::new("SLOT-A"),
                    }),
                }),
                None,
                None,
            ),
            booking_of(
                BookingState::AwaitingBooking(AwaitingBooking {
                    venue_id: VenueId::new("TH-A"),
                    slot_id: SlotId::new("SLOT-A"),
                    verified_fee: Money::from_pence(4_500),
                }),
                None,
                None,
            ),
            booking_of(
                BookingState::BookingInProgress(BookingInProgress {
                    effect_intent_id: EffectIntentId::new(BOOK_ID),
                }),
                None,
                Some(BOOK_ID),
            ),
            booking_of(
                BookingState::CancellationRequested(CancellationRequested {
                    effect_intent_id: EffectIntentId::new(BOOK_ID),
                }),
                None,
                Some(BOOK_ID),
            ),
            booking_of(
                BookingState::Booked(Booked {
                    booking_ref: CouncilBookingRef::new(REF),
                }),
                Some(REF),
                None,
            ),
            booking_of(
                BookingState::CancellingBooking(CancellingBooking {
                    booking_ref: CouncilBookingRef::new(REF),
                    effect_intent_id: EffectIntentId::new(CANCEL_ID),
                }),
                Some(REF),
                Some(CANCEL_ID),
            ),
            booking_of(BookingState::Cancelled(Cancelled), None, None),
            booking_of(BookingState::NeedsHuman(NeedsHuman), None, None),
        ]
        .into_iter()
        .collect()
    }

    fn exhausted(id: &str) -> SystemEvent {
        SystemEvent::ReconciliationExhausted {
            effect_intent_id: EffectIntentId::new(id),
        }
    }

    /// The whole matrix: exactly the three in-flight states move to
    /// `NeedsHuman`; everywhere else the behaviour does not exist.
    #[tokio::test]
    async fn exhaustion_moves_exactly_the_in_flight_states() {
        let mut checked = 0_usize;
        for booking in all_states() {
            let state_name = booking.state.name();
            let in_flight = booking.state.effect_intent_id().cloned();
            let event = exhausted(in_flight.as_ref().map_or(BOOK_ID, EffectIntentId::as_str));
            let got = TownHallDomain.resolve_system_event(&booking, event).await;

            if in_flight.is_some() {
                let Resolution::Ready(plan) = &got else {
                    panic!(
                        "{state_name} has an effect in flight and must reach NeedsHuman, got {got:?}"
                    );
                };
                let next = plan.next_state();
                next.coherent()
                    .unwrap_or_else(|why| panic!("{state_name} produced incoherence: {why}"));
                // The complete booking, not the discriminator: giving up
                // clears the pointer — no automation acts on this effect again
                // — and touches nothing else.
                assert_eq!(
                    plan,
                    &TransitionPlan::Local {
                        next_state: Booking {
                            state: BookingState::NeedsHuman(NeedsHuman),
                            active_effect: None,
                            ..booking.clone()
                        },
                    },
                    "at {state_name}"
                );
            } else {
                assert!(
                    matches!(got, Resolution::Undefined),
                    "{state_name} has no retry budget to exhaust; expected Undefined, got {got:?}"
                );
            }
            checked += 1;
        }
        assert_eq!(checked, 10, "the sweep must cover every state");
    }

    /// Exhaustion of some OTHER effect says nothing about this state: refused
    /// with a reason, never a silent gap — and never a giving-up on the wrong
    /// effect's behalf.
    #[tokio::test]
    async fn exhaustion_of_a_different_effect_is_refused() {
        for booking in all_states() {
            if booking.state.effect_intent_id().is_none() {
                continue;
            }
            let got = TownHallDomain
                .resolve_system_event(&booking, exhausted("EFF-SOMEBODY-ELSE"))
                .await;
            assert_eq!(
                got,
                Resolution::Denied(BookingError::EffectMismatch),
                "at {}",
                booking.state.name()
            );
        }
    }

    /// C1 holds on this door too: a state whose two effect pointers disagree
    /// is refused before the event is interpreted.
    #[tokio::test]
    async fn a_self_contradictory_aggregate_is_refused_by_the_event_door() {
        let mut booking = booking_of(
            BookingState::BookingInProgress(BookingInProgress {
                effect_intent_id: EffectIntentId::new(BOOK_ID),
            }),
            None,
            Some(BOOK_ID),
        );
        booking.active_effect = Some(EffectIntentId::new("EFF-SOMETHING-ELSE"));
        let got = TownHallDomain
            .resolve_system_event(&booking, exhausted(BOOK_ID))
            .await;
        assert_eq!(
            got,
            Resolution::Denied(BookingError::InconsistentEffectIdentity)
        );
    }
}
