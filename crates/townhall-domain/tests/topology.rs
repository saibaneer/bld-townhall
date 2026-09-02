//! Exports the transition topology, by running the domain.
//!
//! # Why this exists
//!
//! BLD's claim is that behaviour belongs to states and a state can only go where
//! its path allows. The unit tests *prove* that — 70 proposal cells, 40 fact
//! cells, every one asserted. But nothing *shows* it. Ask "where can
//! `AwaitingBooking` go?" and the answer is spread across match arms in three
//! resolvers and half a dozen helpers.
//!
//! For an implementation, tests holding the line is enough. For a principle
//! others adopt, the graph is the deliverable, and it has to be readable in one
//! place.
//!
//! # Why it is generated rather than written
//!
//! A hand-drawn diagram is a second source of truth that rots silently. This
//! walks every (state, input) pair through the **real domain** and records what
//! came back, so the artifact cannot disagree with the code — it *is* the code's
//! behaviour, serialised.
//!
//! The output is committed, which is the point: a change to the state machine
//! shows up as a diff in the graph, in the pull request, where someone will see
//! it. A test that merely regenerated on demand would let a topology change land
//! unremarked.
//!
//! Run `UPDATE_TOPOLOGY=1 cargo test -p townhall-domain --test topology` to
//! rewrite the artifacts after a deliberate change.
//!
//! # Why an integration test and not a unit test
//!
//! Integration tests see only the public API. If the topology could not be
//! derived from outside this crate, that would be a finding about the API rather
//! than something to work around — an external toolchain (a diagram renderer, a
//! synthesis step) needs exactly this access.
//!
//! # What the artifact does and does not guarantee
//!
//! It guarantees **totality**: every (state, input) pair has an entry, so no input
//! sequence can reach a cell nobody specified. Unreachable transitions are
//! *absent* rather than guarded, which is the difference between a rule enforced
//! at runtime and a path that does not exist.
//!
//! It says nothing about whether the guards on a permitted edge are correct, and
//! nothing about whether the world matches what a state believes.

use bld_kernel::{BoundaryDomain, FactResolution, Resolution, TransitionPlan, Verified};
use bld_types::{
    AvailabilityGrant, BookingId, BookingRequirements, CouncilBookingRef, EffectIntentId, Money,
    PrincipalId, SlotId, TimeWindow, VenueId,
};
use std::{fmt::Write as _, fs, path::PathBuf};
use townhall_domain::{
    AwaitingBooking, Booked, Booking, BookingContext, BookingEffect, BookingProposal, BookingState,
    CancellationRequested, Cancelled, CancellingBooking, Draft, EffectIntent, EffectStatus,
    FactContext, NeedsHuman, NeedsRevalidation, OperationKind, SelectedVenueRef, SystemEvent,
    TownHallDomain, VenueFacts, VenueSelected, VerifiedAuthority, VerifiedAvailability,
    VerifiedProviderFact,
};

/// One cell of the topology: what happens to this state on this input.
#[derive(Clone, Debug, PartialEq, Eq)]
enum Cell {
    /// No edge. The behaviour does not exist here.
    ///
    /// Data-independent, and that is what makes it exportable: the domain decides
    /// `Undefined` from the (state, input) pair before any guard reads the
    /// aggregate. So this answer holds for every possible booking, not just the
    /// fixture below.
    NoEdge,
    /// An edge to `to`, taken entirely inside the boundary.
    Local { to: &'static str },
    /// An edge to `to` that also asks the outside world for something.
    External { to: &'static str, effect: String },
    /// The edge exists, and this fixture's data did not satisfy its guard.
    ///
    /// The target is not observable here, which is honest rather than a gap: a
    /// guarded edge's destination depends on data, and pretending otherwise would
    /// put a line on the graph that a reader could not rely on.
    Guarded { refused: String },
    /// Authoritative state already reflects this input. Fact door only.
    Converged,
    /// Accepted, and recorded durably against the effect — with **no**
    /// transition. System-event door only (ADR-019): exhaustion writes a pursuit
    /// marker and the booking stays exactly where it is.
    Records,
}

impl Cell {
    fn tag(&self) -> &'static str {
        match self {
            Self::NoEdge => "no_edge",
            Self::Local { .. } => "local",
            Self::External { .. } => "external",
            Self::Guarded { .. } => "guarded",
            Self::Converged => "converged",
            Self::Records => "records",
        }
    }

    fn target(&self) -> Option<&'static str> {
        match self {
            Self::Local { to } | Self::External { to, .. } => Some(to),
            _ => None,
        }
    }
}

struct Door {
    name: &'static str,
    /// How this door's edges are drawn, so the three are distinguishable at a
    /// glance: intent, external reality, runtime fact.
    arrow: &'static str,
    inputs: Vec<String>,
    /// `cells[state_index][input_index]`
    cells: Vec<Vec<Cell>>,
    /// Whether this door's cells are decided by `(state, input)` alone.
    ///
    /// True for the proposal and system-event doors, and that is the whole of the
    /// safety claim: those cells are a fixed table, so no sequence of inputs can
    /// reach one nobody specified.
    ///
    /// False for the fact door. Its cells read the *persisted intent* — kind, and
    /// status — so the same fact means different things depending on what was in
    /// flight. That is ADR-012 working as designed rather than a wrinkle: reality's
    /// meaning depends on what we were doing. PR review caught this claimed as a
    /// fixed table when it is not one.
    fixed_table: bool,
    /// What a machine-readable consumer needs to know about the axes.
    dimensions: &'static str,
}

// ------------------------------------------------------------------- fixtures

/// A booking whose data satisfies every guard it can.
///
/// Deliberately permissive. The export needs to distinguish "no edge here" from
/// "an edge a guard happened to refuse", and a fixture that failed guards would
/// report the second as though it told you something about the topology. With a
/// permissive fixture, a `Guarded` cell means the guard is genuinely
/// data-dependent rather than incidentally unsatisfied.
fn requirements() -> BookingRequirements {
    BookingRequirements {
        purpose: "town hall".to_owned(),
        requested_date: "2026-09-01".to_owned(),
        time_window: TimeWindow {
            from: "18:00".to_owned(),
            to: "20:00".to_owned(),
        },
        attendees: 20,
        wheelchair_accessible: true,
        max_fee: Money::from_pence(9_000),
    }
}

fn facts() -> VenueFacts {
    VenueFacts {
        venue_id: VenueId::new("TH-A"),
        slot_id: SlotId::new("SLOT-A"),
        capacity: 30,
        wheelchair_accessible: true,
        fee: Money::from_pence(4_500),
        available: true,
    }
}

fn selection() -> SelectedVenueRef {
    SelectedVenueRef {
        venue_id: VenueId::new("TH-A"),
        slot_id: SlotId::new("SLOT-A"),
    }
}

const BOOK_EFFECT: &str = "EFF-BKG-1001-BOOK-0";
const CANCEL_EFFECT: &str = "EFF-BKG-1001-CANCEL-0";
const REFERENCE: &str = "TH-92718";
/// A successor identity, for the one fact-door cell that mints a new effect.
const SUCCESSOR_EFFECT: &str = "EFF-BKG-1001-CANCEL-9";

/// A grant over this suite's one fixture booking.
///
/// Issued through the real approval path rather than constructed: the envelope
/// has private fields and no `test-support` constructor, because a feature that
/// revealed one would leak through unification (ADR-025). The topology this
/// file pins does not depend on authority at all — that is the point it makes —
/// so any real grant serves.
fn authority() -> VerifiedAuthority {
    // EVERY behaviour. This suite pins which cells EXIST, and since M7B
    // consults the grant for every proposal, a fixture missing one would turn a
    // legal cell into a guarded one and read as a topology change — a fixture
    // detail masquerading as a state-machine change, in the one file whose job
    // is to make real changes visible.
    townhall_testkit::issuer::issue_blocking(
        &townhall_testkit::issuer::GrantSpec::own("lucy", "BKG-1001", 9_000)
            .permitting(townhall_testkit::issuer::ALL),
    )
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
        BookingState::BookingInProgress(townhall_domain::BookingInProgress {
            effect_intent_id: EffectIntentId::new(BOOK_EFFECT),
        }),
        BookingState::CancellationRequested(CancellationRequested {
            effect_intent_id: EffectIntentId::new(BOOK_EFFECT),
            cancelled_by: PrincipalId::new("lucy"),
        }),
        BookingState::Booked(Booked {
            booking_ref: CouncilBookingRef::new(REFERENCE),
        }),
        BookingState::CancellingBooking(CancellingBooking {
            booking_ref: CouncilBookingRef::new(REFERENCE),
            effect_intent_id: EffectIntentId::new(CANCEL_EFFECT),
        }),
        BookingState::Cancelled(Cancelled),
        BookingState::NeedsHuman(NeedsHuman),
    ]
}

/// A coherent aggregate for `state`.
///
/// Coherence is gated on every door, so an incoherent fixture would make every
/// cell fail for the same uninteresting reason and the export would be empty.
fn booking_for(state: &BookingState) -> Booking {
    Booking {
        id: BookingId::new("BKG-1001"),
        requirements: requirements(),
        selected_venue: state.selection(),
        availability: state.selection().map(|_| facts()),
        booking_ref: state.council_booking_ref().cloned(),
        active_effect: state.effect_intent_id().cloned(),
        state: state.clone(),
    }
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
            attendees: Some(20),
        },
        BookingProposal::RevalidateVenue,
        BookingProposal::Book,
        BookingProposal::Cancel {
            reason: "no longer needed".to_owned(),
        },
    ]
}

/// The facts, each about whichever effect the state under test is waiting on.
///
/// A fact naming an unrelated effect is refused by the binding, which would make
/// every cell look guarded and tell us nothing about the topology.
fn facts_about(effect: &str) -> Vec<VerifiedProviderFact> {
    vec![
        VerifiedProviderFact::BookingExists {
            effect_intent_id: EffectIntentId::new(effect),
            booking_ref: CouncilBookingRef::new(REFERENCE),
            venue_id: VenueId::new("TH-A"),
            slot_id: SlotId::new("SLOT-A"),
            attendees: 20,
            fee: Money::from_pence(4_500),
            principal: PrincipalId::new("lucy"),
        },
        VerifiedProviderFact::EffectAbsent {
            effect_intent_id: EffectIntentId::new(effect),
        },
        VerifiedProviderFact::CancellationExists {
            effect_intent_id: EffectIntentId::new(effect),
            booking_ref: CouncilBookingRef::new(REFERENCE),
        },
        VerifiedProviderFact::ProviderRejected {
            effect_intent_id: EffectIntentId::new(effect),
            reason: bld_types::BoundedString::truncating("the council refused"),
        },
    ]
}

/// The persisted intent a fact is bound against.
///
/// `kind` is a *parameter* rather than derived from the state, and that is the
/// correction PR review forced. Deriving it — `in_flight_kind().unwrap_or(Book)` —
/// gave every settled state a *booking* intent, so the export reported
/// `Booked + EffectAbsent` as guarded. The real domain converges there when the
/// intent is a **cancellation**: a cancellation that did not happen leaves the
/// booking booked. The fixture was answering a question the machine was not asked.
fn intent_for(
    state: &BookingState,
    effect: &str,
    kind: OperationKind,
    status: EffectStatus,
) -> EffectIntent {
    let _ = state;
    EffectIntent {
        effect_intent_id: EffectIntentId::new(effect),
        booking_id: BookingId::new("BKG-1001"),
        operation_kind: kind,
        source_version: 0,
        canonical_plan: match kind {
            OperationKind::Book => BookingEffect::Book {
                principal: PrincipalId::new("lucy"),
                attendees: 20,
                facts: facts(),
                grant: AvailabilityGrant::new("export-grant"),
            },
            OperationKind::Cancel => BookingEffect::CancelBooking {
                booking_ref: CouncilBookingRef::new(REFERENCE),
                principal: PrincipalId::new("lucy"),
            },
        },
        status,
        expires_at_ms: 1_000_030_000,
        provider_reference: None,
        outcome_detail: None,
        supersedes: None,
        created_at_ms: 1_000_000_000,
        updated_at_ms: 1_000_000_000,
    }
}

// ------------------------------------------------------------------- the sweep

fn classify_plan(plan: &TransitionPlan<Booking, BookingEffect>) -> Cell {
    match plan {
        TransitionPlan::Local { next_state } => Cell::Local {
            to: next_state.state.name(),
        },
        TransitionPlan::ExternalEffect { next_state, effect } => Cell::External {
            to: next_state.state.name(),
            effect: match effect {
                BookingEffect::Book { .. } => "Book".to_owned(),
                BookingEffect::CancelBooking { .. } => "CancelBooking".to_owned(),
            },
        },
    }
}

async fn proposal_door() -> Door {
    let mut cells = Vec::new();
    for state in all_states() {
        let booking = booking_for(&state);
        let mut row = Vec::new();
        for proposal in all_proposals() {
            let context = BookingContext {
                selected_facts: townhall_domain::ObservedAvailability::Answered(
                    state.selection().map(|_| {
                        Verified::assert_verified(VerifiedAvailability {
                            facts: facts(),
                            grant: AvailabilityGrant::new("export-grant"),
                        })
                    }),
                ),
                pending_effect: Some(EffectIntentId::new(BOOK_EFFECT)),
            };
            let proposal_name = proposal.name();
            let resolved = TownHallDomain
                .resolve_proposal(&booking, proposal, &authority(), &context)
                .await;
            // The export's second witness (the LOCKED-table test is the
            // first): the menu the domain EXPORTS must equal what the domain
            // DOES, cell by cell, in the same run that generates the docs.
            assert_eq!(
                state.proposal_menu().contains(&proposal_name),
                !matches!(resolved, Resolution::Undefined),
                "{} + {proposal_name}: the exported menu disagrees with the resolved topology",
                state.name()
            );
            row.push(match resolved {
                Resolution::Undefined => Cell::NoEdge,
                Resolution::Denied(error) => Cell::Guarded {
                    refused: error.to_string(),
                },
                Resolution::Ready(plan) => classify_plan(&plan),
            });
        }
        cells.push(row);
    }

    Door {
        name: "proposal",
        arrow: "-->",
        inputs: all_proposals()
            .iter()
            .map(|p| p.name().to_owned())
            .collect(),
        cells,
        fixed_table: true,
        dimensions: "state x proposal",
    }
}

async fn fact_door() -> Door {
    let mut cells = Vec::new();
    for state in all_states() {
        let booking = booking_for(&state);
        let effect = state
            .effect_intent_id()
            .map_or(BOOK_EFFECT, |id| match id.as_str() {
                CANCEL_EFFECT => CANCEL_EFFECT,
                _ => BOOK_EFFECT,
            });
        let mut row = Vec::new();
        for (kind, status, fact) in fact_inputs() {
            // Mirrors how the coordinator builds this context, which is the only
            // way the artifact describes the machine rather than the fixture.
            //
            // The intent is always present: the coordinator loads it by the
            // *fact's* effect id, not from the aggregate's `active_effect`. That
            // matters here — a settled state like `Booked` has no active effect,
            // but a reconciler re-applying a fact about the effect that produced
            // it can still load that intent, and the door has something to say
            // about it. Withholding the intent instead would report every settled
            // state as "no intent supplied", which is a statement about the
            // fixture and not about the topology.
            //
            // `pending_effect` follows the domain's own answer for whether this
            // cell mints a successor effect. Its value only needs to be present;
            // the repository verifies the actual identity.
            let context = FactContext {
                intent: Some(intent_for(&state, effect, kind, status)),
                pending_effect: TownHallDomain::fact_intended_effect_kind(&state, &fact)
                    .map(|_| EffectIntentId::new(SUCCESSOR_EFFECT)),
            };
            let resolved = TownHallDomain
                .resolve_fact(&booking, Verified::assert_verified(fact), &context)
                .await;
            row.push(match resolved {
                FactResolution::Undefined => Cell::NoEdge,
                FactResolution::Denied(error) => Cell::Guarded {
                    refused: error.to_string(),
                },
                FactResolution::Converged => Cell::Converged,
                FactResolution::Ready(plan) => classify_plan(&plan),
            });
        }
        cells.push(row);
    }

    Door {
        name: "fact",
        arrow: "-.->",
        inputs: fact_inputs()
            .iter()
            .map(|(kind, status, fact)| {
                format!("{} · intent {} {status:?}", fact.name(), kind.name())
            })
            .collect(),
        cells,
        fixed_table: false,
        dimensions: "state x fact x intent kind x intent status",
    }
}

/// The fact door reads the persisted intent, so its input is not the fact alone.
///
/// Both dimensions were found by PR review, in two steps. The intent's **kind**
/// first: the same fact against a booking intent and against a cancellation intent
/// means different things. Then its **status**, which is what decides whether a
/// fact is fresh news or a re-application of something already settled — a
/// cancellation that did not happen leaves a booking `Booked`, and re-applying
/// that absence once the intent is already `Absent` is `Converged` rather than a
/// contradiction.
///
/// Enumerating facts alone exported a quarter of the input space and called it
/// total.
fn fact_inputs() -> Vec<(OperationKind, EffectStatus, VerifiedProviderFact)> {
    let statuses = [
        EffectStatus::Prepared,
        EffectStatus::Unknown,
        EffectStatus::Confirmed,
        EffectStatus::Rejected,
        EffectStatus::Absent,
    ];

    [OperationKind::Book, OperationKind::Cancel]
        .into_iter()
        .flat_map(move |kind| {
            statuses.into_iter().flat_map(move |status| {
                facts_about(BOOK_EFFECT)
                    .into_iter()
                    .map(move |fact| (kind, status, fact))
            })
        })
        .collect()
}

async fn system_event_door() -> Door {
    let mut cells = Vec::new();
    for state in all_states() {
        let booking = booking_for(&state);
        let effect = state
            .effect_intent_id()
            .map_or(BOOK_EFFECT, |id| id.as_str());
        // This door takes no context at all — the only binding a system event
        // needs is "is this the effect this state is waiting on", and the state
        // carries that itself.
        let event = SystemEvent::ReconciliationExhausted {
            effect_intent_id: EffectIntentId::new(effect),
        };
        let resolved = TownHallDomain.resolve_system_event(&booking, event).await;
        cells.push(vec![match resolved {
            bld_kernel::SystemEventResolution::Undefined => Cell::NoEdge,
            bld_kernel::SystemEventResolution::Denied(error) => Cell::Guarded {
                refused: error.to_string(),
            },
            // A durable record with no transition — ADR-019. Neither `no_edge`
            // (there is behaviour), nor `local` (nothing moves), nor `converged`
            // (something is written): its own tag, or the artifact lies.
            bld_kernel::SystemEventResolution::Record => Cell::Records,
        }]);
    }

    Door {
        name: "system_event",
        arrow: "==>",
        inputs: vec!["ReconciliationExhausted".to_owned()],
        cells,
        // `resolve_system_event` takes no context at all, so this door is decided
        // by (state, event) with nothing else in reach.
        fixed_table: true,
        dimensions: "state x event",
    }
}

// ------------------------------------------------------------------- rendering

fn to_json(doors: &[Door], states: &[String]) -> String {
    let mut out = String::from("{\n");
    let _ = writeln!(
        out,
        "  \"generated_by\": \"cargo test -p townhall-domain --test topology\","
    );
    let _ = writeln!(
        out,
        "  \"note\": \"Derived by running the domain. A `no_edge` cell is a transition that does not exist, not one refused at runtime. Read `fixed_table` per door before relying on a cell: where it is true the door is decided by (state, input) alone and the enumeration is complete; where it is false the door reads persisted data and `dimensions` names every axis varied here.\","
    );
    let _ = write!(out, "  \"states\": [");
    let names: Vec<String> = states.iter().map(|s| format!("\"{s}\"")).collect();
    let _ = write!(out, "{}", names.join(", "));
    let _ = writeln!(out, "],");

    let _ = writeln!(out, "  \"doors\": [");
    for (door_ix, door) in doors.iter().enumerate() {
        let _ = writeln!(out, "    {{");
        let _ = writeln!(out, "      \"name\": \"{}\",", door.name);
        let _ = writeln!(out, "      \"fixed_table\": {},", door.fixed_table);
        let _ = writeln!(out, "      \"dimensions\": \"{}\",", door.dimensions);
        let inputs: Vec<String> = door.inputs.iter().map(|i| format!("\"{i}\"")).collect();
        let _ = writeln!(out, "      \"inputs\": [{}],", inputs.join(", "));
        let _ = writeln!(out, "      \"cells\": [");

        let mut rendered = Vec::new();
        for (state_ix, row) in door.cells.iter().enumerate() {
            for (input_ix, cell) in row.iter().enumerate() {
                let mut fields = format!(
                    "\"from\": \"{}\", \"input\": \"{}\", \"edge\": \"{}\"",
                    states[state_ix],
                    door.inputs[input_ix],
                    cell.tag()
                );
                if let Some(to) = cell.target() {
                    let _ = write!(fields, ", \"to\": \"{to}\"");
                }
                if let Cell::External { effect, .. } = cell {
                    let _ = write!(fields, ", \"effect\": \"{effect}\"");
                }
                if let Cell::Guarded { refused } = cell {
                    let _ = write!(fields, ", \"refused_here\": \"{refused}\"");
                }
                rendered.push(format!("        {{{fields}}}"));
            }
        }
        let _ = writeln!(out, "{}", rendered.join(",\n"));
        let _ = writeln!(out, "      ]");
        let comma = if door_ix + 1 == doors.len() { "" } else { "," };
        let _ = writeln!(out, "    }}{comma}");
    }
    let _ = writeln!(out, "  ]");
    out.push_str("}\n");
    out
}

fn to_markdown(doors: &[Door], states: &[String]) -> String {
    let mut out = String::new();
    out.push_str("# Transition topology\n\n");
    out.push_str(
        "**Generated — do not edit.** Produced by running the domain:\n\n\
         ```\n\
         UPDATE_TOPOLOGY=1 cargo test -p townhall-domain --test topology\n\
         ```\n\n\
         Every (state, input) pair below has an entry. A pair that is absent from the graph is a \
         transition that **does not exist** — not one refused at runtime. That is the difference \
         between a rule a caller can argue with and a path that was never wired, and it is what \
         `Undefined` means throughout this codebase.\n\n\
         Three doors, three arrow styles: intent, externally verified reality, runtime fact. A \
         proposer's vocabulary reaches only the first.\n\n",
    );

    out.push_str("```mermaid\nstateDiagram-v2\n");
    for state in states {
        let _ = writeln!(out, "    {state}");
    }
    for door in doors {
        for (state_ix, row) in door.cells.iter().enumerate() {
            for (input_ix, cell) in row.iter().enumerate() {
                if let Some(to) = cell.target() {
                    let mark = if matches!(cell, Cell::External { .. }) {
                        " ⇗"
                    } else {
                        ""
                    };
                    let _ = writeln!(
                        out,
                        "    {} {} {} : {}{}",
                        states[state_ix], door.arrow, to, door.inputs[input_ix], mark
                    );
                }
            }
        }
    }
    out.push_str("```\n\n");
    out.push_str("`⇗` marks an edge that asks the outside world for something, so it commits an in-flight state first and settles later on verified evidence (ADR-014).\n\n");

    for door in doors {
        let _ = writeln!(out, "## The {} door\n", door.name);
        if door.fixed_table {
            out.push_str(&fixed_table_section(door, states));
        } else {
            out.push_str(&reachability_section(door, states));
        }
    }

    out.push_str(
        "`—` is no edge. `guarded` means the edge exists and this export's data did not satisfy \
         it, so its destination depends on the data and is deliberately not drawn. `converged` \
         means authoritative state already reflected the input, which is success rather than a \
         refusal — a reconciler re-applies facts by design.\n",
    );
    out
}

/// A door decided by `(state, input)` alone, as a matrix.
fn fixed_table_section(door: &Door, states: &[String]) -> String {
    let mut out = String::new();
    {
        {
            let _ = writeln!(
                out,
                "**A fixed table**, over `{}`. Every cell is decided by the state and the input \
                 alone — nothing else is in reach — so this enumeration is complete and no input \
                 sequence can reach a cell nobody specified. This is where the safety claim lives, \
                 and it is the door an untrusted proposer can reach.\n",
                door.dimensions
            );
        }
    }

    let _ = write!(out, "| from |");
    for input in &door.inputs {
        let _ = write!(out, " {input} |");
    }
    out.push('\n');
    let _ = write!(out, "|---|");
    for _ in &door.inputs {
        out.push_str("---|");
    }
    out.push('\n');

    for (state_ix, row) in door.cells.iter().enumerate() {
        let _ = write!(out, "| **{}** |", states[state_ix]);
        for cell in row {
            let _ = write!(out, " {} |", describe(cell));
        }
        out.push('\n');
    }
    out.push('\n');
    out
}

/// A door that reads persisted data, as a list of the edges found.
fn reachability_section(door: &Door, states: &[String]) -> String {
    let mut out = String::new();
    {
        {
            let _ = writeln!(
                out,
                "**Not a fixed table.** This door reads the *persisted intent*, so its axes are \
                 `{}` — the same fact means different things depending on what was in flight, \
                 which is ADR-012 working as designed rather than a wrinkle. Every axis above is \
                 varied below, but do not read this as a combinational table: it is a reachability \
                 view.\n\n\
                 Only the edges are listed. A pair absent from this list has no edge, and there \
                 are too many columns for a matrix to be readable.\n",
                door.dimensions
            );
        }
    }

    let mut any = false;
    for (state_ix, row) in door.cells.iter().enumerate() {
        for (input_ix, cell) in row.iter().enumerate() {
            let text = match cell {
                Cell::Local { to } => format!("→ {to}"),
                Cell::External { to, effect } => format!("→ {to} ⇗ {effect}"),
                Cell::Converged => "converged".to_owned(),
                Cell::Records => "records ⏺ (no transition)".to_owned(),
                Cell::NoEdge | Cell::Guarded { .. } => continue,
            };
            any = true;
            let _ = writeln!(
                out,
                "- **{}** on `{}` {text}",
                states[state_ix], door.inputs[input_ix]
            );
        }
    }
    if !any {
        out.push_str("- (none)\n");
    }
    out.push('\n');
    out
}

fn describe(cell: &Cell) -> String {
    match cell {
        Cell::NoEdge => "—".to_owned(),
        Cell::Local { to } => format!("→ {to}"),
        Cell::External { to, effect } => format!("→ {to} ⇗ {effect}"),
        Cell::Guarded { refused } => format!("guarded ({refused})"),
        Cell::Converged => "converged".to_owned(),
        Cell::Records => "records ⏺".to_owned(),
    }
}

// --------------------------------------------------------------------- the gate

fn artifact_path(name: &str) -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR"))
        .join("../../docs")
        .join(name)
}

/// The graph is committed, and this fails if the code no longer produces it.
///
/// Not "regenerate on demand": a topology change that nobody looked at is exactly
/// what this is for. Committing the artifact puts the change in the diff.
#[tokio::test]
async fn the_committed_topology_matches_the_domain() {
    let states: Vec<String> = all_states().iter().map(|s| s.name().to_owned()).collect();
    let doors = vec![
        proposal_door().await,
        fact_door().await,
        system_event_door().await,
    ];

    // Totality: the property the whole artifact is for. Every state, every input,
    // an entry — so no input sequence can reach a cell nobody specified.
    for door in &doors {
        assert_eq!(
            door.cells.len(),
            states.len(),
            "the {} door skipped a state",
            door.name
        );
        for row in &door.cells {
            assert_eq!(
                row.len(),
                door.inputs.len(),
                "the {} door skipped an input",
                door.name
            );
        }
    }

    let artifacts = [
        ("topology.json", to_json(&doors, &states)),
        ("topology.md", to_markdown(&doors, &states)),
    ];

    if std::env::var("UPDATE_TOPOLOGY").is_ok() {
        for (name, content) in &artifacts {
            fs::write(artifact_path(name), content).expect("write the artifact");
        }
        return;
    }

    for (name, content) in &artifacts {
        let committed = fs::read_to_string(artifact_path(name)).unwrap_or_default();
        assert_eq!(
            committed.trim(),
            content.trim(),
            "docs/{name} no longer matches the domain. If the state machine changed on \
             purpose, rerun with UPDATE_TOPOLOGY=1 and commit the diff — the point is that a \
             topology change is visible in review."
        );
    }
}
