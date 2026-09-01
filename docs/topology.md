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
    BookingInProgress --> CancellationRequested : Cancel
    Booked --> CancellingBooking : Cancel ⇗
    BookingInProgress -.-> Booked : BookingExists · intent Book Prepared
    BookingInProgress -.-> AwaitingBooking : EffectAbsent · intent Book Prepared
    BookingInProgress -.-> AwaitingBooking : ProviderRejected · intent Book Prepared
    BookingInProgress -.-> Booked : BookingExists · intent Book Unknown
    BookingInProgress -.-> AwaitingBooking : EffectAbsent · intent Book Unknown
    BookingInProgress -.-> AwaitingBooking : ProviderRejected · intent Book Unknown
    BookingInProgress -.-> AwaitingBooking : EffectAbsent · intent Book Rejected
    BookingInProgress -.-> AwaitingBooking : ProviderRejected · intent Book Rejected
    BookingInProgress -.-> AwaitingBooking : EffectAbsent · intent Book Absent
    BookingInProgress -.-> AwaitingBooking : ProviderRejected · intent Book Absent
    CancellationRequested -.-> CancellingBooking : BookingExists · intent Book Prepared ⇗
    CancellationRequested -.-> Cancelled : EffectAbsent · intent Book Prepared
    CancellationRequested -.-> Cancelled : ProviderRejected · intent Book Prepared
    CancellationRequested -.-> CancellingBooking : BookingExists · intent Book Unknown ⇗
    CancellationRequested -.-> Cancelled : EffectAbsent · intent Book Unknown
    CancellationRequested -.-> Cancelled : ProviderRejected · intent Book Unknown
    CancellationRequested -.-> Cancelled : EffectAbsent · intent Book Rejected
    CancellationRequested -.-> Cancelled : ProviderRejected · intent Book Rejected
    CancellationRequested -.-> Cancelled : EffectAbsent · intent Book Absent
    CancellationRequested -.-> Cancelled : ProviderRejected · intent Book Absent
```

`⇗` marks an edge that asks the outside world for something, so it commits an in-flight state first and settles later on verified evidence (ADR-014).

## The proposal door

**A fixed table**, over `state x proposal`. Every cell is decided by the state and the input alone — nothing else is in reach — so this enumeration is complete and no input sequence can reach a cell nobody specified. This is where the safety claim lives, and it is the door an untrusted proposer can reach.

| from | SelectVenue | VerifySlot | ChangeVenue | UpdateRequirements | RevalidateVenue | Book | Cancel |
|---|---|---|---|---|---|---|---|
| **Draft** | → VenueSelected | — | — | — | — | — | → Cancelled |
| **VenueSelected** | — | → AwaitingBooking | → Draft | → NeedsRevalidation | — | — | → Cancelled |
| **NeedsRevalidation** | — | — | → Draft | — | → VenueSelected | — | → Cancelled |
| **AwaitingBooking** | — | — | → Draft | → NeedsRevalidation | — | → BookingInProgress ⇗ Book | → Cancelled |
| **BookingInProgress** | — | — | — | — | — | — | → CancellationRequested |
| **CancellationRequested** | — | — | — | — | — | — | — |
| **Booked** | — | — | — | — | — | — | → CancellingBooking ⇗ CancelBooking |
| **CancellingBooking** | — | — | — | — | — | — | — |
| **Cancelled** | — | — | — | — | — | — | — |
| **NeedsHuman** | — | — | — | — | — | — | — |

## The fact door

**Not a fixed table.** This door reads the *persisted intent*, so its axes are `state x fact x intent kind x intent status` — the same fact means different things depending on what was in flight, which is ADR-012 working as designed rather than a wrinkle. Every axis above is varied below, but do not read this as a combinational table: it is a reachability view.

Only the edges are listed. A pair absent from this list has no edge, and there are too many columns for a matrix to be readable.

- **AwaitingBooking** on `EffectAbsent · intent Book Rejected` converged
- **AwaitingBooking** on `ProviderRejected · intent Book Rejected` converged
- **AwaitingBooking** on `EffectAbsent · intent Book Absent` converged
- **AwaitingBooking** on `ProviderRejected · intent Book Absent` converged
- **BookingInProgress** on `BookingExists · intent Book Prepared` → Booked
- **BookingInProgress** on `EffectAbsent · intent Book Prepared` → AwaitingBooking
- **BookingInProgress** on `ProviderRejected · intent Book Prepared` → AwaitingBooking
- **BookingInProgress** on `BookingExists · intent Book Unknown` → Booked
- **BookingInProgress** on `EffectAbsent · intent Book Unknown` → AwaitingBooking
- **BookingInProgress** on `ProviderRejected · intent Book Unknown` → AwaitingBooking
- **BookingInProgress** on `EffectAbsent · intent Book Rejected` → AwaitingBooking
- **BookingInProgress** on `ProviderRejected · intent Book Rejected` → AwaitingBooking
- **BookingInProgress** on `EffectAbsent · intent Book Absent` → AwaitingBooking
- **BookingInProgress** on `ProviderRejected · intent Book Absent` → AwaitingBooking
- **CancellationRequested** on `BookingExists · intent Book Prepared` → CancellingBooking ⇗ CancelBooking
- **CancellationRequested** on `EffectAbsent · intent Book Prepared` → Cancelled
- **CancellationRequested** on `ProviderRejected · intent Book Prepared` → Cancelled
- **CancellationRequested** on `BookingExists · intent Book Unknown` → CancellingBooking ⇗ CancelBooking
- **CancellationRequested** on `EffectAbsent · intent Book Unknown` → Cancelled
- **CancellationRequested** on `ProviderRejected · intent Book Unknown` → Cancelled
- **CancellationRequested** on `EffectAbsent · intent Book Rejected` → Cancelled
- **CancellationRequested** on `ProviderRejected · intent Book Rejected` → Cancelled
- **CancellationRequested** on `EffectAbsent · intent Book Absent` → Cancelled
- **CancellationRequested** on `ProviderRejected · intent Book Absent` → Cancelled
- **Booked** on `EffectAbsent · intent Cancel Rejected` converged
- **Booked** on `ProviderRejected · intent Cancel Rejected` converged
- **Booked** on `EffectAbsent · intent Cancel Absent` converged
- **Booked** on `ProviderRejected · intent Cancel Absent` converged
- **Cancelled** on `EffectAbsent · intent Book Rejected` converged
- **Cancelled** on `ProviderRejected · intent Book Rejected` converged
- **Cancelled** on `EffectAbsent · intent Book Absent` converged
- **Cancelled** on `ProviderRejected · intent Book Absent` converged

## The system_event door

**A fixed table**, over `state x event`. Every cell is decided by the state and the input alone — nothing else is in reach — so this enumeration is complete and no input sequence can reach a cell nobody specified. This is where the safety claim lives, and it is the door an untrusted proposer can reach.

| from | ReconciliationExhausted |
|---|---|
| **Draft** | — |
| **VenueSelected** | — |
| **NeedsRevalidation** | — |
| **AwaitingBooking** | — |
| **BookingInProgress** | records ⏺ |
| **CancellationRequested** | records ⏺ |
| **Booked** | — |
| **CancellingBooking** | records ⏺ |
| **Cancelled** | — |
| **NeedsHuman** | — |

`—` is no edge. `guarded` means the edge exists and this export's data did not satisfy it, so its destination depends on the data and is deliberately not drawn. `converged` means authoritative state already reflected the input, which is success rather than a refusal — a reconciler re-applies facts by design.
