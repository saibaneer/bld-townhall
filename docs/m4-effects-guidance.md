# M4 Guidance — External Effects, Stable Identity and Reconciliation

Status: **implementation guidance only**. Do not mark M4 complete until its acceptance gate passes.

M4 is the first milestone where the BLD process can change an external world. That makes it the point where a naive `call provider -> save result` implementation becomes unsafe.

The required principle is:

> Persist the intended consequence before attempting it; execute it under a stable identity; then reconcile authoritative evidence back into the workflow.

## M4 acceptance gate

M4 is accepted only when this failure is handled correctly:

1. BLD persists one canonical booking intent.
2. The mock council creates the booking.
3. The mock council intentionally drops the response.
4. The local process therefore does not know whether the booking happened.
5. Retry/recovery uses the **same `EffectIntentId`**.
6. The mock council returns or exposes the original booking rather than creating another.
7. Reconciliation obtains authoritative evidence.
8. Local state converges to exactly one correct `Booked` outcome.

If that scenario can duplicate a booking, M4 is not complete.

## The two doors (ADR-012) — settle this before writing code

M4 is where the single-vocabulary design breaks. Every exit from `BookingInProgress`,
`CancellationRequested` and `CancellingBooking` is an *evidence* outcome, not a request.

```text
INTENT                              OBSERVATION
Lucy or the agent asks              only verified evidence drives

BookingInProgress + cancel          BookingInProgress + booking_confirmed -> Booked
                                    BookingInProgress + booking_failed    -> AwaitingBooking
                                    CancellationRequested + booking_found -> CancellingBooking
                                    CancellationRequested + no_booking_found -> Cancelled
                                    CancellingBooking + cancellation_confirmed -> Cancelled
                                    CancellingBooking + cancellation_failed    -> Booked
                                    * + reconciliation_failed              -> NeedsHuman
```

Adding the observation edges to `BookingProposal` would let a hostile proposer submit
`BookingConfirmed` and reach `Booked` with no council call. M4 must therefore add:

```rust
enum BookingObservation {
    BookingConfirmed(VerifiedBookingEvidence),
    BookingFailed(VerifiedBookingFailure),
    BookingNotFound(VerifiedNoBookingEvidence),
    CancellationConfirmed(VerifiedCancellationEvidence),
    CancellationFailed(VerifiedCancellationFailure),
    ReconciliationFailed(VerifiedReconciliationFailure),
}

apply_observation(state, verified_observation) -> Result<NextState, Error>
```

Only an adapter that has verified the raw response and bound it to the expected
`EffectIntentId`, venue, slot and principal may construct one of these. A raw council
response is not admissible on its own.

`BookingProposal::Reconcile` is **removed** in M4. Reconciliation is runtime recovery, not
a user intention — it must run when the model is offline, hostile or absent. Removing it
takes the proposal vocabulary from 8 variants to 7 and the topology matrix in
`townhall-domain` from 80 cells to 70; update `LOCKED` in the same commit.

`BookingInProgress + Cancel` is the one intent edge that lands here too — it is currently
in the matrix's `PENDING` table, deferred because `CancellationRequested` had no exit.
The observation door gives it one.

## Commit before calling (ADR-013)

The `resolve -> execute -> validate` pipeline conflates requesting an effect with learning
its result. Harmless with M2's synchronous fake; not harmless with a real council.

```text
AwaitingBooking v3
    -> resolve Book, derive canonical plan
    -> persist EffectIntentId + canonical plan
    -> COMMIT BookingInProgress v4          <-- durable BEFORE any external call
    -> call the council
    -> raw response / timeout / lost response
    -> adapter verifies and binds -> VerifiedBookingEvidence
    -> apply_observation -> COMMIT Booked v5
```

Crash anywhere after the first commit and recovery finds `BookingInProgress` plus its
`EffectIntentId`, which is exactly what the reconciler needs. Crash before it and no
external call was ever made.

This is also why the repository's prepare/finalize methods must **return committed
state**: it leaves no signature through which a capability could be invoked while a
transaction is open. Since `commit` now uses `BEGIN IMMEDIATE`, a transaction held across
a council call would block every unrelated booking for the busy timeout.

## Do not hold a database transaction across a network call

The external provider and SQLite cannot participate in one atomic transaction. Holding the SQLite transaction open while making an HTTP call creates lock contention and still does not make the provider call atomic with the database.

M4 should instead use three explicit phases.

## Phase A — Prepare and persist intent

Within a short database transaction:

```text
load booking @ version N
    ↓
resolve Book behaviour
    ↓
derive canonical BookingPlan from authoritative state
    ↓
derive stable EffectIntentId
    ↓
insert effect_intents row if absent
    ↓
commit BookingInProgress / active_effect
    ↓
version N -> N+1
    ↓
audit prepared effect
```

Nothing external has been called yet.

Suggested M4 table:

```sql
CREATE TABLE effect_intents (
    effect_intent_id    TEXT PRIMARY KEY,
    booking_id          TEXT NOT NULL,
    effect_kind         TEXT NOT NULL,
    canonical_plan_json TEXT NOT NULL,
    plan_hash           TEXT NOT NULL,
    status              TEXT NOT NULL,
    provider_reference  TEXT,
    last_error          TEXT,
    created_at_ms       INTEGER NOT NULL,
    updated_at_ms       INTEGER NOT NULL,
    FOREIGN KEY (booking_id) REFERENCES bookings(id)
);
```

The exact schema can evolve, but the stable effect identity and durable plan cannot be optional.

## Phase B — Execute outside the transaction

The capability adapter receives the canonical plan, not model instructions:

```text
BookingCapability.execute(canonical_plan, effect_intent_id)
```

The mock council must implement provider-side idempotency:

```text
first request with BOOK-BKG-1001-1
    -> create TH-92718

retry with BOOK-BKG-1001-1
    -> return TH-92718
    -> DO NOT create another booking
```

`RequestId` must never be used as this identity because a transport retry receives a new request ID.

## Phase C — Observe, validate and converge

There are three broad outcomes.

### Confirmed success

```text
provider evidence
    ↓
validate evidence against persisted canonical plan
    ↓
CAS BookingInProgress -> Booked
    ↓
clear/complete active effect
    ↓
mark effect Confirmed
```

### Confirmed failure

If the provider authoritatively says no booking was created, record the failure and transition according to domain policy, for example back to `AwaitingBooking`.

### Ambiguous / unavailable

Timeout is **not failure** and is **not success**.

Keep an explicit unresolved state and enqueue reconciliation:

```text
BookingInProgress
    + effect intent = Pending/Unknown
    ↓
reconciliation worker/provider lookup
```

Never create a second effect identity merely because the first response was lost.

## Reconciliation capability

The mock council needs a lookup surface keyed by effect identity, not only by a provider-generated booking reference:

```text
GET /effects/{effect_intent_id}
```

Possible authoritative results:

```text
NotFound
Booked(reference, canonical facts)
Cancelled(reference)
Unavailable / Unknown
```

Only externally grounded results may advance the workflow.

## Cancellation while booking is in flight

M4 must preserve the state distinction discussed in the spec:

```text
BookingInProgress + Cancel
    -> CancellationRequested
```

The cancellation proposal does not kill a thread or pretend the provider call never happened.

Reconciliation asks whether the booking exists:

```text
no booking exists
    -> Cancelled

booking exists
    -> CancellingBooking
    -> cancellation capability
    -> verified cancellation evidence
    -> Cancelled
```

This is a compensation protocol, not history rewriting.

## Effect identity requirements

An `EffectIntentId` must identify the intended consequence, not the attempt.

It should remain stable across:

- HTTP retries;
- process restart;
- model replacement;
- different `RequestId`s;
- timeout recovery;
- reconciliation workers.

It should not include volatile attempt metadata such as timestamp, model output, request ID, or approver identity unless that data is genuinely part of the business identity of the intended effect.

## Canonical plan requirements

Persist the plan used to create the effect. At minimum it should bind:

- booking/resource ID;
- principal;
- venue ID;
- slot ID;
- attendees;
- authoritative fee;
- effect identity.

Later evidence must be validated against this persisted plan. Do not reconstruct consequential parameters from conversation history during recovery.

## Suggested crate boundary

M4 should add:

```text
crates/council-client/
services/mock-council/
```

Keep `townhall-domain` free of HTTP. The domain defines plans/evidence semantics; `council-client` implements capability adapters; `mock-council` owns the simulated external truth.

A useful dependency direction is:

```text
townhall-domain
      ↑
townhall-store
      ↑
council-client

mock-council = separate process/service
```

Do not let the Rig agent, future Axum handler, or repository receive raw council mutation access.

## Required failure-injection tests

M4 is primarily a recovery milestone. Add deterministic tests for:

1. crash/failure before provider call — intent exists, provider has nothing;
2. provider rejects before effect — no booking exists;
3. provider commits then response is dropped — reconciliation finds one booking;
4. retry after dropped response — same effect identity returns original result;
5. process restart between provider commit and local evidence commit;
6. malformed or field-perfect forged evidence — validation rejects it;
7. provider lookup unavailable — workflow remains unknown/in-progress;
8. two workers attempt the same pending effect — provider still creates one booking;
9. cancellation arrives while booking outcome is ambiguous;
10. reconciliation discovers a provider effect that local state has not yet adopted.

## M4 implementation order

Do this in order:

1. Add durable `effect_intents` schema + repository API.
2. Refactor `Book` so the boundary derives a deterministic canonical plan without executing a fake booking inside `TownHallDomain`.
3. Add `BookingInProgress` prepare transition and persist-before-effect coordinator.
4. Build an in-process fake capability proving effect identity/idempotency semantics.
5. Build the separate mock-council service.
6. Add council-client adapter.
7. Add fault injection.
8. Add reconciliation loop/API.
9. Add in-flight cancellation/compensation tests.
10. Only after the recovery suite passes, start M5/Axum BLD API work.

## Stop conditions for the implementation agent

Stop and raise an architecture question instead of improvising if any implementation requires:

- the model to choose `EffectIntentId`;
- a network call while a DB transaction is held open;
- treating timeout as a definitive provider failure;
- creating a new intent on every retry;
- accepting provider-shaped/model-shaped evidence without authoritative lookup or trusted adapter provenance;
- allowing cancellation to erase an already possible external effect;
- bypassing M3 CAS/version semantics.
