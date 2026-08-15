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

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct NeedsRevalidation;

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
    NeedsRevalidation(NeedsRevalidation),
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

#[derive(Clone, Debug)]
pub struct BookingContext {
    pub booking_id: BookingId,
    pub requirements: BookingRequirements,
    /// The user's authoritative venue selection, reloaded from the aggregate.
    ///
    /// This is what `selected_facts` must be bound *against*. The two are not
    /// the same thing: this is what the user chose, `selected_facts` is what a
    /// capability loaded. A behaviour that consumes loaded facts without
    /// checking them against this is laundering an unverified venue into the
    /// booking.
    pub selected_venue: Option<SelectedVenueRef>,
    pub selected_facts: Option<VenueFacts>,
    pub next_effect: u64,
    pub fake_booking_ref: CouncilBookingRef,
}

#[derive(Clone, Debug, PartialEq, Eq)]
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
            (BookingState::NeedsRevalidation(_), BookingProposal::RevalidateVenue) => {
                let Some(facts) = context.selected_facts.clone() else {
                    return Resolution::Denied(BookingError::VenueFactsMissing);
                };
                // Bind the loaded facts back to the user's authoritative
                // selection, exactly as `VerifySlot` does.
                //
                // `VerifySlot` can read the selection off `VenueSelected`
                // itself; `NeedsRevalidation` carries no venue, so the binding
                // target is the aggregate's `selected_venue`. Without it, an
                // ordinary `UpdateRequirements` is enough to launder whatever
                // venue the context happens to hold into the booking - the
                // per-venue guards in `validate_facts` all pass for a venue the
                // user never chose.
                let Some(selected) = context.selected_venue.as_ref() else {
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
        _current: &Self::State,
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
                Ok(BookingState::NeedsRevalidation(NeedsRevalidation))
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
            selected_venue: Some(SelectedVenueRef {
                venue_id: VenueId::new("TH-A"),
                slot_id: SlotId::new("SLOT-A"),
            }),
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
