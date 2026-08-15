# Architecture Decision Log

## ADR-001 — Kernel stays domain-agnostic

The BLD kernel owns sequencing semantics only. Town-hall policy stays in `townhall-domain`.

## ADR-002 — Domain has no HTTP/SMS/model dependency

The domain must run in unit tests with fake context/capabilities. Axum, HumanChannel and Rig are later adapters.

## ADR-003 — Proposal is not authority

Authority is passed separately from proposal data. A proposal cannot grant itself permission.

## ADR-004 — State-scoped behaviours

A state exposes only behaviours that exist there. Invalid state/proposal pairs are `Undefined`, not generic policy failures.

## ADR-005 — External effects are not implemented in M2

M2 uses fake deterministic evidence. Durable effect intents, idempotency and reconciliation arrive in M4 after persistence exists.

## ADR-006 — Open-source model is a replaceable proposer

Rig/model integration is deliberately late (M11). No model is needed to establish kernel/domain safety properties.


## ADR-007 — Durable authority lives in the database, not a process mutex

M3 uses database compare-and-set (`id + expected version`) as the serialization point. Process-local locks may be used for performance later but cannot replace the durable version check.

## ADR-008 — Repository owns version advancement

Callers provide an expected version and next business snapshot. The repository derives exactly `N + 1`; adapters may not manufacture resource versions.

## ADR-009 — Current snapshot + append-oriented local audit

`bookings` is the authoritative current aggregate snapshot. `audit_events` is append-oriented history. Both are updated in the same local SQLite transaction. This is not event sourcing and does not claim external audit anchoring.

## ADR-010 — No network effects in M3

M3 persists an optional active-effect field but does not execute provider calls. M4 must persist a stable effect intent before any external consequence and must reconcile ambiguous outcomes.

## ADR-011 — SQLite is a POC adapter, not a protocol dependency

The repository trait isolates persistence. SQLite + SQLx is selected for the POC's single-file durability and real transactions; a later Postgres adapter must preserve the same CAS and atomic-audit semantics.

## ADR-012 — Intent and evidence enter the domain through separate doors

Until now every transition arrived through one type, `BookingProposal`, which is the
vocabulary the untrusted proposer submits from. But the state machine mixes two
fundamentally different kinds of edge.

**Intent edges** are things a human or agent may *request*: `select_venue`,
`verify_slot`, `change_venue`, `update_requirements`, `revalidate_venue`, `book`,
`cancel`.

**Observation edges** are things the system *learns*: `booking_confirmed`,
`booking_failed`, `booking_found`, `no_booking_found`, `cancellation_confirmed`,
`cancellation_failed`, `reconciliation_failed`. Nobody decides these. They are facts.

Putting both in one proposer-facing type means a hostile proposer can submit
`BookingConfirmed` and reach `Booked` without the council ever being called — the model
declaring its own success, which is the single failure this project exists to prevent.
Guarding against that is a check we must remember to write everywhere, forever.

**Decision.** Two types, two entry points:

```rust
enum BookingProposal { SelectVenue { .. }, VerifySlot, ChangeVenue,
                       UpdateRequirements(..), RevalidateVenue, Book, Cancel { .. } }

enum BookingObservation {
    BookingConfirmed(VerifiedBookingEvidence),
    BookingFailed(VerifiedBookingFailure),
    BookingNotFound(VerifiedNoBookingEvidence),
    CancellationConfirmed(VerifiedCancellationEvidence),
    CancellationFailed(VerifiedCancellationFailure),
    ReconciliationFailed(VerifiedReconciliationFailure),
}

resolve_proposal(state, proposal, verified_authority, context) -> Resolution<Plan, Error>
apply_observation(state, verified_observation)               -> Result<NextState, Error>
```

The forbidden transition is now **absent from the proposer-facing type system** rather
than rejected by a guard.

### The second door is the *verified evidence* door, not the council's door

A raw provider response is not admissible. The council is external, and an attacker who
can shape a response must not thereby move our state. Only evidence an adapter has
verified and bound to the expected `EffectIntentId`, venue, slot and principal may
construct a `BookingObservation`:

```text
AgentClaim<T>   != truth
RawProvider<T>  != truth
Verified<T>     == admissible evidence
```

### `Reconcile` leaves the proposal vocabulary

Reconciliation is not a business intention Lucy expresses. It is recovery machinery owned
by the runtime: an uncertain outcome enqueues a job, the reconciler reads the
`EffectIntentId`, asks the council what happened, and the verified answer enters through
the observation door. Recovery must happen whether the model is awake, offline, malicious
or absent, so it cannot depend on the model asking for it.

### A third category exists but does not get a third door yet

Timer expiry, retry-budget exhaustion, lease expiry and provider timeout are neither
intent nor external fact — they are deterministic runtime events. If they begin changing
domain state they should be modelled separately (`SystemEvent`), but no API door is
created for them until that need is real.

## ADR-013 — `BookingInProgress` is committed before the council is called

The `resolve → execute → validate` pipeline conflates two different moments: *requesting*
an effect and *learning its result*. That was harmless in M2, where the fake capability
answered synchronously. It is not harmless once the effect is real.

If we call the council first and commit afterwards:

```text
call council -> council books the room -> process crashes -> no local record
```

there is no durable evidence we ever intended that booking, and nothing for recovery to
reconcile against.

**Decision.** The intent is persisted and committed *before* any external call:

```text
AwaitingBooking v3
    -> resolve Book, derive canonical plan
    -> persist EffectIntentId + canonical plan, commit BookingInProgress v4
    -> COMMIT
    -> only now call the council
    -> raw response / timeout / lost response
    -> adapter verifies and binds -> VerifiedBookingEvidence
    -> apply_observation -> COMMIT Booked v5
```

A crash at any point after the first commit leaves `BookingInProgress` plus
`EffectIntentId`, which is exactly what the reconciler needs.

This also means a network call must never happen inside a database transaction. Combined
with ADR-014's `BEGIN IMMEDIATE`, holding a transaction across a council call would block
every unrelated commit for the busy timeout. The protection is structural: the repository's
prepare/finalize methods return *committed* state, so there is no signature through which
a capability can be invoked mid-transaction.
