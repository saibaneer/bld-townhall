# Town Hall State Machine

State vocabulary follows the technical specification. **Edge provenance follows ADR-012** —
transitions are grouped by which door drives them, because that is the security boundary,
not a presentational choice.

> Canonical type definitions live in [`decisions.md`](decisions.md) (ADR-012). This document
> shows the graph; it deliberately does not restate field lists, so the two cannot drift.

## Intent edges — a human or agent may request these

Reachable through `BookingProposal` via `resolve_proposal`.

```mermaid
stateDiagram-v2
    [*] --> Draft
    Draft --> VenueSelected: select_venue
    Draft --> Cancelled: cancel

    VenueSelected --> AwaitingBooking: verify_slot
    VenueSelected --> Draft: change_venue
    VenueSelected --> NeedsRevalidation: update_requirements
    VenueSelected --> Cancelled: cancel

    NeedsRevalidation --> VenueSelected: revalidate_venue
    NeedsRevalidation --> Draft: change_venue
    NeedsRevalidation --> Cancelled: cancel

    AwaitingBooking --> BookingInProgress: book
    AwaitingBooking --> Draft: change_venue
    AwaitingBooking --> NeedsRevalidation: update_requirements
    AwaitingBooking --> Cancelled: cancel

    BookingInProgress --> CancellationRequested: cancel
    Booked --> CancellingBooking: cancel
```

Note `book` now targets `BookingInProgress`, not `Booked`: the effect intent is committed
before the council is called (ADR-014).

## Observation edges — only verified provider facts may drive these

Reachable only through `VerifiedProviderFact` via `resolve_fact`. **No proposer can submit
these.** The fact is state-neutral; the label below is the domain's *interpretation* of it
in that state.

```mermaid
stateDiagram-v2
    BookingInProgress --> Booked: BookingExists
    BookingInProgress --> AwaitingBooking: EffectAbsent
    BookingInProgress --> AwaitingBooking: ProviderRejected

    CancellationRequested --> CancellingBooking: BookingExists
    CancellationRequested --> Cancelled: EffectAbsent

    CancellingBooking --> Cancelled: CancellationExists
    CancellingBooking --> Booked: EffectAbsent
    CancellingBooking --> Booked: ProviderRejected
```

One `EffectAbsent` fact, three meanings — derived from the persisted intent and the current
state, never from the fact itself:

| At | The absent intent was | Means | Goes to |
|---|---|---|---|
| `BookingInProgress` | a booking | the booking never happened | `AwaitingBooking` |
| `CancellationRequested` | a booking | there is nothing to cancel | `Cancelled` |
| `CancellingBooking` | a cancellation | the cancellation never happened | `Booked` |

The first is the commonest recovery path, not a corner case: the create request never
arrived, its deadline passed, and the council tombstoned the intent. The third is its exact
mirror on the cancellation side. Each finalises the old intent and clears `active_effect`,
and any re-proposal mints a **fresh** intent — a tombstoned one can never succeed, so reusing
it would guarantee an effect that never happens.

The same `BookingExists` fact means *booking confirmed* at `BookingInProgress` and *booking
found* at `CancellationRequested`. That is what lets a fact which lost a compare-and-set be
re-evaluated against the new state rather than discarded.

> Every `EffectAbsent` edge above is gated on **ADR-016**: absence is admissible only from
> the council's definitive-absence response, which durably tombstones the intent. Before
> that, a "not found" is `Unknown` and drives nothing — the council may still act on an
> in-flight request.

## System-event edges — deterministic runtime facts

Reachable only through `SystemEvent` via `resolve_system_event`. Neither intent nor
external fact: the council cannot tell us our own retry budget is exhausted.

```mermaid
stateDiagram-v2
    BookingInProgress --> NeedsHuman: ReconciliationExhausted
    CancellationRequested --> NeedsHuman: ReconciliationExhausted
    CancellingBooking --> NeedsHuman: ReconciliationExhausted
```

`NeedsHuman` is reachable only this way, which is why M4 builds this door rather than
deferring it.

## Terminal states

`Cancelled` and `NeedsHuman` have no outbound edges through any door.

## State-scoped behaviour rule

Concrete state types own only the behaviours that exist in that state — `Draft` can select a
venue or cancel, but has no `book`. An absent `(state, proposal)` pairing resolves to
`Resolution::Undefined`, which is distinct from `Denied`: the first means the behaviour does
not exist here at all, the second that it exists but a guard refused it.

The same trichotomy applies to the fact door, which adds a fourth outcome, `Converged`, for
a fact that local state already reflects. See ADR-012.

The full 70-cell intent topology (10 states x 7 proposals, once `Reconcile` is removed) is pinned by the `topology` test module in
`crates/townhall-domain`; `LOCKED` there is the executable form of the intent graph above.
