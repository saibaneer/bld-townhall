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

#[derive(Clone, Debug)]
pub struct BookingContext {
    pub booking_id: BookingId,
    pub requirements: BookingRequirements,
    /// Facts loaded by a capability. Never authoritative on their own: every
    /// behaviour that consumes them must first bind them to what the user
    /// actually chose, which lives in the *state*, not here.
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
fn local(
    next_state: BookingState,
) -> Resolution<TransitionPlan<BookingState, BookingEffect>, BookingError> {
    Resolution::Ready(TransitionPlan::Local { next_state })
}

impl TownHallDomain {
    /// Load the context's facts and bind them to the venue the user actually
    /// chose, then check them against requirements and authority.
    ///
    /// The binding is the point. Loaded facts are never authoritative on their
    /// own — every per-venue guard passes for a venue the user never selected,
    /// so only comparing against the selection catches a substitution.
    fn bind_facts<'a>(
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
        Self::validate_facts(facts, &context.requirements, authority)?;
        Ok(facts)
    }

    /// `Book` no longer books. It commits the intent to book.
    ///
    /// The transition stops at `BookingInProgress`, which is committed *before*
    /// the council is called (ADR-014). Previously this faked a synchronous
    /// confirmation and jumped straight to `Booked` — fine against an in-process
    /// fake, and the reason a lost response could leave no record that an
    /// external consequence might exist.
    fn resolve_book(
        waiting: &AwaitingBooking,
        authority: &VerifiedAuthority,
        context: &BookingContext,
    ) -> Resolution<TransitionPlan<BookingState, BookingEffect>, BookingError> {
        if !authority.may_book {
            return Resolution::Denied(BookingError::BookingAuthorityRequired);
        }
        let facts = match Self::bind_facts(context, &waiting.venue_id, &waiting.slot_id, authority)
        {
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
            next_state: BookingState::BookingInProgress(BookingInProgress { effect_intent_id }),
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
        booked: &Booked,
        authority: &VerifiedAuthority,
        context: &BookingContext,
    ) -> Resolution<TransitionPlan<BookingState, BookingEffect>, BookingError> {
        if !authority.may_cancel {
            return Resolution::Denied(BookingError::CancellationAuthorityRequired);
        }
        let Some(effect_intent_id) = context.pending_effect.clone() else {
            return Resolution::Denied(BookingError::EffectIdentityMissing);
        };

        Resolution::Ready(TransitionPlan::ExternalEffect {
            next_state: BookingState::CancellingBooking(CancellingBooking {
                booking_ref: booked.booking_ref.clone(),
                effect_intent_id,
            }),
            effect: BookingEffect::CancelBooking {
                booking_ref: booked.booking_ref.clone(),
            },
        })
    }
}

#[async_trait]
impl BoundaryDomain for TownHallDomain {
    type State = BookingState;
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
        state: &Self::State,
        proposal: Self::Proposal,
        authority: &Self::Authority,
        context: &Self::Context,
    ) -> Resolution<TransitionPlan<Self::State, Self::Effect>, Self::Error> {
        match (state, proposal) {
            (BookingState::Draft(_), BookingProposal::SelectVenue { venue_id, slot_id }) => {
                local(BookingState::VenueSelected(VenueSelected {
                    venue_id,
                    slot_id,
                }))
            }
            (BookingState::Draft(_), BookingProposal::Cancel { .. }) => {
                local(BookingState::Cancelled(Cancelled))
            }
            (BookingState::VenueSelected(selected), BookingProposal::VerifySlot) => {
                match Self::bind_facts(context, &selected.venue_id, &selected.slot_id, authority) {
                    Ok(facts) => local(BookingState::AwaitingBooking(AwaitingBooking {
                        venue_id: facts.venue_id.clone(),
                        slot_id: facts.slot_id.clone(),
                        verified_fee: facts.fee,
                    })),
                    Err(error) => Resolution::Denied(error),
                }
            }
            (BookingState::VenueSelected(_), BookingProposal::ChangeVenue) => {
                local(BookingState::Draft(Draft))
            }
            (BookingState::VenueSelected(selected), BookingProposal::UpdateRequirements { .. }) => {
                local(BookingState::NeedsRevalidation(NeedsRevalidation {
                    selected: Some(SelectedVenueRef {
                        venue_id: selected.venue_id.clone(),
                        slot_id: selected.slot_id.clone(),
                    }),
                }))
            }
            (BookingState::VenueSelected(_), BookingProposal::Cancel { .. }) => {
                local(BookingState::Cancelled(Cancelled))
            }
            (BookingState::NeedsRevalidation(pending), BookingProposal::RevalidateVenue) => {
                // The binding target is state data, not context, so this holds
                // without trusting whoever assembled the context. Without it, an
                // ordinary `UpdateRequirements` is enough to launder any venue
                // into the booking.
                let Some(selected) = pending.selected.as_ref() else {
                    return Resolution::Denied(BookingError::VenueFactsMissing);
                };
                match Self::bind_facts(context, &selected.venue_id, &selected.slot_id, authority) {
                    Ok(facts) => local(BookingState::VenueSelected(VenueSelected {
                        venue_id: facts.venue_id.clone(),
                        slot_id: facts.slot_id.clone(),
                    })),
                    Err(error) => Resolution::Denied(error),
                }
            }
            (BookingState::NeedsRevalidation(_), BookingProposal::ChangeVenue) => {
                local(BookingState::Draft(Draft))
            }
            (BookingState::NeedsRevalidation(_), BookingProposal::Cancel { .. }) => {
                local(BookingState::Cancelled(Cancelled))
            }
            (BookingState::AwaitingBooking(waiting), BookingProposal::Book) => {
                Self::resolve_book(waiting, authority, context)
            }
            (BookingState::AwaitingBooking(_), BookingProposal::ChangeVenue) => {
                local(BookingState::Draft(Draft))
            }
            (
                BookingState::AwaitingBooking(waiting),
                BookingProposal::UpdateRequirements { .. },
            ) => local(BookingState::NeedsRevalidation(NeedsRevalidation {
                selected: Some(SelectedVenueRef {
                    venue_id: waiting.venue_id.clone(),
                    slot_id: waiting.slot_id.clone(),
                }),
            })),
            (BookingState::AwaitingBooking(_), BookingProposal::Cancel { .. }) => {
                local(BookingState::Cancelled(Cancelled))
            }
            (BookingState::Booked(booked), BookingProposal::Cancel { .. }) => {
                Self::resolve_cancel_booked(booked, authority, context)
            }
            _ => Resolution::Undefined,
        }
    }
}

/// The state × proposal topology, pinned.
///
/// Spec §7 draws arrows in two vocabularies, and only one of them is a
/// `BookingProposal`. These are: `SelectVenue`, `VerifySlot`, `ChangeVenue`,
/// `UpdateRequirements`, `RevalidateVenue`, `Book`, `Cancel`, `Reconcile`.
/// These are **not** — they are evidence or read outcomes, and no agent can
/// submit them: `booking_confirmed`, `booking_failed`, `no_booking_found`,
/// `booking_found`, `reconciliation_failed`, `cancellation_confirmed`,
/// `cancellation_failed`, `view_booking`.
///
/// Counting proposal arrows only, the spec defines 15 cells and this code
/// implements 14. The single difference is recorded in [`PENDING`].
///
/// `Reconcile` is listed in §7.1 but drawn on **no arrow anywhere**, so
/// returning `Undefined` for it on every state matches the spec literally.
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

    fn permissive_context() -> BookingContext {
        BookingContext {
            booking_id: BookingId::new("BKG-1001"),
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

    async fn sweep(label: &str, authority: &VerifiedAuthority, context: &BookingContext) {
        let domain = TownHallDomain;
        let mut checked = 0_usize;

        for state in all_states() {
            for proposal in all_proposals() {
                let state_name = state.name();
                let proposal_name = proposal.name();
                let want_defined = expected_defined(state_name, proposal_name);

                let got = domain
                    .resolve_proposal(&state, proposal.clone(), authority, context)
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
        sweep("permissive", &permissive_authority(), &permissive_context()).await;
    }

    /// The same matrix under a fixture where every guard fails. A behaviour
    /// that exists must still exist — it just gets `Denied` instead of `Ready`.
    ///
    /// This is what catches the guide's Mistake 13: collapsing `Undefined` into
    /// `Denied` would light up all 66 impossible cells at once.
    #[tokio::test]
    async fn topology_does_not_depend_on_authority_or_context() {
        sweep("hostile", &hostile_authority(), &hostile_context()).await;
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
                let got = domain
                    .resolve_proposal(&state, proposal.clone(), &authority, &context)
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

/// Characterization of the domain's behaviour **before** the M4 kernel change.
///
/// Slice B rewrites the most load-bearing types in the repo: `Kernel::apply`
/// stops owning `&mut State`, `execute` and `validate` leave `BoundaryDomain`
/// for `Capability` and `Verifier`, and `Reconcile` leaves the proposal
/// vocabulary. Behaviour-preserving refactors of that size are exactly where
/// drift is silent, so this module pins what the domain does today.
///
/// # Why these run a whole turn rather than calling `resolve`
///
/// Pinning `resolve` alone would capture only the intermediate `BookingPlan`.
/// It would say nothing about what `execute` and `validate` contribute — the
/// next state and its fields, evidence binding, and the rule that nothing
/// commits unless validation succeeds. Those are precisely what slice B moves,
/// so every test here drives `Kernel::apply` end to end and asserts the exact
/// `BoundaryOutcome`.
///
/// # One defect per fixture
///
/// Denial tests use a fixture with exactly **one** thing wrong. A fixture with
/// two defects would pin whichever guard the code happens to check first, and
/// swapping two safety-neutral checks would then turn the test red for no
/// reason. The expected error has to be forced by meaning.
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
            booking_id: BookingId::new("BKG-1001"),
            requirements: requirements(),
            selected_facts: Some(good_facts()),
            pending_effect: Some(EffectIntentId::new("EFF-BKG-1001-BOOK-0")),
        }
    }

    fn venue_selected() -> BookingState {
        BookingState::VenueSelected(VenueSelected {
            venue_id: VenueId::new("TH-A"),
            slot_id: SlotId::new("SLOT-A"),
        })
    }

    fn needs_revalidation() -> BookingState {
        BookingState::NeedsRevalidation(NeedsRevalidation {
            selected: Some(SelectedVenueRef {
                venue_id: VenueId::new("TH-A"),
                slot_id: SlotId::new("SLOT-A"),
            }),
        })
    }

    fn awaiting_booking() -> BookingState {
        BookingState::AwaitingBooking(AwaitingBooking {
            venue_id: VenueId::new("TH-A"),
            slot_id: SlotId::new("SLOT-A"),
            verified_fee: Money::from_pence(4_500),
        })
    }

    fn booked() -> BookingState {
        BookingState::Booked(Booked {
            booking_ref: CouncilBookingRef::new("TH-92718"),
        })
    }

    /// Classify one proposal and return the resolution.
    ///
    /// B2 changed what a turn *is*: the kernel classifies and the coordinator
    /// commits, so there is no longer a single call that both decides and
    /// mutates. What these tests pin is unchanged — the exact next state for
    /// every legal cell, and the exact error for every denial. Only the
    /// wrapper moved from `BoundaryOutcome::Committed` to
    /// `Resolution::Ready(TransitionPlan::…)`.
    async fn turn(
        state: BookingState,
        proposal: BookingProposal,
        authority: &VerifiedAuthority,
        context: &BookingContext,
    ) -> Resolution<TransitionPlan<BookingState, BookingEffect>, BookingError> {
        TownHallDomain
            .resolve_proposal(&state, proposal, authority, context)
            .await
    }

    /// A local transition to `next`, which is what most cells produce.
    fn committed_local(
        next: BookingState,
    ) -> Resolution<TransitionPlan<BookingState, BookingEffect>, BookingError> {
        Resolution::Ready(TransitionPlan::Local { next_state: next })
    }

    // ------------------------------------------------ preserved local cells
    //
    // These twelve must produce byte-identical outcomes after slice B. They are
    // the regression surface for the refactor.

    #[tokio::test]
    async fn draft_select_venue() {
        let got = turn(
            BookingState::Draft(Draft),
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
            BookingState::Draft(Draft),
            BookingProposal::Cancel {
                reason: "changed mind".to_owned(),
            },
            &authority(),
            &context(),
        )
        .await;
        assert_eq!(got, committed_local(BookingState::Cancelled(Cancelled)));
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
        assert_eq!(got, committed_local(BookingState::Draft(Draft)));
    }

    /// The selection must be carried forward — this is the field that closed
    /// the venue-substitution bug, so the refactor must not drop it.
    #[tokio::test]
    async fn venue_selected_update_requirements_carries_the_selection() {
        let got = turn(
            venue_selected(),
            BookingProposal::UpdateRequirements {
                attendees: Some(25),
            },
            &authority(),
            &context(),
        )
        .await;
        assert_eq!(got, committed_local(needs_revalidation()));
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
        assert_eq!(got, committed_local(BookingState::Cancelled(Cancelled)));
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
        assert_eq!(got, committed_local(venue_selected()));
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
        assert_eq!(got, committed_local(BookingState::Draft(Draft)));
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
        assert_eq!(got, committed_local(BookingState::Cancelled(Cancelled)));
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
        assert_eq!(got, committed_local(BookingState::Draft(Draft)));
    }

    #[tokio::test]
    async fn awaiting_booking_update_requirements_carries_the_selection() {
        let got = turn(
            awaiting_booking(),
            BookingProposal::UpdateRequirements {
                attendees: Some(25),
            },
            &authority(),
            &context(),
        )
        .await;
        assert_eq!(got, committed_local(needs_revalidation()));
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
        let state = BookingState::NeedsRevalidation(NeedsRevalidation { selected: None });
        let got = turn(
            state,
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

    // --------------------------------------- cells slice B changes on purpose
    //
    // These two are the reason "tests unchanged" cannot be B's gate. They are
    // pinned twice: what they do today, and what they must do after B2. The
    // post-M4 assertions are `#[ignore]`d because they describe behaviour that
    // does not exist yet — B2 removes the attribute, and a green run there is
    // the evidence the change landed as designed rather than as it happened to
    // come out.

    // `book_today_jumps_straight_to_booked` lived here and was the tripwire for
    // the B2 expectation above. B2 landed, it failed as designed, and both have
    // been resolved: the expectation is now an ordinary passing test.

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
        let BookingState::BookingInProgress(in_progress) = plan.next_state() else {
            panic!(
                "Book must stop at BookingInProgress, got {:?}",
                plan.next_state()
            );
        };
        // And it must be an ExternalEffect, not a Local one — that distinction is
        // what forces the intent to be persisted before the council is called.
        assert!(
            matches!(plan, TransitionPlan::ExternalEffect { .. }),
            "Book must be an ExternalEffect so its intent is persisted first"
        );

        // Matching the variant is not enough: B2 could produce the right state
        // with the wrong effect identity and this would still pass. The id must
        // be present, and it must be *deterministic* — the same operation
        // proposed twice must derive the same identity, because that is what
        // makes a retry idempotent rather than a second booking (ADR-014).
        assert!(
            !in_progress.effect_intent_id.as_str().is_empty(),
            "BookingInProgress must carry an effect identity"
        );
        let again = turn(
            awaiting_booking(),
            BookingProposal::Book,
            &authority(),
            &context(),
        )
        .await;
        assert_eq!(
            got, again,
            "the same operation must derive the same effect identity"
        );
    }

    // `booked_cancel_today_jumps_straight_to_cancelled` lived here, same story.

    /// `Booked + Cancel` stops at `CancellingBooking`. This is the *ordinary*
    /// cancellation path, not the in-flight one — had it stayed local, an
    /// ordinary cancel would commit `Cancelled` while the council booking
    /// stayed live for every slice between the coordinator landing and F.
    #[tokio::test]
    async fn booked_cancel_stops_at_cancelling_booking_with_an_effect() {
        let got = turn(
            booked(),
            BookingProposal::Cancel {
                reason: "changed mind".to_owned(),
            },
            &authority(),
            &context(),
        )
        .await;
        // Full equality, not just the variant. The reference must be carried
        // through from `Booked` — cancelling the wrong council booking is
        // exactly what this state exists to make impossible.
        assert_eq!(
            got,
            Resolution::Ready(TransitionPlan::ExternalEffect {
                next_state: BookingState::CancellingBooking(CancellingBooking {
                    booking_ref: CouncilBookingRef::new("TH-92718"),
                    effect_intent_id: EffectIntentId::new("EFF-BKG-1001-BOOK-0"),
                }),
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
        assert_eq!(got, committed_local(BookingState::Cancelled(Cancelled)));
    }
}
