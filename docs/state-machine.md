# Town Hall State Machine

The initial state vocabulary follows the technical specification.

```mermaid
stateDiagram-v2
    [*] --> Draft
    Draft --> VenueSelected: SelectVenue
    Draft --> Cancelled: Cancel

    VenueSelected --> AwaitingBooking: VerifySlot
    VenueSelected --> Draft: ChangeVenue
    VenueSelected --> NeedsRevalidation: UpdateRequirements
    VenueSelected --> Cancelled: Cancel

    NeedsRevalidation --> VenueSelected: RevalidateVenue
    NeedsRevalidation --> Draft: ChangeVenue
    NeedsRevalidation --> Cancelled: Cancel

    AwaitingBooking --> BookingInProgress: Book
    AwaitingBooking --> Draft: ChangeVenue
    AwaitingBooking --> NeedsRevalidation: UpdateRequirements
    AwaitingBooking --> Cancelled: Cancel

    BookingInProgress --> Booked: BookingConfirmed
    BookingInProgress --> AwaitingBooking: BookingFailed
    BookingInProgress --> CancellationRequested: Cancel

    CancellationRequested --> Cancelled: NoBookingFound
    CancellationRequested --> CancellingBooking: BookingFound
    CancellationRequested --> NeedsHuman: ReconciliationFailed

    Booked --> CancellingBooking: Cancel
    CancellingBooking --> Cancelled: CancellationConfirmed
    CancellingBooking --> Booked: CancellationFailed
    CancellingBooking --> NeedsHuman: ReconciliationFailed

    Cancelled --> [*]
    NeedsHuman --> [*]
```

## State-scoped behaviour rule

Concrete state types own only behaviours that exist in that state. For example `Draft` can select a venue or cancel, but cannot book. Runtime enum dispatch maps an absent state/proposal pairing to `Resolution::Undefined`.
