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

## ADR-012 — Three provenance classes, three doors

Until now every transition arrived through one type, `BookingProposal`, which is the
vocabulary the untrusted proposer submits from. But the state machine mixes edges of
fundamentally different provenance.

```text
1. Proposal              human or agent intent
2. VerifiedProviderFact  externally verified reality
3. SystemEvent           deterministic runtime fact
```

**Intent** is what someone *requests*: `select_venue`, `verify_slot`, `change_venue`,
`update_requirements`, `revalidate_venue`, `book`, `cancel`.

**Verified provider facts** are what is *externally true*: a booking exists, a booking does
not exist, a cancellation exists, the provider rejected the request.

**System events** are what the *runtime knows*: retry budget exhausted, reconciliation
deadline exceeded, lease expired, approval expired.

Putting these in one proposer-facing type lets a hostile proposer submit
`BookingConfirmed` and reach `Booked` with no council call — the model announcing its own
success. Guarding against that is a check someone must remember to write everywhere,
forever.

**Decision.** Separate types and separate entry points, one per provenance class:

```rust
kernel.resolve_proposal(...)      // what someone wants
kernel.resolve_fact(...)          // what reality says
kernel.resolve_system_event(...)  // what the runtime knows
```

Not one enum with three groups of variants. The type-level boundary is the point.

The forbidden transition is **absent from the proposer-facing type system** rather than
rejected by a guard.

### Evidence is a fact; the transition is state-relative

The durable thing is not an observation. It is a fact:

```rust
// CANONICAL DEFINITIONS. Other documents reference this section rather than
// restating these shapes - restating them in four places is what caused the
// drift this ADR had to be revised for.

/// Intent. Reachable by a human or agent through `resolve_proposal`.
enum BookingProposal {
    SelectVenue { venue_id: VenueId, slot_id: SlotId },
    VerifySlot,
    ChangeVenue,
    UpdateRequirements(RequirementsPatch),
    RevalidateVenue,
    Book,
    Cancel { reason: CancellationReason },
    // no Reconcile - recovery is runtime-owned, see below
}

/// Externally verified reality. State-neutral: the domain interprets these.
/// Every field is bound against the persisted canonical plan.
enum VerifiedProviderFact {
    BookingExists {
        effect_intent_id: EffectIntentId,
        booking_ref: CouncilBookingRef,
        venue_id: VenueId,
        slot_id: SlotId,
        attendees: u16,
        fee: Money,
        principal: PrincipalId,
    },
    /// Admissible only once the intent has expired - see ADR-016. Before that
    /// the council's "not found" is Unknown and drives no transition.
    BookingAbsent { effect_intent_id: EffectIntentId },
    CancellationExists { effect_intent_id: EffectIntentId, booking_ref: CouncilBookingRef },
    ProviderRejected { effect_intent_id: EffectIntentId, reason: BoundedString },
}

/// Deterministic runtime fact. Neither intent nor external truth.
enum SystemEvent {
    ReconciliationExhausted { effect_intent_id: EffectIntentId },
}
```

The same fact means different things depending on where the resource currently is:

```text
BookingExists + BookingInProgress      -> booking_confirmed -> Booked
BookingExists + CancellationRequested  -> booking_found     -> CancellingBooking
```

This is what makes a lost race safe. If Lucy's `Cancel` wins `v4 -> v5` while a verified
`BookingExists` was in flight, the fact is **re-evaluated against the new state**, not
discarded. The council really did book the room; losing a compare-and-set does not make
that untrue.

> **Evidence identity is stable across races; transition meaning is derived from evidence
> plus current authoritative state.**

### This holds for monotonic facts only — negative facts are unresolved

`BookingExists` stays true once true, so re-evaluating it after a lost CAS is safe.
**Absence is not stable**, and the rule above was stated too broadly:

```text
1. reconciler queries while the booking call is still completing -> verifies BookingAbsent
2. Cancel wins the CAS -> CancellationRequested
3. the council finishes creating the booking
4. the stale BookingAbsent is re-applied -> commits Cancelled
   -> terminal local state, live external booking, and nothing will reconcile it
```

**Resolved by ADR-016:** `BookingAbsent` is admissible **only from the council's definitive
absence response**, which atomically tombstones the effect intent. We never evaluate a
deadline ourselves — anything short of that response is `Unknown`.

The verifier establishes *what is true externally*. The domain decides *what that truth
means here*. A verifier that emitted state-specific observation variants would have to
know the state, which is the wrong coupling.

### `Verified<T>` is provenance, not blanket trust — and not unforgeable

`Verified<T>` means the external claim passed its provenance verifier. It does **not** mean
every relationship between that evidence and the current resource is valid. The domain
still checks the binding: **every consequential field the fact carries** must match the
persisted canonical plan — effect identity, resource, venue, slot, attendees, fee and
principal — and the current state must be one where this fact applies at all.

`fee` and `attendees` are not optional in that list. A council booking made for a different
price or headcount than the plan authorised is exactly what the fee ceiling and capacity
guards exist to prevent, and the evidence binding is the only place it is detectable.

The canonical plan lives in the `effect_intents` row rather than in `BookingState`, and
`active_effect` is normally cleared once an effect finalises — so the coordinator loads the
effect intent and supplies the plan through `resolve_fact`'s context. The domain refuses
rather than guesses when it is absent.

**Where these types live.** `Verified<T>` is the generic provenance wrapper and belongs in
`bld-kernel`. `VerifiedProviderFact` is town-hall vocabulary — `BookingExists` names a
venue and a slot — so it belongs in `townhall-domain`, surfaced to the kernel as the
`BoundaryDomain::ProviderFact` associated type. Putting it in the kernel would make the
kernel domain-aware, which ADR-001 forbids.

Division of labour: the **verifier establishes provenance** (this response genuinely came
from the council and is intact); the **domain binds** it (this fact concerns the effect we
are actually running, and this resource in this state). A verifier that did the binding
would need to know the state, which is the coupling ADR-012 exists to avoid.

Both carry private fields and **no `Deserialize`** — deserialising verified evidence from
JSON is exactly the forgery they are meant to prevent. `agent-runtime` and `bld-client` may
depend on neither `bld-kernel` nor `townhall-domain`, so the untrusted half cannot *name*
these types. The fact and system-event entry points must not be
reachable from proposer-facing transport.

What it does **not** provide is unforgeability in general: any code inside the trusted half
can still construct one. The constructor is named `assert_verified` so every call site is
greppable and auditable. Separate enums give vocabulary separation; provenance comes from
the crate graph plus that audit discipline. Claiming more than that would be the
overclaiming this project exists to avoid.

### The fact door needs a fourth outcome

Recovery loops re-apply the same fact by design, so a repeat is normal rather than a
failure:

```rust
enum FactResolution<P, E> {
    Undefined,      // BookingExists + Draft - no applicable edge
    Denied(E),      // BookingExists + BookingInProgress, wrong EffectIntentId
    Ready(P),       // BookingExists + BookingInProgress -> Booked
    Converged,      // BookingExists + Booked - already reflects this fact
}
```

`Converged` is not success-by-ignoring. It is success because authoritative local state
already reflects the verified external fact. Without it a reconciler reads healthy
convergence as breakage.

`Converged` requires the evidence to **match** what is recorded. A `BookingExists` arriving
at `Booked` with a *different* `booking_ref` is not convergence — one effect identity has
resolved to two provider bookings, which means duplication, corruption or broken
idempotency. That is `Denied(DuplicateProviderEffect)` and needs investigation.

`Converged` is deliberately **not** added to the proposal door: for intent, a silent no-op
hides mistakes, and `Book` when already `Booked` is better as `Undefined` or `Denied`.

### `Reconcile` leaves the proposal vocabulary

Reconciliation is not Lucy's intent and not the model's. It is runtime recovery machinery,
and it must run with a helpful model, a hostile model, a broken model, or no model at all.
Removing the variant takes proposals from 8 to 7 and the intent topology from 80 cells to
70; update `LOCKED` in the same commit.

### `reconciliation_failed` is a system event, not a provider observation

The council can tell us a booking exists or does not. It cannot tell us our retry budget is
exhausted. `ReconciliationExhausted { effect_intent_id }` belongs to the third class, and
`NeedsHuman` is reachable only through that door.

**Therefore M4 builds that door**, with exactly that one variant. Deferring it would leave
`NeedsHuman` unreachable and an exhausted reconciliation sitting in-progress forever. The
event must be derived from durable retry/deadline accounting, not an in-memory counter, or
a restart resets the budget.

## ADR-013 — The kernel classifies; the coordinator commits

`Kernel::apply` currently runs `resolve -> execute -> validate` in one call and assigns
`*state = next` at the end. That worked while the capability was an in-process fake. It
cannot express what a real external effect requires:

```text
commit  ->  external call  ->  commit again
```

**Decision.** The kernel stops mutating state. Its job becomes: *given authoritative
current state and an input, derive a legal transition decision.* The repository performs
the compare-and-set; the coordinator sequences the two commits around the network call.

```rust
kernel.resolve_proposal(domain, &state, proposal, authority, context)
    -> Resolution<TransitionPlan<S, E>, DomainError>

enum TransitionPlan<S, E> {
    Local          { next_state: S },
    ExternalEffect { next_state: S, effect: E },
}
```

A local transition (`Draft -> VenueSelected`) completes immediately. An external-effect
transition (`AwaitingBooking -> BookingInProgress`) yields a durable effect plan that must
be persisted before execution. This avoids forcing every transition through an effect
workflow.

`S` is the **complete next aggregate value** the domain has decided on — state plus
`booking_ref`, `active_effect` and `availability` — not the state discriminator alone.

Having the repository derive those from a state-only plan would put domain mutation
semantics in the persistence layer, which ADR-001 and the guide's dependency direction both
forbid: the repository would have to know that confirming a booking sets `booking_ref` and
clears `active_effect`. It must not know that. The domain decides every business field; the
repository owns only the version increment, timestamps and atomicity.

The repository then writes that value **atomically with** the audit row, the effect-intent
row and any reconciliation job. One transaction, or the guarantees are worthless.

Querying the provider is deliberately **not** a third variant. "Ask the council what
happened" is a coordinator operation producing a `VerifiedProviderFact` that then enters
through `resolve_fact`; modelling it as a transition would invite minting a second effect
intent during recovery, which is the failure M4 exists to prevent. Loading authoritative
availability before `VerifySlot` is likewise a coordinator responsibility that populates
context, not a transition.

`execute` and `validate` leave `BoundaryDomain`; they were never domain concerns:

```rust
trait Capability<E> { async fn execute(&self, effect: &E) -> Result<RawProviderResult, CapabilityError>; }
trait Verifier<R, F> { fn verify(&self, raw: R) -> Result<Verified<F>, VerificationError>; }
```

Responsibilities settle as:

```text
Domain       legal meaning
Kernel       deterministic transition resolution
Repository   authoritative CAS commit
Coordinator  external-effect choreography
Capability   external action
Verifier     provenance establishment
Reconciler   recovery loop
```

The four existing kernel tests migrate from "kernel mutates state" to "kernel
deterministically classifies and derives legal transitions" — a stronger contract, and
worth changing the API for rather than preserving a misleading abstraction.

### What this supersedes, stated explicitly

The implementation guide requires an agent to stop and surface contradictions rather than
silently choose a new architecture, so the conflicts are named here rather than left for a
reader to trip over:

| Superseded | Where | By |
|---|---|---|
| `BoundaryDomain::{resolve, execute, validate}` | spec §5; guide §17 | ADR-013 — `execute` and `validate` move to `Capability` and `Verifier` |
| Kernel owning `&mut State` and committing | spec §5.1 | ADR-013 — the kernel classifies; the repository commits |
| `BookingProposal::Reconcile` | spec §7.1 | ADR-012 — reconciliation is runtime-owned |
| Single proposal vocabulary for all edges | spec §7 | ADR-012 — three provenance classes |

The specification is not edited: it remains the v0.4.2 execution contract of record, and
these ADRs are the amendment trail against it. Spec §5.1's sequencing *intent* is preserved
— resolve, then effect, then evidence, then commit — what changes is which component owns
each step.

## ADR-014 — `BookingInProgress` is committed before the council is called

If we call first and commit afterwards:

```text
call council -> council books the room -> process crashes -> local state still AwaitingBooking
```

We have lost the fact that an external consequence may exist, and there is nothing for
recovery to reconcile against.

**Decision.** The intent is persisted and committed before any external call:

```text
AwaitingBooking v3
    -> resolve_proposal(Book) -> Ready(ExternalEffect { BookingInProgress, E-9271 })
    -> CAS v3 -> v4, persisting BookingInProgress + canonical plan + EffectIntentId
    -> COMMIT
    -> capability.execute(E-9271)          <-- only now
    -> raw result / timeout / lost response
    -> verifier -> VerifiedProviderFact
    -> reload authoritative state
    -> resolve_fact -> Ready | Converged | Undefined | Denied
    -> CAS if a transition is required
```

Crash anywhere after the first commit and recovery finds `BookingInProgress` plus its
`EffectIntentId`. Crash before it and no external call was ever made.

After a lost CAS the coordinator **reloads and re-applies the same
`VerifiedProviderFact`** — it does not replay a stale state-specific observation.

A network call must never happen inside a database transaction. The protection is
structural: the repository's prepare and finalize methods return *committed* state, so
there is no signature through which a capability can be invoked mid-transaction. This
matters more since `commit` takes its write lock at `BEGIN` (ADR-015): a transaction held
across a council call would block every unrelated booking for the busy timeout.

## ADR-015 — `commit` uses `BEGIN IMMEDIATE`

`SqliteBookingRepository::commit` opened a deferred transaction, read the version, then
updated. Two measured consequences on the real code:

| | DEFERRED (before) | `BEGIN IMMEDIATE` |
|---|---|---|
| concurrent commits to **disjoint** bookings | **52 of 60 failed** | 0 of 60 |
| loser of a genuine CAS race | `SQLITE_BUSY` | `StaleVersion` |

The disjoint-booking row is the real defect: those commits have no version contention
whatsoever — different resources, different rows — and roughly half to seven-eighths of
them failed outright with "database is locked", with no retry.

**Cause.** `commit` unconditionally writes, so a deferred begin buys nothing. It takes no
lock; the version `SELECT` opens a read transaction; the `UPDATE` must then promote that
read to a write. Under WAL a deferred transaction cannot promote once anyone has written
anywhere in the database. Worse, because `inTransaction` is already `TRANS_READ` the busy
handler is skipped, so `busy_timeout` never applies and the call fails immediately.

**Decision.** Take the write lock at `BEGIN`. SQLite permits only one writer regardless, so
this costs no real concurrency — it moves the serialisation point from mid-transaction,
where it failed, to `BEGIN`, where it waits.

This also restores the three-outcome model at the storage layer. A lost race is now a clean
`Denied(StaleVersion)` rather than an opaque infrastructure error, which is what M5 needs to
map `ETag`/`If-Match` to 412 (versus 503 for genuine infrastructure failure), and what M4's
reconciliation workers need in order to know whether to retry.

### The tradeoff, recorded so it is not rediscovered as a regression

`IMMEDIATE` holds a **database-wide** write lock for the whole transaction, under
`busy_timeout(5s)`. Today every transaction is short and local, so this is free. If a
transaction ever spans a network call, the failure mode changes character: `DEFERRED` fails
fast on the offending path, whereas `IMMEDIATE` turns it into every unrelated commit
blocking five seconds. The symptom becomes "the service stalls in five-second steps" rather
than "this one booking errored".

**This is a requirement on M4, not a property the repository has today.** Nothing in the
current surface prevents a caller holding a transaction across a network call — there is
simply no capability to call yet. When M4 introduces one, the protection must be
structural rather than advisory: the repository's prepare and finalize methods should
return *committed* state, so there is no signature through which a capability can be
invoked while a transaction is open. Do not read this ADR as saying that guard already
exists.

### Deliberately not done

Do **not** remap `SQLITE_BUSY_SNAPSHOT` to `StaleVersion` in `commit`. The WAL snapshot is
database-wide, so a commit on booking Y invalidates a concurrent commit's snapshot on
unrelated booking X. Remapping would tell that caller "your version is stale" when it is
perfectly current — a false `Denied` is a lie about authoritative state, and worse than an
honest infrastructure error.

Do **not** drop the pre-`UPDATE` `SELECT`. It is not redundant: it supplies `from_state` and
`created_at_ms` for the audit row.

`if result.rows_affected() != 1` is unreachable under SQLite WAL — a writer holding the
write lock has a valid snapshot, so the row still matches — but it stays for the anticipated
Postgres port, where READ COMMITTED re-reads per statement.

## ADR-016 — Absence is a provider determination, gated on effect expiry

ADR-012 said a verified fact that loses a compare-and-set is re-evaluated against the new
state rather than discarded. Sound for **monotonic** facts — `BookingExists` stays true once
true — and unsound for absence, which is a claim about *now*:

```text
10:00:00.0  we send E-9271 to the council; the request is in flight
10:00:00.1  reconciler asks "anything for E-9271?" -> verified BookingAbsent
10:00:00.2  Lucy's cancel wins the CAS -> CancellationRequested
10:00:00.3  the request lands; the council books the room
10:00:00.4  the stale BookingAbsent is re-applied -> commits Cancelled
```

Terminal local state, live external booking, and `Cancelled` is terminal so nothing ever
reconciles it.

**Decision.** Every effect intent carries `expires_at_ms`, and absence becomes something the
**council determines and reports** — never something we infer locally. Three parts, all
load-bearing:

### 1. Expiry binds at the council's commit point, not at receipt

The council's promise is not "I will not accept an expired intent". It is:

> **An effect intent is never *committed* after its expiry, and that check is atomic with
> the commit itself.**

Receipt-time checking is insufficient and reintroduces the original defect one level down:

```text
10:00:29.9  request accepted - not yet expired, so a receipt-time check passes
10:00:30.0  intent expires
10:00:30.1  reconciler asks; council has not written anything yet -> "absent"
10:00:30.2  the council finishes writing the booking
            -> same failure, now with an expiry field that appears to prevent it
```

### 2. The council owns the clock; we never evaluate `now > expires_at`

If our clock runs ahead we would declare absence while the council still considers the
intent live. So the comparison never happens on our side. The council returns a definitive
answer — *"expired, and nothing was committed for E-9271"* — computed with its own clock at
the same serialization point that prevents creation. The verifier turns that answer into
`BookingAbsent`; anything weaker stays `Unknown`.

This also keeps the domain clock-free, as ADR-013's split requires. `BookingAbsent` existing
at all *is* the assertion that absence is permanent; the domain performs no temporal
reasoning.

### 3. A definitive-absence lookup serializes after every possible commit for that intent

Otherwise the lookup can slip between "accepted" and "written". The council must answer
absence only from a point where no commit for that intent can still be in progress.

### 4. Definitive absence is a durable tombstone, not a clock reading

The three rules above still leave absence resting on time, and time can move backwards:

```text
1. council clock reads past expiry; the lookup serializes and answers "absent"
2. the council's clock steps backward (NTP correction, VM migration, operator)
3. a delayed request reaches the commit point, now appears unexpired, and commits
4. the already-verified absence is re-applied -> terminal Cancelled, live booking
```

So the answer must not be *"the deadline has passed"*. It must be **"this effect intent is
permanently closed and nothing was created for it"**, written down:

> Answering definitive absence **persists a tombstone for that effect intent, durably
> committed before the response is observable**. Every later create attempt for that identity
> is rejected by the tombstone's presence, regardless of any subsequent clock reading.

Commit-before-response is not pedantry: no database commit and network response are atomic
with each other, so a council could otherwise answer "absent", crash before the write lands,
and then accept a booking for the same identity. It is the same persist-before-effect
discipline as ADR-014, applied to the council's own answer.

Absence then stops being a temporal claim and becomes a fact about a durable record — which
is monotonic by construction and needs no assumption about clock behaviour. Expiry is merely
what makes the council *willing* to write the tombstone; the tombstone is what makes the
answer permanent.

With all four, absence is monotonic — the property the re-apply rule needs — and the race
above cannot occur.

### What it costs

**Bounded cancellation latency.** During an ambiguous window Lucy's cancel cannot resolve to
`Cancelled` until the council reports definitive absence. That latency is honest: before
then, whether a booking exists is genuinely unknown, and a faster answer would be a guess.

**A bounded execution window.** A first delivery slow enough to miss its deadline now fails
authoritatively rather than eventually succeeding. That is a real behaviour change: the
booking must be re-proposed, minting a *new* effect intent. Expiry converts an unbounded
unknown into a bounded failure, which is the trade being made.

**Permanent discoverability.** A booking committed *just before* expiry must remain
discoverable and idempotently returnable **forever after**, including long past the
deadline. Expiry bounds when an effect may be *created*; it must never bound how long an
effect that was created remains visible. Otherwise expiry contradicts stable effect identity
and `Converged` — a retry after the deadline would see nothing and duplicate the booking.

### Alternatives rejected

**Provider revision numbers.** Refuse answers older than the newest seen. Fails our case —
the stale answer *was* the newest we had. Making it work needs the council to promise "at
revision N I had finished processing everything I had received", a much stronger claim than
a counter, and it still needs a serialization point.

**Two flavours of "no" without expiry.** Distinguishing "definitively absent" from "not
currently visible" does not help, because the council cannot tell "never sent" from "sent and
still in flight". The ambiguous case never resolves and cancellation could never complete.
Expiry is what makes the first answer reachable at all.
