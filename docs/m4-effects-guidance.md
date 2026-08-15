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

M4 is where the single-vocabulary design breaks. Those three in-flight states —
`BookingInProgress`, `CancellationRequested`, `CancellingBooking` — are where the two
non-intent classes appear. Most of their exits are verified provider facts or runtime
facts.

They are not *exclusively* so: `BookingInProgress + cancel -> CancellationRequested` is a
genuine intent edge, and mid-flight cancellation depends on it. The point is not that these
states take no requests; it is that their *outcome* edges are never requests.

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

See **ADR-012 in [`decisions.md`](decisions.md)** for the canonical `BookingProposal`,
`VerifiedProviderFact` and `SystemEvent` definitions. They are deliberately not restated
here — duplicating them across documents is what caused this design to drift out of sync
four separate times during review.

The same fact means different things depending on where the resource is:

```text
BookingExists + BookingInProgress      -> booking_confirmed -> Booked
BookingExists + CancellationRequested  -> booking_found     -> CancellingBooking
```

### Negative facts: absence is the council's determination (ADR-016)

`BookingExists` is monotonic — true once true — so re-evaluating it after a lost CAS is
safe. **Absence is not**, and a stale `BookingAbsent` re-applied after a cancel won the race
would commit a terminal `Cancelled` while the room is booked.

ADR-016 closes this, and the shape matters:

> **We never evaluate `now > expires_at`.** The council reports definitive absence, using
> its own clock, at the same serialization point that prevents creation. The verifier turns
> that answer into `BookingAbsent`; anything weaker stays `Unknown`.

Three requirements on the mock council, all load-bearing:

1. **Expiry binds at commit, atomically** — not at receipt. A council that accepts at
   10:00:29.9, passes a receipt-time check, and writes at 10:00:30.2 reproduces the exact
   defect with an expiry field that appears to prevent it.
2. **The council owns the clock.** If our clock ran ahead we would declare absence while the
   council still considered the intent live. The comparison never happens on our side, which
   also keeps the domain clock-free as ADR-013 requires.
3. **A definitive-absence answer serializes after every possible commit** for that intent, so
   the lookup cannot slip between "accepted" and "written".
4. **Answering definitive absence writes a tombstone, and that write is durably committed
   *before* the response is observable.** Every later create attempt for that identity is
   rejected by the tombstone's presence, regardless of any subsequent clock reading.

   Two failure modes this closes. Without the tombstone, absence rests on time, and a clock
   that steps backwards lets a delayed request commit after absence was verified. Without
   commit-before-response, the council can answer "absent", crash before the write lands,
   and then accept a booking for the same identity — no database commit and network response
   are atomic with each other, so the ordering has to be stated.

   The tombstone is what makes the answer permanent; expiry is only what makes the council
   willing to write it. This is the same persist-before-effect discipline as ADR-014, applied
   to the council's own answer.

And one requirement that pulls the other way, easy to miss: **a booking committed just before
expiry must stay discoverable and idempotently returnable forever after.** Expiry bounds when
an effect may be *created*, never how long a created effect remains visible — otherwise a
retry past the deadline sees nothing and books the room twice.

See ADR-016 for the worked interleavings, the costs, and the alternatives rejected.

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

Canonical definition in ADR-012.

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
    expires_at_ms       INTEGER NOT NULL,   -- ADR-016: absence is only definitive past this
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

### The general rule: entering an in-flight state is always an `ExternalEffect`

Stated once so it does not have to be restated per path:

> **Every transition whose target is `BookingInProgress` or `CancellingBooking` is an
> `ExternalEffect` plan, never `Local`. Those states mean "an external call is about to
> happen or may already have happened", so entering one without a committed durable intent
> is a contradiction.**

That covers all three routes into an in-flight state:

| Transition | Door | Effect intent created |
|---|---|---|
| `AwaitingBooking + book -> BookingInProgress` | intent | booking |
| `Booked + cancel -> CancellingBooking` | intent | cancellation |
| `BookingExists + CancellationRequested -> CancellingBooking` | fact | cancellation |

The middle row is the ordinary case — Lucy cancelling a confirmed booking — and it needs
the identical contract. ADR-014 applies unchanged to all three: nothing external happens
until a durable intent for *that* effect is committed.

For the fact-driven route, in one transaction before the cancellation capability is called:

```text
CancellationRequested v5
    -> mark the BOOKING intent (E-9271) complete - its outcome is now known
    -> create the CANCELLATION intent (E-9272) with its own canonical plan
    -> set active_effect = E-9272
    -> commit CancellingBooking v6
    -> COMMIT
    -> only now cancellation_capability.execute(E-9272)
```

The handoff must be atomic. Committing `CancellingBooking` and *then* persisting the
cancellation intent leaves a window where a crash gives recovery a state that implies an
in-flight cancellation with no identity to reconcile against — and a retry would mint a
second cancellation attempt, which is the duplicate-effect failure in a different costume.

The direct route from `Booked` is simpler — there is no booking intent still open to
complete — but otherwise identical:

```text
Booked v5
    -> create the CANCELLATION intent (E-9272) with its own canonical plan,
       derived from the booking_ref recorded on the aggregate
    -> set active_effect = E-9272
    -> commit CancellingBooking v6
    -> COMMIT
    -> only now cancellation_capability.execute(E-9272)
```

A booking and its cancellation are two effects with two identities. Reusing E-9271 for the
cancellation would make "has this effect completed?" unanswerable.

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
10. reconciliation discovers a provider effect that local state has not yet adopted;
11. **cancellation** commits at the provider then the response is dropped — reconciliation
    finds exactly one cancellation, not two;
12. **crash between committing `CancellingBooking` and calling the cancellation capability**
    — recovery finds the durable cancellation intent and resumes under the same identity;
13. cancellation retried after a dropped response — same cancellation intent id returns the
    original provider result rather than cancelling twice;
14. **"not found" before expiry is `Unknown`, not absence** — the reconciler must not commit
    `Cancelled` while the intent could still be committed;
15. **accepted before expiry, commit paused until after expiry, lookup concurrent with it** —
    this is the test that distinguishes commit-time expiry from receipt-time expiry, and a
    council doing the latter must fail it;
16. **BLD clock deliberately ahead of the council's** — we must still not manufacture
    absence, because we never evaluate the deadline ourselves;
17. **post-expiry `BookingAbsent` loses a CAS, is re-applied, while a competing request was
    accepted** — the original race, run through the full re-apply path;
18. **a booking committed immediately before expiry stays discoverable afterwards**, and a
    same-identity retry past the deadline returns that original result rather than creating
    a second booking;
19. **the council's clock steps backwards after a definitive-absence answer** — a delayed
    request must still be rejected, by the tombstone rather than by a time comparison;
20. **the council crashes immediately before the tombstone write commits** — no absence
    answer may have been observed, so a later booking for that identity is still legitimate;
21. **the council crashes immediately after the tombstone write commits but before
    responding** — the retried lookup must return the same definitive absence, and a later
    create attempt must still be rejected.

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
- bypassing M3 CAS/version semantics;
- evaluating `now > expires_at` locally instead of taking the council's determination
  (ADR-016);
- a mock council that checks expiry at receipt rather than atomically at commit;
- a council that stops returning an effect that was committed before its expiry - that
  breaks stable identity and duplicates bookings on retry;
- answering definitive absence without durably tombstoning the intent, which leaves the
  guarantee resting on the clock never moving backwards.
