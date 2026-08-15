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

## Three doors (ADR-012) — settle this before writing code

M4 is where the single-vocabulary design breaks. Every exit from `BookingInProgress`,
`CancellationRequested` and `CancellingBooking` is either an externally verified fact or a
runtime fact — never a request.

```text
1. Proposal              what a human or agent WANTS
2. VerifiedProviderFact  what is externally TRUE
3. SystemEvent           what the runtime KNOWS
```

Adding the second or third class to `BookingProposal` would let a hostile proposer submit
`BookingConfirmed` and reach `Booked` with no council call.

```rust
kernel.resolve_proposal(...)      // intent
kernel.resolve_fact(...)          // verified reality
kernel.resolve_system_event(...)  // runtime fact
```

### Facts are state-neutral; the domain interprets them

The verifier must **not** emit state-specific variants — it would have to know the state,
which is the wrong coupling. It emits what is externally true:

```rust
enum VerifiedProviderFact {
    BookingExists { effect_intent_id, booking_ref, venue_id, slot_id, principal },
    BookingAbsent { effect_intent_id, /* see the temporal caveat below */ },
    CancellationExists { effect_intent_id, booking_ref },
    ProviderRejected { effect_intent_id, reason },
}
```

The same fact means different things depending on where the resource is:

```text
BookingExists + BookingInProgress      -> booking_confirmed -> Booked
BookingExists + CancellationRequested  -> booking_found     -> CancellingBooking
```

### ⚠ Unresolved: negative facts are not stable across races

The re-evaluate-after-a-lost-CAS rule is safe for **monotonic** facts. `BookingExists` stays
true once true. It is **not** safe for absence:

```text
1. reconciler queries while the original booking call is still completing
   -> verifies BookingAbsent
2. Lucy's Cancel wins the CAS   -> CancellationRequested
3. the council finishes creating the booking
4. the stale BookingAbsent is re-applied -> commits Cancelled
   -> local state is terminal while an external booking exists, and
      Cancelled is terminal so nothing will ever reconcile it
```

**Do not implement `BookingAbsent -> Cancelled` until this is decided.** The options are a
provider watermark or revision that lets a stale absence be recognised and refused, or
requiring the council to distinguish "definitively no booking for this effect id" from "I
do not currently see one" and treating the latter as `Unknown`. This is an open
architectural decision, not an implementation detail.

### `Verified<T>` is provenance, not blanket trust, and not unforgeable

It means the claim passed its verifier. The domain still binds it: effect identity matches
the active effect, resource and parameters match the **persisted canonical plan**, current
state is one where the fact applies.

What the type system actually gives:

- `Verified<T>` is the generic wrapper and lives in `bld-kernel`. `VerifiedProviderFact` is
  town-hall vocabulary and lives in `townhall-domain`, reaching the kernel as the
  `BoundaryDomain::ProviderFact` associated type — putting it in the kernel would make the
  kernel domain-aware.
- Both have private fields and **no `Deserialize`** — deserialising verified evidence from
  JSON is precisely the forgery it is meant to prevent.
- The verifier establishes **provenance**; the domain does the **binding**. A verifier that
  bound facts to state would need to know the state, which is the coupling we are avoiding.
- `agent-runtime` and `bld-client` may not depend on `bld-kernel` (see
  `docs/architecture.md`), so the untrusted half cannot *name* these types at all.
- The fact and system-event entry points must not be reachable from proposer-facing
  transport.

What it does **not** give: construction is still possible anywhere inside the trusted half.
The constructor is named `assert_verified` so it is greppable and every call site is an
audit point. Do not claim the type is unforgeable in general — claim what is true, that the
untrusted half cannot name it.

### `Converged`, and when a repeat is a conflict

A reconciler re-applies the same fact by design:

```text
BookingExists + BookingInProgress                 -> Ready(Booked)
BookingExists + Booked, same booking_ref          -> Converged
BookingExists + Booked, DIFFERENT booking_ref     -> Denied(DuplicateProviderEffect)
BookingExists + Draft                             -> Undefined
BookingExists + BookingInProgress, wrong effect id -> Denied(EffectMismatch)
```

The third line matters: one effect identity resolving to two different provider bookings
means duplication, corruption or broken idempotency. It must be an explicit conflict
requiring investigation, never silent convergence.

### Getting the canonical plan to `resolve_fact`

Binding needs venue, slot, fee and principal, which live in the `effect_intents` row, not in
`BookingState` — and `active_effect` is normally cleared once an effect finalises. So the
coordinator loads the effect intent by id and supplies the canonical plan through the
context passed to `resolve_fact`. The domain must not reconstruct consequential parameters
from anywhere else, and must refuse rather than guess when the plan is absent.

### `SystemEvent` is in M4 scope, minimally

`reconciliation_failed` is not something the council can tell us — it is our own retry
budget. It becomes:

```rust
enum SystemEvent { ReconciliationExhausted { effect_intent_id: EffectIntentId } }
```

M4 builds this door with exactly that one variant. Without it `NeedsHuman` is unreachable
and an exhausted reconciliation would sit in-progress forever. Deriving the event from
durable retry/deadline accounting — not from an in-memory counter — is part of the work.

### `Reconcile` leaves the proposal vocabulary

Recovery must run with a helpful model, a hostile model, a broken model, or no model.
Removing the variant takes proposals from 8 to 7 and the topology matrix from 80 cells to
70; update `LOCKED` in the same commit.

### What `TransitionPlan` covers, and what it does not

```rust
enum TransitionPlan<S, E> { Local { next_state: S }, ExternalEffect { next_state: S, effect: E } }
```

`S` is the **complete next aggregate value** the domain decided on — state plus
`booking_ref`, `active_effect` and `availability` — not the state discriminator alone. If
the repository had to derive those it would need to know that confirming a booking sets
`booking_ref` and clears `active_effect`, which is domain knowledge in the persistence
layer. The domain decides every business field; the repository owns the version increment,
timestamps and atomicity, writing that value together with the audit row, the effect-intent
row and any reconciliation job in one transaction.

Querying the provider is deliberately **not** a plan variant. "Ask the council what
happened" is a coordinator/reconciler operation that produces a `VerifiedProviderFact`,
which then enters through `resolve_fact`. Modelling it as a transition would invite minting
a second effect intent during recovery, which is the failure M4 exists to prevent.
Similarly, loading authoritative availability before `VerifySlot` is a coordinator
responsibility that populates context; it is not a transition.


## Commit before calling (ADR-014)

The `resolve -> execute -> validate` pipeline conflates requesting an effect with learning
its result. Harmless with M2's synchronous fake; not harmless with a real council.

```text
AwaitingBooking v3
    -> resolve Book, derive canonical plan
    -> persist EffectIntentId + canonical plan
    -> COMMIT BookingInProgress v4          <-- durable BEFORE any external call
    -> call the council
    -> raw response / timeout / lost response
    -> verifier establishes PROVENANCE only -> Verified<ProviderFact>
    -> load canonical plan from the effect intent
    -> resolve_fact does the BINDING (effect id, plan, state) -> COMMIT Booked v5
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
