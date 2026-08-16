#![forbid(unsafe_code)]

use async_trait::async_trait;
use bld_kernel::{BoundaryDomain, Resolution};
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
    Reconcile,
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
            Self::Reconcile => "Reconcile",
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
    pub canonical_plan: BookingPlan,
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
    pub next_effect: u64,
    pub fake_booking_ref: CouncilBookingRef,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub enum BookingPlan {
    SelectVenue {
        venue_id: VenueId,
        slot_id: SlotId,
    },
    VerifySlot {
        facts: VenueFacts,
    },
    ChangeVenue,
    MarkNeedsRevalidation,
    RevalidateVenue {
        facts: VenueFacts,
    },
    Book {
        effect_intent_id: EffectIntentId,
        principal: PrincipalId,
        facts: VenueFacts,
    },
    CancelLocal,
    CancelBooked {
        booking_ref: CouncilBookingRef,
    },
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum BookingEvidence {
    NoExternalEffect,
    AvailabilityVerified(VenueFacts),
    BookingConfirmed {
        effect_intent_id: EffectIntentId,
        booking_ref: CouncilBookingRef,
    },
    CancellationConfirmed {
        booking_ref: CouncilBookingRef,
    },
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
    #[error("evidence does not match the canonical plan")]
    EvidenceMismatch,
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

#[async_trait]
impl BoundaryDomain for TownHallDomain {
    type State = BookingState;
    type Proposal = BookingProposal;
    type Authority = VerifiedAuthority;
    type Context = BookingContext;
    type Plan = BookingPlan;
    type Evidence = BookingEvidence;
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
    async fn resolve(
        &self,
        state: &Self::State,
        proposal: Self::Proposal,
        authority: &Self::Authority,
        context: &Self::Context,
    ) -> Resolution<Self::Plan, Self::Error> {
        match (state, proposal) {
            (BookingState::Draft(_), BookingProposal::SelectVenue { venue_id, slot_id }) => {
                Resolution::Ready(BookingPlan::SelectVenue { venue_id, slot_id })
            }
            (BookingState::Draft(_), BookingProposal::Cancel { .. }) => {
                Resolution::Ready(BookingPlan::CancelLocal)
            }
            (BookingState::VenueSelected(selected), BookingProposal::VerifySlot) => {
                let Some(facts) = context.selected_facts.clone() else {
                    return Resolution::Denied(BookingError::VenueFactsMissing);
                };
                if facts.venue_id != selected.venue_id || facts.slot_id != selected.slot_id {
                    return Resolution::Denied(BookingError::VenueFactsMissing);
                }
                match Self::validate_facts(&facts, &context.requirements, authority) {
                    Ok(()) => Resolution::Ready(BookingPlan::VerifySlot { facts }),
                    Err(error) => Resolution::Denied(error),
                }
            }
            (BookingState::VenueSelected(_), BookingProposal::ChangeVenue) => {
                Resolution::Ready(BookingPlan::ChangeVenue)
            }
            (BookingState::VenueSelected(_), BookingProposal::UpdateRequirements { .. }) => {
                Resolution::Ready(BookingPlan::MarkNeedsRevalidation)
            }
            (BookingState::VenueSelected(_), BookingProposal::Cancel { .. }) => {
                Resolution::Ready(BookingPlan::CancelLocal)
            }
            (BookingState::NeedsRevalidation(pending), BookingProposal::RevalidateVenue) => {
                let Some(facts) = context.selected_facts.clone() else {
                    return Resolution::Denied(BookingError::VenueFactsMissing);
                };
                // Bind the loaded facts back to what the user actually chose,
                // exactly as `VerifySlot` does against `VenueSelected`.
                //
                // The binding target is state data, not context, so this holds
                // without trusting whoever assembled the context. Without it,
                // an ordinary `UpdateRequirements` is enough to launder any
                // venue into the booking: every per-venue guard in
                // `validate_facts` passes for a venue the user never chose.
                let Some(selected) = pending.selected.as_ref() else {
                    return Resolution::Denied(BookingError::VenueFactsMissing);
                };
                if facts.venue_id != selected.venue_id || facts.slot_id != selected.slot_id {
                    return Resolution::Denied(BookingError::VenueFactsMissing);
                }
                match Self::validate_facts(&facts, &context.requirements, authority) {
                    Ok(()) => Resolution::Ready(BookingPlan::RevalidateVenue { facts }),
                    Err(error) => Resolution::Denied(error),
                }
            }
            (BookingState::NeedsRevalidation(_), BookingProposal::ChangeVenue) => {
                Resolution::Ready(BookingPlan::ChangeVenue)
            }
            (BookingState::NeedsRevalidation(_), BookingProposal::Cancel { .. }) => {
                Resolution::Ready(BookingPlan::CancelLocal)
            }
            (BookingState::AwaitingBooking(waiting), BookingProposal::Book) => {
                if !authority.may_book {
                    return Resolution::Denied(BookingError::BookingAuthorityRequired);
                }
                let Some(facts) = context.selected_facts.clone() else {
                    return Resolution::Denied(BookingError::VenueFactsMissing);
                };
                if facts.venue_id != waiting.venue_id
                    || facts.slot_id != waiting.slot_id
                    || facts.fee != waiting.verified_fee
                {
                    return Resolution::Denied(BookingError::VenueFactsMissing);
                }
                match Self::validate_facts(&facts, &context.requirements, authority) {
                    Ok(()) => Resolution::Ready(BookingPlan::Book {
                        effect_intent_id: EffectIntentId::new(format!(
                            "BOOK-{}-{}",
                            context.booking_id, context.next_effect
                        )),
                        principal: authority.principal.clone(),
                        facts,
                    }),
                    Err(error) => Resolution::Denied(error),
                }
            }
            (BookingState::AwaitingBooking(_), BookingProposal::ChangeVenue) => {
                Resolution::Ready(BookingPlan::ChangeVenue)
            }
            (BookingState::AwaitingBooking(_), BookingProposal::UpdateRequirements { .. }) => {
                Resolution::Ready(BookingPlan::MarkNeedsRevalidation)
            }
            (BookingState::AwaitingBooking(_), BookingProposal::Cancel { .. }) => {
                Resolution::Ready(BookingPlan::CancelLocal)
            }
            (BookingState::Booked(booked), BookingProposal::Cancel { .. }) => {
                if !authority.may_cancel {
                    return Resolution::Denied(BookingError::CancellationAuthorityRequired);
                }
                Resolution::Ready(BookingPlan::CancelBooked {
                    booking_ref: booked.booking_ref.clone(),
                })
            }
            _ => Resolution::Undefined,
        }
    }

    async fn execute(
        &self,
        plan: &Self::Plan,
        context: &mut Self::Context,
    ) -> Result<Self::Evidence, Self::Error> {
        match plan {
            BookingPlan::VerifySlot { facts } | BookingPlan::RevalidateVenue { facts } => {
                Ok(BookingEvidence::AvailabilityVerified(facts.clone()))
            }
            BookingPlan::Book {
                effect_intent_id, ..
            } => {
                context.next_effect += 1;
                Ok(BookingEvidence::BookingConfirmed {
                    effect_intent_id: effect_intent_id.clone(),
                    booking_ref: context.fake_booking_ref.clone(),
                })
            }
            BookingPlan::CancelBooked { booking_ref } => {
                Ok(BookingEvidence::CancellationConfirmed {
                    booking_ref: booking_ref.clone(),
                })
            }
            BookingPlan::SelectVenue { .. }
            | BookingPlan::ChangeVenue
            | BookingPlan::MarkNeedsRevalidation
            | BookingPlan::CancelLocal => Ok(BookingEvidence::NoExternalEffect),
        }
    }

    async fn validate(
        &self,
        current: &Self::State,
        plan: &Self::Plan,
        evidence: &Self::Evidence,
        _context: &Self::Context,
    ) -> Result<Self::State, Self::Error> {
        match (plan, evidence) {
            (BookingPlan::SelectVenue { venue_id, slot_id }, BookingEvidence::NoExternalEffect) => {
                Ok(BookingState::VenueSelected(VenueSelected {
                    venue_id: venue_id.clone(),
                    slot_id: slot_id.clone(),
                }))
            }
            (BookingPlan::VerifySlot { facts }, BookingEvidence::AvailabilityVerified(actual))
                if facts == actual =>
            {
                Ok(BookingState::AwaitingBooking(AwaitingBooking {
                    venue_id: facts.venue_id.clone(),
                    slot_id: facts.slot_id.clone(),
                    verified_fee: facts.fee,
                }))
            }
            (BookingPlan::ChangeVenue, BookingEvidence::NoExternalEffect) => {
                Ok(BookingState::Draft(Draft))
            }
            (BookingPlan::MarkNeedsRevalidation, BookingEvidence::NoExternalEffect) => {
                // Carry the selection forward from whichever state we came
                // from, so revalidation has an authoritative binding target
                // that does not depend on the caller-supplied context.
                let selected = match current {
                    BookingState::VenueSelected(selected) => Some(SelectedVenueRef {
                        venue_id: selected.venue_id.clone(),
                        slot_id: selected.slot_id.clone(),
                    }),
                    BookingState::AwaitingBooking(waiting) => Some(SelectedVenueRef {
                        venue_id: waiting.venue_id.clone(),
                        slot_id: waiting.slot_id.clone(),
                    }),
                    _ => None,
                };
                Ok(BookingState::NeedsRevalidation(NeedsRevalidation {
                    selected,
                }))
            }
            (
                BookingPlan::RevalidateVenue { facts },
                BookingEvidence::AvailabilityVerified(actual),
            ) if facts == actual => Ok(BookingState::VenueSelected(VenueSelected {
                venue_id: facts.venue_id.clone(),
                slot_id: facts.slot_id.clone(),
            })),
            (
                BookingPlan::Book {
                    effect_intent_id, ..
                },
                BookingEvidence::BookingConfirmed {
                    effect_intent_id: actual_effect,
                    booking_ref,
                },
            ) if effect_intent_id == actual_effect => Ok(BookingState::Booked(Booked {
                booking_ref: booking_ref.clone(),
            })),
            (BookingPlan::CancelLocal, BookingEvidence::NoExternalEffect) => {
                Ok(BookingState::Cancelled(Cancelled))
            }
            (
                BookingPlan::CancelBooked { booking_ref },
                BookingEvidence::CancellationConfirmed {
                    booking_ref: actual_ref,
                },
            ) if booking_ref == actual_ref => Ok(BookingState::Cancelled(Cancelled)),
            _ => Err(BookingError::EvidenceMismatch),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use bld_kernel::{BoundaryOutcome, Kernel};
    use bld_types::TimeWindow;

    fn authority() -> VerifiedAuthority {
        VerifiedAuthority {
            principal: PrincipalId::new("lucy"),
            actor: ActorId::new("townhall-agent"),
            max_fee: Money::from_pence(5_000),
            may_book: true,
            may_cancel: true,
        }
    }

    fn context() -> BookingContext {
        BookingContext {
            booking_id: BookingId::new("BKG-1001"),
            requirements: BookingRequirements {
                purpose: "meeting".into(),
                requested_date: "2026-08-20".into(),
                time_window: TimeWindow {
                    from: "13:00".into(),
                    to: "17:00".into(),
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
            next_effect: 1,
            fake_booking_ref: CouncilBookingRef::new("TH-92718"),
        }
    }

    #[tokio::test]
    async fn book_from_draft_is_undefined() {
        let mut state = BookingState::Draft(Draft);
        let mut ctx = context();

        let outcome = Kernel
            .apply(
                &TownHallDomain,
                &mut state,
                BookingProposal::Book,
                &authority(),
                &mut ctx,
            )
            .await;

        assert_eq!(outcome, BoundaryOutcome::Undefined);
        assert_eq!(state, BookingState::Draft(Draft));
    }

    #[tokio::test]
    async fn inaccessible_venue_is_denied_on_verification() {
        let mut state = BookingState::VenueSelected(VenueSelected {
            venue_id: VenueId::new("TH-B"),
            slot_id: SlotId::new("SLOT-B"),
        });
        let mut ctx = context();
        ctx.selected_facts = Some(VenueFacts {
            venue_id: VenueId::new("TH-B"),
            slot_id: SlotId::new("SLOT-B"),
            capacity: 25,
            wheelchair_accessible: false,
            fee: Money::from_pence(3_500),
            available: true,
        });

        let outcome = Kernel
            .apply(
                &TownHallDomain,
                &mut state,
                BookingProposal::VerifySlot,
                &authority(),
                &mut ctx,
            )
            .await;

        assert_eq!(
            outcome,
            BoundaryOutcome::Denied(BookingError::AccessibilityRequired)
        );
        assert!(matches!(state, BookingState::VenueSelected(_)));
    }

    #[tokio::test]
    async fn happy_path_books_then_cancels() {
        let domain = TownHallDomain;
        let kernel = Kernel;
        let auth = authority();
        let mut ctx = context();
        let mut state = BookingState::Draft(Draft);

        let out = kernel
            .apply(
                &domain,
                &mut state,
                BookingProposal::SelectVenue {
                    venue_id: VenueId::new("TH-A"),
                    slot_id: SlotId::new("SLOT-A"),
                },
                &auth,
                &mut ctx,
            )
            .await;
        assert!(matches!(
            out,
            BoundaryOutcome::Committed(BookingState::VenueSelected(_))
        ));

        let out = kernel
            .apply(
                &domain,
                &mut state,
                BookingProposal::VerifySlot,
                &auth,
                &mut ctx,
            )
            .await;
        assert!(matches!(
            out,
            BoundaryOutcome::Committed(BookingState::AwaitingBooking(_))
        ));

        let out = kernel
            .apply(&domain, &mut state, BookingProposal::Book, &auth, &mut ctx)
            .await;
        assert!(matches!(
            out,
            BoundaryOutcome::Committed(BookingState::Booked(_))
        ));

        let out = kernel
            .apply(
                &domain,
                &mut state,
                BookingProposal::Cancel {
                    reason: "user_cancelled".into(),
                },
                &auth,
                &mut ctx,
            )
            .await;
        assert_eq!(
            out,
            BoundaryOutcome::Committed(BookingState::Cancelled(Cancelled))
        );
    }

    /// The user's authoritative venue selection must survive a requirements
    /// update. This is the reachable path, not a synthetic one: `VenueSelected`
    /// + `UpdateRequirements` produces `NeedsRevalidation` today, and if
    /// `RevalidateVenue` does not bind the loaded facts back to the selection,
    /// whatever venue the context happens to carry silently becomes the booking.
    #[tokio::test]
    async fn revalidation_cannot_substitute_a_different_venue() {
        let domain = TownHallDomain;
        let kernel = Kernel;
        let auth = authority();
        let mut ctx = context();
        let mut state = BookingState::Draft(Draft);

        // Lucy selects TH-A.
        let out = kernel
            .apply(
                &domain,
                &mut state,
                BookingProposal::SelectVenue {
                    venue_id: VenueId::new("TH-A"),
                    slot_id: SlotId::new("SLOT-A"),
                },
                &auth,
                &mut ctx,
            )
            .await;
        assert!(matches!(
            out,
            BoundaryOutcome::Committed(BookingState::VenueSelected(_))
        ));

        // She changes the attendee count, which invalidates the verification.
        let out = kernel
            .apply(
                &domain,
                &mut state,
                BookingProposal::UpdateRequirements {
                    attendees: Some(25),
                },
                &auth,
                &mut ctx,
            )
            .await;
        assert!(matches!(
            out,
            BoundaryOutcome::Committed(BookingState::NeedsRevalidation(_))
        ));

        // Now the context carries facts for a DIFFERENT venue. Every guard in
        // `validate_facts` passes for TH-B in isolation - it is available,
        // large enough, accessible and within budget. Only the binding back to
        // the authoritative selection can catch this.
        ctx.selected_facts = Some(VenueFacts {
            venue_id: VenueId::new("TH-B"),
            slot_id: SlotId::new("SLOT-B"),
            capacity: 30,
            wheelchair_accessible: true,
            fee: Money::from_pence(4_500),
            available: true,
        });

        let out = kernel
            .apply(
                &domain,
                &mut state,
                BookingProposal::RevalidateVenue,
                &auth,
                &mut ctx,
            )
            .await;

        assert_eq!(
            out,
            BoundaryOutcome::Denied(BookingError::VenueFactsMissing),
            "revalidation accepted facts for a venue the user never selected"
        );
        assert!(
            matches!(state, BookingState::NeedsRevalidation(_)),
            "a denied revalidation must not advance state"
        );
    }

    /// The same path with matching facts must still succeed, so the guard above
    /// is not simply refusing everything.
    #[tokio::test]
    async fn revalidation_succeeds_when_facts_match_the_selection() {
        let domain = TownHallDomain;
        let kernel = Kernel;
        let auth = authority();
        let mut ctx = context();
        let mut state = BookingState::Draft(Draft);

        for proposal in [
            BookingProposal::SelectVenue {
                venue_id: VenueId::new("TH-A"),
                slot_id: SlotId::new("SLOT-A"),
            },
            BookingProposal::UpdateRequirements {
                attendees: Some(25),
            },
            BookingProposal::RevalidateVenue,
        ] {
            let out = kernel
                .apply(&domain, &mut state, proposal, &auth, &mut ctx)
                .await;
            assert!(
                matches!(out, BoundaryOutcome::Committed(_)),
                "expected a commit, got {out:?}"
            );
        }

        assert!(matches!(state, BookingState::VenueSelected(_)));
    }

    /// A row persisted before `NeedsRevalidation` carried a selection decodes
    /// with `selected: None`. It must refuse to revalidate rather than fall
    /// back to trusting the context.
    #[tokio::test]
    async fn legacy_revalidation_state_without_a_selection_is_denied() {
        let mut state = BookingState::NeedsRevalidation(NeedsRevalidation { selected: None });
        let mut ctx = context();

        let outcome = Kernel
            .apply(
                &TownHallDomain,
                &mut state,
                BookingProposal::RevalidateVenue,
                &authority(),
                &mut ctx,
            )
            .await;

        assert_eq!(
            outcome,
            BoundaryOutcome::Denied(BookingError::VenueFactsMissing)
        );
        assert!(matches!(state, BookingState::NeedsRevalidation(_)));
    }

    /// The wire form of a legacy row must still decode, or M3's
    /// restart-survival gate breaks for every existing `NeedsRevalidation`.
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

    #[tokio::test]
    async fn fee_over_authority_limit_is_denied() {
        let mut state = BookingState::VenueSelected(VenueSelected {
            venue_id: VenueId::new("TH-C"),
            slot_id: SlotId::new("SLOT-C"),
        });
        let mut ctx = context();
        ctx.selected_facts = Some(VenueFacts {
            venue_id: VenueId::new("TH-C"),
            slot_id: SlotId::new("SLOT-C"),
            capacity: 80,
            wheelchair_accessible: true,
            fee: Money::from_pence(9_000),
            available: true,
        });

        let outcome = Kernel
            .apply(
                &TownHallDomain,
                &mut state,
                BookingProposal::VerifySlot,
                &authority(),
                &mut ctx,
            )
            .await;

        assert_eq!(outcome, BoundaryOutcome::Denied(BookingError::FeeExceeded));
    }

    #[tokio::test]
    async fn insufficient_capacity_is_denied() {
        let mut state = BookingState::VenueSelected(VenueSelected {
            venue_id: VenueId::new("TH-D"),
            slot_id: SlotId::new("SLOT-D"),
        });
        let mut ctx = context();
        ctx.selected_facts = Some(VenueFacts {
            venue_id: VenueId::new("TH-D"),
            slot_id: SlotId::new("SLOT-D"),
            capacity: 12,
            wheelchair_accessible: true,
            fee: Money::from_pence(2_000),
            available: true,
        });

        let outcome = Kernel
            .apply(
                &TownHallDomain,
                &mut state,
                BookingProposal::VerifySlot,
                &authority(),
                &mut ctx,
            )
            .await;

        assert_eq!(
            outcome,
            BoundaryOutcome::Denied(BookingError::CapacityInsufficient {
                capacity: 12,
                required: 20,
            })
        );
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
    const PROPOSAL_COUNT: usize = 8;

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
            BookingProposal::Reconcile => 7,
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
            BookingProposal::Reconcile,
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
            next_effect: 1,
            fake_booking_ref: CouncilBookingRef::new("TH-92718"),
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
                    .resolve(&state, proposal.clone(), authority, context)
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
                    .resolve(&state, proposal.clone(), &authority, &context)
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
