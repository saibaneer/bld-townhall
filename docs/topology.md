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
    VerifyingSlot
    AwaitingBooking
    OfferSelected
    CheckoutPrepared
    AwaitingHumanPayment
    PaymentConfirmed
    BookingInProgress
    PaidBookingInProgress
    CancellationRequested
    Booked
    CancellingBooking
    Cancelled
    NeedsHuman
    Draft --> VenueSelected : SelectVenue
    Draft --> Cancelled : Cancel
    VenueSelected --> VerifyingSlot : VerifySlot ⇗
    VenueSelected --> Draft : ChangeVenue
    VenueSelected --> NeedsRevalidation : UpdateRequirements
    VenueSelected --> Cancelled : Cancel
    NeedsRevalidation --> Draft : ChangeVenue
    NeedsRevalidation --> VerifyingSlot : RevalidateVenue ⇗
    NeedsRevalidation --> Cancelled : Cancel
    AwaitingBooking --> Draft : ChangeVenue
    AwaitingBooking --> NeedsRevalidation : UpdateRequirements
    AwaitingBooking --> BookingInProgress : Book ⇗
    AwaitingBooking --> Cancelled : Cancel
    OfferSelected --> Draft : ChangeVenue
    OfferSelected --> NeedsRevalidation : UpdateRequirements
    OfferSelected --> CheckoutPrepared : Book ⇗
    OfferSelected --> Cancelled : Cancel
    CheckoutPrepared --> Cancelled : Cancel
    AwaitingHumanPayment --> Cancelled : Cancel
    BookingInProgress --> CancellationRequested : Cancel
    Booked --> CancellingBooking : Cancel ⇗
    VerifyingSlot -.-> VenueSelected : EffectAbsent · intent Verify Prepared
    VerifyingSlot -.-> VenueSelected : ProviderRejected · intent Verify Prepared
    VerifyingSlot -.-> AwaitingBooking : AvailabilityVerified · intent Verify Prepared
    VerifyingSlot -.-> VenueSelected : EffectAbsent · intent Verify Unknown
    VerifyingSlot -.-> VenueSelected : ProviderRejected · intent Verify Unknown
    VerifyingSlot -.-> AwaitingBooking : AvailabilityVerified · intent Verify Unknown
    VerifyingSlot -.-> AwaitingBooking : AvailabilityVerified · intent Verify Confirmed
    VerifyingSlot -.-> VenueSelected : EffectAbsent · intent Verify Rejected
    VerifyingSlot -.-> VenueSelected : ProviderRejected · intent Verify Rejected
    VerifyingSlot -.-> VenueSelected : EffectAbsent · intent Verify Absent
    VerifyingSlot -.-> VenueSelected : ProviderRejected · intent Verify Absent
    CheckoutPrepared -.-> OfferSelected : EffectAbsent · intent Pay Prepared
    CheckoutPrepared -.-> OfferSelected : ProviderRejected · intent Pay Prepared
    CheckoutPrepared -.-> AwaitingHumanPayment : SessionCreated · intent Pay Prepared ⇗
    CheckoutPrepared -.-> OfferSelected : EffectAbsent · intent Pay Unknown
    CheckoutPrepared -.-> OfferSelected : ProviderRejected · intent Pay Unknown
    CheckoutPrepared -.-> AwaitingHumanPayment : SessionCreated · intent Pay Unknown ⇗
    CheckoutPrepared -.-> AwaitingHumanPayment : SessionCreated · intent Pay Confirmed ⇗
    CheckoutPrepared -.-> OfferSelected : EffectAbsent · intent Pay Rejected
    CheckoutPrepared -.-> OfferSelected : ProviderRejected · intent Pay Rejected
    CheckoutPrepared -.-> OfferSelected : EffectAbsent · intent Pay Absent
    CheckoutPrepared -.-> OfferSelected : ProviderRejected · intent Pay Absent
    AwaitingHumanPayment -.-> OfferSelected : EffectAbsent · intent Pay Prepared
    AwaitingHumanPayment -.-> OfferSelected : ProviderRejected · intent Pay Prepared
    AwaitingHumanPayment -.-> PaidBookingInProgress : PaymentConfirmed · intent Pay Prepared ⇗
    AwaitingHumanPayment -.-> OfferSelected : PaymentAbandoned · intent Pay Prepared
    AwaitingHumanPayment -.-> OfferSelected : EffectAbsent · intent Pay Unknown
    AwaitingHumanPayment -.-> OfferSelected : ProviderRejected · intent Pay Unknown
    AwaitingHumanPayment -.-> PaidBookingInProgress : PaymentConfirmed · intent Pay Unknown ⇗
    AwaitingHumanPayment -.-> OfferSelected : PaymentAbandoned · intent Pay Unknown
    AwaitingHumanPayment -.-> PaidBookingInProgress : PaymentConfirmed · intent Pay Confirmed ⇗
    AwaitingHumanPayment -.-> OfferSelected : EffectAbsent · intent Pay Rejected
    AwaitingHumanPayment -.-> OfferSelected : ProviderRejected · intent Pay Rejected
    AwaitingHumanPayment -.-> OfferSelected : PaymentAbandoned · intent Pay Rejected
    AwaitingHumanPayment -.-> OfferSelected : EffectAbsent · intent Pay Absent
    AwaitingHumanPayment -.-> OfferSelected : ProviderRejected · intent Pay Absent
    AwaitingHumanPayment -.-> OfferSelected : PaymentAbandoned · intent Pay Absent
    BookingInProgress -.-> Booked : BookingExists · intent Book Prepared
    BookingInProgress -.-> AwaitingBooking : EffectAbsent · intent Book Prepared
    BookingInProgress -.-> AwaitingBooking : ProviderRejected · intent Book Prepared
    BookingInProgress -.-> Booked : BookingExists · intent Book Unknown
    BookingInProgress -.-> AwaitingBooking : EffectAbsent · intent Book Unknown
    BookingInProgress -.-> AwaitingBooking : ProviderRejected · intent Book Unknown
    BookingInProgress -.-> Booked : BookingExists · intent Book Confirmed
    BookingInProgress -.-> AwaitingBooking : EffectAbsent · intent Book Rejected
    BookingInProgress -.-> AwaitingBooking : ProviderRejected · intent Book Rejected
    BookingInProgress -.-> AwaitingBooking : EffectAbsent · intent Book Absent
    BookingInProgress -.-> AwaitingBooking : ProviderRejected · intent Book Absent
    PaidBookingInProgress -.-> Booked : BookingExists · intent Book Prepared
    PaidBookingInProgress -.-> NeedsHuman : EffectAbsent · intent Book Prepared
    PaidBookingInProgress -.-> NeedsHuman : ProviderRejected · intent Book Prepared
    PaidBookingInProgress -.-> Booked : BookingExists · intent Book Unknown
    PaidBookingInProgress -.-> NeedsHuman : EffectAbsent · intent Book Unknown
    PaidBookingInProgress -.-> NeedsHuman : ProviderRejected · intent Book Unknown
    PaidBookingInProgress -.-> Booked : BookingExists · intent Book Confirmed
    PaidBookingInProgress -.-> NeedsHuman : EffectAbsent · intent Book Rejected
    PaidBookingInProgress -.-> NeedsHuman : ProviderRejected · intent Book Rejected
    PaidBookingInProgress -.-> NeedsHuman : EffectAbsent · intent Book Absent
    PaidBookingInProgress -.-> NeedsHuman : ProviderRejected · intent Book Absent
    CancellationRequested -.-> CancellingBooking : BookingExists · intent Book Prepared ⇗
    CancellationRequested -.-> Cancelled : EffectAbsent · intent Book Prepared
    CancellationRequested -.-> Cancelled : ProviderRejected · intent Book Prepared
    CancellationRequested -.-> CancellingBooking : BookingExists · intent Book Unknown ⇗
    CancellationRequested -.-> Cancelled : EffectAbsent · intent Book Unknown
    CancellationRequested -.-> Cancelled : ProviderRejected · intent Book Unknown
    CancellationRequested -.-> CancellingBooking : BookingExists · intent Book Confirmed ⇗
    CancellationRequested -.-> Cancelled : EffectAbsent · intent Book Rejected
    CancellationRequested -.-> Cancelled : ProviderRejected · intent Book Rejected
    CancellationRequested -.-> Cancelled : EffectAbsent · intent Book Absent
    CancellationRequested -.-> Cancelled : ProviderRejected · intent Book Absent
    CancellingBooking -.-> Booked : EffectAbsent · intent Cancel Prepared
    CancellingBooking -.-> Cancelled : CancellationExists · intent Cancel Prepared
    CancellingBooking -.-> Booked : ProviderRejected · intent Cancel Prepared
    CancellingBooking -.-> Booked : EffectAbsent · intent Cancel Unknown
    CancellingBooking -.-> Cancelled : CancellationExists · intent Cancel Unknown
    CancellingBooking -.-> Booked : ProviderRejected · intent Cancel Unknown
    CancellingBooking -.-> Cancelled : CancellationExists · intent Cancel Confirmed
    CancellingBooking -.-> Booked : EffectAbsent · intent Cancel Rejected
    CancellingBooking -.-> Booked : ProviderRejected · intent Cancel Rejected
    CancellingBooking -.-> Booked : EffectAbsent · intent Cancel Absent
    CancellingBooking -.-> Booked : ProviderRejected · intent Cancel Absent
```

`⇗` marks an edge that asks the outside world for something, so it commits an in-flight state first and settles later on verified evidence (ADR-014).

## The proposal door

**A fixed table**, over `state x proposal`. Every cell is decided by the state and the input alone — nothing else is in reach — so this enumeration is complete and no input sequence can reach a cell nobody specified. This is where the safety claim lives, and it is the door an untrusted proposer can reach.

| from | SelectVenue | VerifySlot | ChangeVenue | UpdateRequirements | RevalidateVenue | Book | Cancel |
|---|---|---|---|---|---|---|---|
| **Draft** | → VenueSelected | — | — | — | — | — | → Cancelled |
| **VenueSelected** | — | → VerifyingSlot ⇗ VerifyAvailability | → Draft | → NeedsRevalidation | — | — | → Cancelled |
| **NeedsRevalidation** | — | — | → Draft | — | → VerifyingSlot ⇗ VerifyAvailability | — | → Cancelled |
| **VerifyingSlot** | — | — | — | — | — | — | — |
| **AwaitingBooking** | — | — | → Draft | → NeedsRevalidation | — | → BookingInProgress ⇗ Book | → Cancelled |
| **OfferSelected** | — | — | → Draft | → NeedsRevalidation | — | → CheckoutPrepared ⇗ PreparePayment | → Cancelled |
| **CheckoutPrepared** | — | — | — | — | — | — | → Cancelled |
| **AwaitingHumanPayment** | — | — | — | — | — | — | → Cancelled |
| **PaymentConfirmed** | — | — | — | — | — | — | — |
| **BookingInProgress** | — | — | — | — | — | — | → CancellationRequested |
| **PaidBookingInProgress** | — | — | — | — | — | — | — |
| **CancellationRequested** | — | — | — | — | — | — | — |
| **Booked** | — | — | — | — | — | — | → CancellingBooking ⇗ CancelBooking |
| **CancellingBooking** | — | — | — | — | — | — | — |
| **Cancelled** | — | — | — | — | — | — | — |
| **NeedsHuman** | — | — | — | — | — | — | — |

## The fact door

**Not a fixed table.** This door reads the *persisted intent*, so its axes are `state x fact x intent kind x intent status` — the same fact means different things depending on what was in flight, which is ADR-012 working as designed rather than a wrinkle. Every axis above is varied below, but do not read this as a combinational table: it is a reachability view.

Only the edges are listed. A pair absent from this list has no edge, and there are too many columns for a matrix to be readable.

- **VerifyingSlot** on `EffectAbsent · intent Verify Prepared` → VenueSelected
- **VerifyingSlot** on `ProviderRejected · intent Verify Prepared` → VenueSelected
- **VerifyingSlot** on `AvailabilityVerified · intent Verify Prepared` → AwaitingBooking
- **VerifyingSlot** on `EffectAbsent · intent Verify Unknown` → VenueSelected
- **VerifyingSlot** on `ProviderRejected · intent Verify Unknown` → VenueSelected
- **VerifyingSlot** on `AvailabilityVerified · intent Verify Unknown` → AwaitingBooking
- **VerifyingSlot** on `AvailabilityVerified · intent Verify Confirmed` → AwaitingBooking
- **VerifyingSlot** on `EffectAbsent · intent Verify Rejected` → VenueSelected
- **VerifyingSlot** on `ProviderRejected · intent Verify Rejected` → VenueSelected
- **VerifyingSlot** on `EffectAbsent · intent Verify Absent` → VenueSelected
- **VerifyingSlot** on `ProviderRejected · intent Verify Absent` → VenueSelected
- **AwaitingBooking** on `EffectAbsent · intent Book Rejected` converged
- **AwaitingBooking** on `ProviderRejected · intent Book Rejected` converged
- **AwaitingBooking** on `EffectAbsent · intent Book Absent` converged
- **AwaitingBooking** on `ProviderRejected · intent Book Absent` converged
- **AwaitingBooking** on `AvailabilityVerified · intent Verify Confirmed` converged
- **CheckoutPrepared** on `EffectAbsent · intent Pay Prepared` → OfferSelected
- **CheckoutPrepared** on `ProviderRejected · intent Pay Prepared` → OfferSelected
- **CheckoutPrepared** on `SessionCreated · intent Pay Prepared` → AwaitingHumanPayment ⇗ PreparePayment
- **CheckoutPrepared** on `EffectAbsent · intent Pay Unknown` → OfferSelected
- **CheckoutPrepared** on `ProviderRejected · intent Pay Unknown` → OfferSelected
- **CheckoutPrepared** on `SessionCreated · intent Pay Unknown` → AwaitingHumanPayment ⇗ PreparePayment
- **CheckoutPrepared** on `SessionCreated · intent Pay Confirmed` → AwaitingHumanPayment ⇗ PreparePayment
- **CheckoutPrepared** on `EffectAbsent · intent Pay Rejected` → OfferSelected
- **CheckoutPrepared** on `ProviderRejected · intent Pay Rejected` → OfferSelected
- **CheckoutPrepared** on `EffectAbsent · intent Pay Absent` → OfferSelected
- **CheckoutPrepared** on `ProviderRejected · intent Pay Absent` → OfferSelected
- **AwaitingHumanPayment** on `EffectAbsent · intent Pay Prepared` → OfferSelected
- **AwaitingHumanPayment** on `ProviderRejected · intent Pay Prepared` → OfferSelected
- **AwaitingHumanPayment** on `PaymentConfirmed · intent Pay Prepared` → PaidBookingInProgress ⇗ Book
- **AwaitingHumanPayment** on `PaymentAbandoned · intent Pay Prepared` → OfferSelected
- **AwaitingHumanPayment** on `EffectAbsent · intent Pay Unknown` → OfferSelected
- **AwaitingHumanPayment** on `ProviderRejected · intent Pay Unknown` → OfferSelected
- **AwaitingHumanPayment** on `PaymentConfirmed · intent Pay Unknown` → PaidBookingInProgress ⇗ Book
- **AwaitingHumanPayment** on `PaymentAbandoned · intent Pay Unknown` → OfferSelected
- **AwaitingHumanPayment** on `PaymentConfirmed · intent Pay Confirmed` → PaidBookingInProgress ⇗ Book
- **AwaitingHumanPayment** on `EffectAbsent · intent Pay Rejected` → OfferSelected
- **AwaitingHumanPayment** on `ProviderRejected · intent Pay Rejected` → OfferSelected
- **AwaitingHumanPayment** on `PaymentAbandoned · intent Pay Rejected` → OfferSelected
- **AwaitingHumanPayment** on `EffectAbsent · intent Pay Absent` → OfferSelected
- **AwaitingHumanPayment** on `ProviderRejected · intent Pay Absent` → OfferSelected
- **AwaitingHumanPayment** on `PaymentAbandoned · intent Pay Absent` → OfferSelected
- **BookingInProgress** on `BookingExists · intent Book Prepared` → Booked
- **BookingInProgress** on `EffectAbsent · intent Book Prepared` → AwaitingBooking
- **BookingInProgress** on `ProviderRejected · intent Book Prepared` → AwaitingBooking
- **BookingInProgress** on `BookingExists · intent Book Unknown` → Booked
- **BookingInProgress** on `EffectAbsent · intent Book Unknown` → AwaitingBooking
- **BookingInProgress** on `ProviderRejected · intent Book Unknown` → AwaitingBooking
- **BookingInProgress** on `BookingExists · intent Book Confirmed` → Booked
- **BookingInProgress** on `EffectAbsent · intent Book Rejected` → AwaitingBooking
- **BookingInProgress** on `ProviderRejected · intent Book Rejected` → AwaitingBooking
- **BookingInProgress** on `EffectAbsent · intent Book Absent` → AwaitingBooking
- **BookingInProgress** on `ProviderRejected · intent Book Absent` → AwaitingBooking
- **PaidBookingInProgress** on `BookingExists · intent Book Prepared` → Booked
- **PaidBookingInProgress** on `EffectAbsent · intent Book Prepared` → NeedsHuman
- **PaidBookingInProgress** on `ProviderRejected · intent Book Prepared` → NeedsHuman
- **PaidBookingInProgress** on `BookingExists · intent Book Unknown` → Booked
- **PaidBookingInProgress** on `EffectAbsent · intent Book Unknown` → NeedsHuman
- **PaidBookingInProgress** on `ProviderRejected · intent Book Unknown` → NeedsHuman
- **PaidBookingInProgress** on `BookingExists · intent Book Confirmed` → Booked
- **PaidBookingInProgress** on `EffectAbsent · intent Book Rejected` → NeedsHuman
- **PaidBookingInProgress** on `ProviderRejected · intent Book Rejected` → NeedsHuman
- **PaidBookingInProgress** on `EffectAbsent · intent Book Absent` → NeedsHuman
- **PaidBookingInProgress** on `ProviderRejected · intent Book Absent` → NeedsHuman
- **CancellationRequested** on `BookingExists · intent Book Prepared` → CancellingBooking ⇗ CancelBooking
- **CancellationRequested** on `EffectAbsent · intent Book Prepared` → Cancelled
- **CancellationRequested** on `ProviderRejected · intent Book Prepared` → Cancelled
- **CancellationRequested** on `BookingExists · intent Book Unknown` → CancellingBooking ⇗ CancelBooking
- **CancellationRequested** on `EffectAbsent · intent Book Unknown` → Cancelled
- **CancellationRequested** on `ProviderRejected · intent Book Unknown` → Cancelled
- **CancellationRequested** on `BookingExists · intent Book Confirmed` → CancellingBooking ⇗ CancelBooking
- **CancellationRequested** on `EffectAbsent · intent Book Rejected` → Cancelled
- **CancellationRequested** on `ProviderRejected · intent Book Rejected` → Cancelled
- **CancellationRequested** on `EffectAbsent · intent Book Absent` → Cancelled
- **CancellationRequested** on `ProviderRejected · intent Book Absent` → Cancelled
- **Booked** on `BookingExists · intent Book Confirmed` converged
- **Booked** on `EffectAbsent · intent Cancel Rejected` converged
- **Booked** on `ProviderRejected · intent Cancel Rejected` converged
- **Booked** on `EffectAbsent · intent Cancel Absent` converged
- **Booked** on `ProviderRejected · intent Cancel Absent` converged
- **CancellingBooking** on `EffectAbsent · intent Cancel Prepared` → Booked
- **CancellingBooking** on `CancellationExists · intent Cancel Prepared` → Cancelled
- **CancellingBooking** on `ProviderRejected · intent Cancel Prepared` → Booked
- **CancellingBooking** on `EffectAbsent · intent Cancel Unknown` → Booked
- **CancellingBooking** on `CancellationExists · intent Cancel Unknown` → Cancelled
- **CancellingBooking** on `ProviderRejected · intent Cancel Unknown` → Booked
- **CancellingBooking** on `CancellationExists · intent Cancel Confirmed` → Cancelled
- **CancellingBooking** on `EffectAbsent · intent Cancel Rejected` → Booked
- **CancellingBooking** on `ProviderRejected · intent Cancel Rejected` → Booked
- **CancellingBooking** on `EffectAbsent · intent Cancel Absent` → Booked
- **CancellingBooking** on `ProviderRejected · intent Cancel Absent` → Booked
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
| **VerifyingSlot** | records ⏺ |
| **AwaitingBooking** | — |
| **OfferSelected** | — |
| **CheckoutPrepared** | records ⏺ |
| **AwaitingHumanPayment** | records ⏺ |
| **PaymentConfirmed** | — |
| **BookingInProgress** | records ⏺ |
| **PaidBookingInProgress** | records ⏺ |
| **CancellationRequested** | records ⏺ |
| **Booked** | — |
| **CancellingBooking** | records ⏺ |
| **Cancelled** | — |
| **NeedsHuman** | — |

`—` is no edge. `guarded` means the edge exists and this export's data did not satisfy it, so its destination depends on the data and is deliberately not drawn. `converged` means authoritative state already reflected the input, which is success rather than a refusal — a reconciler re-applies facts by design.
