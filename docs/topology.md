# Transition topology

**Generated — do not edit.** Produced by running the domain:

```
UPDATE_TOPOLOGY=1 cargo test -p townhall-domain --test topology
```

Every (state, input) pair below has an entry. A pair that is absent from the graph is a transition that **does not exist** — not one refused at runtime. That is the difference between a rule a caller can argue with and a path that was never wired, and it is what `Undefined` means throughout this codebase.

Three doors, three arrow styles: intent, externally verified reality, runtime fact. A proposer's vocabulary reaches only the first.

```mermaid
stateDiagram-v2
    Draft
    VenueSelected
    NeedsRevalidation
    AwaitingBooking
    BookingInProgress
    CancellationRequested
    Booked
    CancellingBooking
    Cancelled
    NeedsHuman
    Draft --> VenueSelected : SelectVenue
    Draft --> Cancelled : Cancel
    VenueSelected --> AwaitingBooking : VerifySlot
    VenueSelected --> Draft : ChangeVenue
    VenueSelected --> NeedsRevalidation : UpdateRequirements
    VenueSelected --> Cancelled : Cancel
    NeedsRevalidation --> Draft : ChangeVenue
    NeedsRevalidation --> VenueSelected : RevalidateVenue
    NeedsRevalidation --> Cancelled : Cancel
    AwaitingBooking --> Draft : ChangeVenue
    AwaitingBooking --> NeedsRevalidation : UpdateRequirements
    AwaitingBooking --> BookingInProgress : Book ⇗
    AwaitingBooking --> Cancelled : Cancel
    Booked --> CancellingBooking : Cancel ⇗
    BookingInProgress -.-> Booked : BookingExists
    BookingInProgress -.-> AwaitingBooking : EffectAbsent
    BookingInProgress -.-> AwaitingBooking : ProviderRejected
    CancellationRequested -.-> CancellingBooking : BookingExists ⇗
    CancellationRequested -.-> Cancelled : EffectAbsent
    CancellationRequested -.-> Cancelled : ProviderRejected
    CancellingBooking -.-> Booked : EffectAbsent
    CancellingBooking -.-> Cancelled : CancellationExists
    CancellingBooking -.-> Booked : ProviderRejected
    BookingInProgress ==> NeedsHuman : ReconciliationExhausted
    CancellationRequested ==> NeedsHuman : ReconciliationExhausted
    CancellingBooking ==> NeedsHuman : ReconciliationExhausted
```

`⇗` marks an edge that asks the outside world for something, so it commits an in-flight state first and settles later on verified evidence (ADR-014).

## The proposal door

| from | SelectVenue | VerifySlot | ChangeVenue | UpdateRequirements | RevalidateVenue | Book | Cancel |
|---|---|---|---|---|---|---|---|
| **Draft** | → VenueSelected | — | — | — | — | — | → Cancelled |
| **VenueSelected** | — | → AwaitingBooking | → Draft | → NeedsRevalidation | — | — | → Cancelled |
| **NeedsRevalidation** | — | — | → Draft | — | → VenueSelected | — | → Cancelled |
| **AwaitingBooking** | — | — | → Draft | → NeedsRevalidation | — | → BookingInProgress ⇗ Book | → Cancelled |
| **BookingInProgress** | — | — | — | — | — | — | — |
| **CancellationRequested** | — | — | — | — | — | — | — |
| **Booked** | — | — | — | — | — | — | → CancellingBooking ⇗ CancelBooking |
| **CancellingBooking** | — | — | — | — | — | — | — |
| **Cancelled** | — | — | — | — | — | — | — |
| **NeedsHuman** | — | — | — | — | — | — | — |

## The fact door

| from | BookingExists | EffectAbsent | CancellationExists | ProviderRejected |
|---|---|---|---|---|
| **Draft** | — | — | — | — |
| **VenueSelected** | — | — | — | — |
| **NeedsRevalidation** | — | — | — | — |
| **AwaitingBooking** | guarded (the evidence contradicts a durable determination already recorded) | guarded (the evidence contradicts a durable determination already recorded) | guarded (the evidence's kind and the effect's kind disagree) | guarded (the evidence contradicts a durable determination already recorded) |
| **BookingInProgress** | → Booked | → AwaitingBooking | guarded (the evidence's kind and the effect's kind disagree) | → AwaitingBooking |
| **CancellationRequested** | → CancellingBooking ⇗ CancelBooking | → Cancelled | guarded (the evidence's kind and the effect's kind disagree) | → Cancelled |
| **Booked** | guarded (the evidence contradicts a durable determination already recorded) | guarded (the evidence contradicts a durable determination already recorded) | guarded (the evidence's kind and the effect's kind disagree) | guarded (the evidence contradicts a durable determination already recorded) |
| **CancellingBooking** | guarded (the evidence's kind and the effect's kind disagree) | → Booked | → Cancelled | → Booked |
| **Cancelled** | guarded (the evidence contradicts a durable determination already recorded) | guarded (the evidence contradicts a durable determination already recorded) | guarded (the evidence's kind and the effect's kind disagree) | guarded (the evidence contradicts a durable determination already recorded) |
| **NeedsHuman** | — | — | — | — |

## The system_event door

| from | ReconciliationExhausted |
|---|---|
| **Draft** | — |
| **VenueSelected** | — |
| **NeedsRevalidation** | — |
| **AwaitingBooking** | — |
| **BookingInProgress** | → NeedsHuman |
| **CancellationRequested** | → NeedsHuman |
| **Booked** | — |
| **CancellingBooking** | → NeedsHuman |
| **Cancelled** | — |
| **NeedsHuman** | — |

`—` is no edge. `guarded` means the edge exists and this export's data did not satisfy it, so its destination depends on the data and is deliberately not drawn. `converged` means authoritative state already reflected the input, which is success rather than a refusal — a reconciler re-applies facts by design.
