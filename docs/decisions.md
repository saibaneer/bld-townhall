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
VerifiedProviderFact::BookingExists {
    effect_intent_id, booking_ref, venue_id, slot_id, principal,
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

The verifier establishes *what is true externally*. The domain decides *what that truth
means here*. A verifier that emitted state-specific observation variants would have to
know the state, which is the wrong coupling.

### `Verified<T>` is provenance, not blanket trust

`Verified<T>` means the external claim passed its provenance verifier. It does **not** mean
every relationship between that evidence and the current resource is valid. The domain
still checks the binding: does the `EffectIntentId` match the active effect, the
`BookingId`, venue, slot and principal match the persisted canonical plan, is the current
state one where this fact applies at all.

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

`Converged` is deliberately **not** added to the proposal door: for intent, a silent no-op
hides mistakes, and `Book` when already `Booked` is better as `Undefined` or `Denied`.

### `Reconcile` leaves the proposal vocabulary

Reconciliation is not Lucy's intent and not the model's. It is runtime recovery machinery,
and it must run with a helpful model, a hostile model, a broken model, or no model at all.
Removing the variant takes proposals from 8 to 7 and the topology matrix from 80 cells to
70; update `LOCKED` in the same commit.

### `reconciliation_failed` is a system event, not a provider observation

The council can tell us a booking exists or does not. It cannot tell us our retry budget is
exhausted. `ReconciliationExhausted { effect_intent_id }` belongs to the third class, and
`NeedsHuman` is reachable only through that door.

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
matters more since `commit` uses `BEGIN IMMEDIATE` (ADR-015): a transaction held across a
council call would block every unrelated booking for the busy timeout.
