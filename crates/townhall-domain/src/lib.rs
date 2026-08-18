#![forbid(unsafe_code)]

use async_trait::async_trait;
use bld_kernel::{BoundaryDomain, Resolution, TransitionPlan};
use bld_types::{
    ActorId, BookingId, BookingRequirements, CouncilBookingRef, EffectIntentId, Money, PrincipalId,
    SlotId, VenueId,
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

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct CancellationRequested;

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
    #[must_use]
    pub const fn in_flight_kind(&self) -> Option<OperationKind> {
        match self {
            Self::BookingInProgress(_) => Some(OperationKind::Book),
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
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
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
        facts: VenueFacts,
    },
    /// Cancel a council booking that is known to exist.
    CancelBooking { booking_ref: CouncilBookingRef },
}

impl BookingEffect {
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
}

#[derive(Clone, Copy, Debug, Default)]
pub struct TownHallDomain;

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
    type Error = BookingError;

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
    async fn resolve_proposal(
        &self,
        booking: &Self::State,
        proposal: Self::Proposal,
        authority: &Self::Authority,
        context: &Self::Context,
    ) -> Resolution<TransitionPlan<Self::State, Self::Effect>, Self::Error> {
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
            BookingState::CancellationRequested(CancellationRequested),
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
        Booking {
            id: BookingId::new("BKG-1001"),
            state,
            requirements,
            selected_venue: Some(selection()),
            availability: None,
            booking_ref: None,
            active_effect: None,
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
