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
    /// Nothing was created for this intent, and nothing ever can be.
    ///
    /// Deliberately kind-agnostic: absence carries only the identity, so one
    /// variant covers a booking intent and a cancellation intent alike. Which
    /// it means is derived from the persisted intent and the current state,
    /// exactly as `BookingExists` is - see ADR-012.
    ///
    /// Admissible only from the council's definitive-absence response, which
    /// tombstones the intent - see ADR-016. Anything weaker is Unknown.
    EffectAbsent { effect_intent_id: EffectIntentId },
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
1. reconciler queries while the booking call is still completing -> verifies EffectAbsent
2. Cancel wins the CAS -> CancellationRequested
3. the council finishes creating the booking
4. the stale EffectAbsent is re-applied -> commits Cancelled
   -> terminal local state, live external booking, and nothing will reconcile it
```

**Resolved by ADR-016:** `EffectAbsent` is admissible **only from the council's definitive
absence response**, which durably tombstones the effect intent before that response is
observable, in the same serialized write that excludes creation. We never evaluate a deadline
ourselves — anything short of that response is `Unknown`.

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

Two notes recorded when B3 made the canonical block executable. `BookingEffect::Book`
carries `attendees` precisely because this section requires headcount bound against the
plan — `VenueFacts.capacity` is the room's limit, not the party's size, so without the
field the one number the capacity guard checks would be unbindable. And
`CancellationRequested` carries the **booking** intent's identity: it means "cancel the
booking we are still waiting on", so the effect in flight is the booking's — a state named
for a cancellation waiting on a `Book`, which is why `in_flight_kind()` must not be read
off the state's name.

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
exhausted. `ReconciliationExhausted { effect_intent_id }` belongs to the third class: only the
runtime can conclude a fact about its own accounting, and neither a proposer nor a provider may
be able to counterfeit it.

**Therefore M4 builds that door**, with exactly that one variant, and the event must be derived
from durable retry/deadline accounting, not an in-memory counter, or a restart resets the budget.

*Amended by ADR-019.* This section originally justified the door by reachability — "deferring it
would leave `NeedsHuman` unreachable and an exhausted reconciliation sitting in-progress
forever." ADR-019 makes exhaustion a pursuit record rather than a transition, so `NeedsHuman` is
deliberately unreachable and an exhausted reconciliation deliberately stays in-progress — chased
slowly, resolvable by a late fact. The provenance argument above is the door's justification and
it survives unchanged; what the door *returns* is ADR-019 §5's.

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

`S` is the **complete next aggregate value** the domain has decided on, not the state
discriminator alone. For the town hall that is `townhall_domain::Booking`, and the list is
exhaustive rather than illustrative:

```text
id  state  requirements  selected_venue  availability  booking_ref  active_effect
```

The repository owns exactly the complement: `version`, `created_at_ms`, `updated_at_ms`.

Having the repository derive any of those from a state-only plan would put domain mutation
semantics in the persistence layer, which ADR-001 and the guide's dependency direction both
forbid: the repository would have to know that confirming a booking sets `booking_ref` and
clears `active_effect`. It must not know that. The domain decides every business field; the
repository owns only the version increment, timestamps and atomicity.

### Why the list is exhaustive, and not a sketch

This paragraph originally named only `booking_ref`, `active_effect` and `availability`. B2
then shipped `type State = BookingState`, and the partial list is part of why eight review
passes did not catch it: a reader checking the implementation against a three-field example
has nothing to notice is missing.

The omission of `requirements` was not cosmetic. `UpdateRequirements { attendees }` could not
apply its own patch, because a plan carrying only a state has nowhere to put changed
requirements — so the headcount was silently discarded and the next capacity guard validated
against the old one. Lucy raising a booking from 20 people to 25 would be revalidated against
20, and a room holding 22 would pass. Fixed in M4 slice B3a, along with the contract that
allowed it.

`id` is on the list for a different reason: evidence must be bound to *this resource*
(ADR-012), and only the authoritatively loaded aggregate can establish which resource that
is. Binding against a caller-supplied identifier would compare two values from the same
source and prove nothing. The repository verifies that a transition does not change it —
a carried field is one a future arm could rebuild wrongly.

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
| `POST /bookings` without an expiry | spec §11 | ADR-016 — `expires_at_ms` is mandatory on create |
| `GET /effects/{id}` without an expiry | spec §11 | ADR-016 — mandatory on lookup too, or absence is undecidable |
| A generic `NotFound` reconciliation result | spec §11 | ADR-016 — `DefinitivelyAbsent` and `NotYetVisible` are different answers |
| `NeedsHuman` as a reachable state | spec §7 L369/378/381 | ADR-019 — exhaustion records a pursuit decision on the effect; the booking's state does not change |

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
10:00:00.1  reconciler asks "anything for E-9271?" -> verified EffectAbsent
10:00:00.2  Lucy's cancel wins the CAS -> CancellationRequested
10:00:00.3  the request lands; the council books the room
10:00:00.4  the stale EffectAbsent is re-applied -> commits Cancelled
```

Terminal local state, live external booking, and `Cancelled` is terminal so nothing ever
reconciles it.

**Decision.** Every effect intent carries `expires_at_ms`, and absence becomes something the
**council determines and reports** — never something we infer locally. Four parts, all
load-bearing, and §4 is where the guarantee finally rests:

### 1. The deadline is evaluated inside the write transaction, never at receipt

The council's promise is not "I will not accept an expired intent". It is:

> **The deadline is read and compared inside the same write transaction that performs the write,
> after the writer lock has been acquired. A request that waited is therefore judged on when it
> reached the write — not on when it arrived.**

*Amended three times during slice D, and the history is the point — each pass removed a claim
rather than adding one.* The clause first read "*never committed after its expiry, and that check
is atomic with the commit itself*": unachievable, because SQLite does not evaluate a clock
predicate at `COMMIT`. The second attempt — "the last thing before an uninterruptible commit
path" — was **also** an overclaim: there is no uninterruptible path, since a task awaiting
`COMMIT` can be paused and a process can be stopped. The third moved the comparison into the
SQL itself (`INSERT … WHERE unixepoch() <= expires_at_ms`) and bought a real problem for a
guarantee it did not need: SQLite reads its own host clock, so the council would have had **two
clocks** — the injected one the reconciler tests move, and the engine's — able to disagree. One
fact in two places, which is the defect this project keeps finding.

**What actually carries the safety, and it is not the clock.** Two properties, both structural:

- **Serialization.** Creating an effect and settling it absent are *both* write transactions
  (§3), so they cannot interleave. Whichever acquires the writer lock first decides.
- **Permanence.** Whatever it decided is terminal (§4), so the loser is refused by the recorded
  state — not by any clock reading, however stale or rolled back.

Those two give *mutual exclusion of creation and absence*, which is exactly what the race at the
top of this ADR needs, and all it needs. The deadline's freshness never enters that argument.

**So what does this clause forbid?** Evaluating the deadline at receipt and treating that reading
as authoritative for a write that happens later. A council doing that accepts arbitrarily late
deliveries: a request that queued for a minute still writes, because it was judged on arrival.
Reading the deadline inside the write transaction is what makes the *bounded execution window*
below a real bound rather than a hopeful one.

**What it does not claim.** Punctuality. A transaction can still be paused between its write and
its `COMMIT`, so a commit can land marginally after the deadline that permitted it. That is
harmless for the reasons above, and pretending otherwise is how this clause went wrong twice.

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
`EffectAbsent`; anything weaker stays `Unknown`.

This also keeps the domain clock-free, as ADR-013's split requires. `EffectAbsent` existing
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
3. a delayed request reaches its write transaction; the clock now reads before the deadline, so
   the comparison passes and the write lands
4. the already-verified absence is re-applied -> terminal Cancelled, live booking
```

So the answer must not be *"the deadline has passed"*. It must be **"this effect intent is
permanently closed and nothing was created for it"**, written down:

> **Both** answering definitive absence **and** refusing a create for expiry persist a
> tombstone for that effect intent, durably committed before the response is observable.

Refusal must tombstone for the same reason the lookup must: a refusal that rests only on a
clock comparison is undone by a clock that rolls back, and the same intent could then commit. Every later create attempt for that identity
> is rejected by the tombstone's presence, regardless of any subsequent clock reading.

The council must also *learn* the expiry, or it cannot distinguish pre-expiry `Unknown` from
post-expiry absence — most importantly in the case this design exists for, where the create
request never arrived and the council has only an effect id. So `expires_at_ms` travels both
with the create request and with the reconciliation lookup; the council records it on first
sight of that identity and treats it as immutable thereafter, rejecting any later request
presenting a different deadline for the same id. That binding stops a caller shortening a
deadline to force premature absence, and it makes the lookup a trusted surface that must stay
unreachable from proposer-facing transport.

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

**A bounded execution window.** A first delivery that does not reach the council's writer lock
before its deadline now fails authoritatively rather than eventually succeeding. That is a real
behaviour change: the booking must be re-proposed, minting a *new* effect intent. Expiry
converts an unbounded unknown into a bounded failure, which is the trade being made.

The failure condition is *reaching the write*, not wall-clock arrival — §1 is explicit that a
commit can land marginally late and that this is safe. What expiry bounds is how long an
undelivered intent stays creatable, not the latency of a delivery already at the door.

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

## ADR-017 — Audit provenance is typed and derived; denials are recorded tiered

Decided 2026-08-18 with the project owner. Point 1 landed in **slice C** with the coordinator;
point 2 (denial recording) was moved to **slice E** by the owner, and this header originally said
otherwise.

### Amendment, slice E — the key, the store, and a declined permission

Three changes, each forced by review against the real code:

1. **The dedup key is `(booking_id, driver_kind, driver_detail, reason, principal,
   window_start_ms)`**, not the original `(booking_id, principal, reason)`. The original could
   not be formed at the fact or system-event doors — no `VerifiedAuthority` exists there — which
   silently scoped recording to `propose` and left `DuplicateProviderEffect`, the most
   consequential refusal in the system, unrecorded. `principal` stays **in** the key (dropping it
   attributes one person's refusals to another) and is *derived* per door: the fact's own where
   it carries one, else the persisted plan's, else the empty string — which means **explicitly
   unattributed**, not unknown. `reason` is the error's stable name, never its display text,
   which interpolates data and would split identical refusals. `window_start_ms` (hour floor)
   restores the "per window" semantics an early draft lost: a flood is one row per hour, and
   history keeps its shape.
2. **Denials live in their own database file with their own writer.** Writing them on the
   boundary's writer queues real work behind attacker-priced audit writes (`BEGIN IMMEDIATE`
   serialises them); every buffered design hands the adversary control of retention. A separate
   file removes the contention instead of budgeting it.
3. **The "may be asynchronous" permission is declined.** Review broke every off-path design; the
   write is a synchronous upsert against the separate store, *after* the answer is computed, and
   a failed write is logged and dropped — the guarantee this ADR actually cares about ("a lost
   denial record strands nothing", "the answer is never rate-limited") is preserved by ordering
   and separation rather than by buffering.

### Context: the audit trail asserts, and can only say one thing

`TransitionAudit` is three caller-supplied strings — `proposal`, `outcome`,
`evidence_summary` — passed through `commit_in_tx` unexamined. Its only constructor
hardcodes `outcome: "Committed"`, and a denial never reaches `commit` at all, so the
`outcome` column has exactly one possible value in the whole system. For a project whose
central claim is *"the boundary refuses correctly"*, not a single refusal is provable from
the database.

Worse once the fact door exists: three provenance classes (ADR-012), one `proposal` column.
When the council confirms a booking, the transition is driven by a verified provider fact,
but the row can only say something proposal-shaped — it must imply an intent caused the
confirmation. Ask that trail "did the model ever cause a booking to be confirmed?" and it
answers yes, wrongly. That is the asserted-not-derived defect class B3a removed from the
aggregate, alive in the audit trail.

### Decision

1. **Audit fields become typed and derived from the resolution, not asserted by the
   caller.** `proposal: String` is replaced by a `driver` carrying the door —
   `Proposal(name)` / `Fact(name)` / `SystemEvent(name)` — and `outcome` becomes the typed
   boundary outcome. `evidence_summary` is dropped; the driver carries it.
2. **Denials are recorded, tiered by outcome.**
   - `Undefined` — constructible from pure garbage, unbounded — is **counted, never
     rowed**: in-memory counters per `(state, proposal)`, flushed periodically,
     crash-lossy by design.
   - `Denied` — requires a real booking in a real state — gets **durable rows**,
     deduplicated per `(booking_id, principal, reason)` per window
     *[superseded by the slice-E amendment above: the key is
     `(booking_id, driver_kind, driver_detail, reason, principal, window_start_ms)` —
     the original could not be formed at the fact or system-event doors]*: the first N are
     recorded, further identical refusals increment a suppressed-count. Identical retries
     are compressible precisely because they are identical: the rows carry the *what*, the
     counter carries the *how many*. Nothing forensic is lost.
   - Denial writes may be **asynchronous and off the request path**
     *[permission declined in the slice-E amendment above: the write is a synchronous
     upsert against the separate store, after the answer is computed]*. A lost denial
     record strands nothing — no state changed, no recovery waits on it — so it does not
     need the commit-grade durability that effect intents need. This is deliberate
     tiering of durability by consequence, not corner-cutting.
3. **The boundary's answer is never rate-limited.** Classification is a pure function;
   request 501 receives the same deterministic `Denied` as request 1, at match-arm cost.
   Only the audit trail's appetite for identical rows saturates. A boundary whose answers
   depended on history would no longer be deterministic.
4. **The returned-value form ("verdict") is deferred to M5**, where the agent-facing
   consumer exists. If built, it revives the dormant `BoundaryOutcome` shape in
   `bld-kernel` rather than adding a peer type, and takes the same anti-forgery treatment
   as `Verified<T>`: private fields, no `Deserialize`.

### Amendment, slice C2 — the deferral was one slice too long

Point 4 deferred the returned value to M5. Slice C2 needs one earlier: `propose` must tell
its caller *what happened*, and the aggregate alone cannot express four of the five answers —
`Undefined`, `Denied`, `Converged` and "an effect is in flight and its outcome is not yet
knowable". Returning the aggregate and leaving the caller to infer is how a transport ends up
guessing.

So `BoundaryOutcome` is revived now, as point 4 prescribes — **not** a peer type beside it,
which is what a first draft of the C plan proposed and review correctly rejected. It gains the
two outcomes the coordinator can genuinely produce and the proposal door cannot:

```rust
pub enum BoundaryOutcome<S, E> {
    Undefined,
    Denied(E),
    Committed(S),
    Converged,    // local state already reflected the evidence; nothing written
    Unresolved,   // an effect is in flight; its outcome is not yet knowable
}
```

`Unresolved` is the one that matters. A coordinator that collapsed it into `Denied` would
return a booking to a re-proposable state while the council held a live booking, which is the
failure M4 exists to prevent. It is neither success nor failure, and it must be sayable.

What stays deferred to M5 is the *anti-forgery* treatment — private fields and no
`Deserialize`. Nothing crosses a wire until M5's transport exists, and adding the ceremony
before there is a wire to protect would be ritual rather than defence.

### Why tiered, not one of the simpler shapes

The decision space was walked explicitly (eight options, from "keep not recording" to
"record everything"), replayed against one night of traffic: a real refusal at 23:00, a
stuck agent flooding ~5M `Undefined` events overnight, and a legitimate booking needing the
single `BEGIN IMMEDIATE` writer lock at 02:00.

- **Record everything, same store** puts attacker-priced writes on the lock this project
  measured failing at 52/60 under contention (ADR-015), and queueing there burns real
  bookings' council TTL. Vetoed.
- **Counters only** answers "are we under attack" but cannot prove any individual refusal.
- **Sampling** keeps statistics, not evidence: the one refusal an auditor asks for is
  probably not there.
- **A bounded buffer** hands the adversary control of retention — flood cheap denials to
  evict the refusal that mattered.
- **Telemetry-only** makes refusals as ephemeral as the model's chatter; a provable
  refusal you can pull from the database is part of this project's demonstration.

The tier survives all of those failure modes at near-constant cost: the flood is one
counter, the refusal that matters is a full durable row.

### The design-basis adversary

Not an internet attacker — there is no public endpoint until M5. BLD's adversary is the
probabilistic component we already distrust, and it does not need to be hostile: an agent
whose loop does not understand denials retries at machine speed, and denials are the one
output it can generate unboundedly, because they are what it produces by being wrong, and
being wrong is free. We hope agents relay typed denial reasons to their principals; we
design for the ones that retry.

The symmetry with ADR-012, stated once: **denial reasons are the vocabulary the untrusted
half is meant to see** — the outbound feedback channel. **Evidence types are the vocabulary
it must not be able to name.** Information flows out as typed refusals; it never flows in
as claimed facts.

## ADR-018 — The transition topology stays synthesisable, as a discipline

No hardware is being built. This POC targets Rust on a general-purpose machine and nothing
else. But the state machine is kept in a form that *could* be moved to a fixed-function
target — an FPGA, a safety-certified controller, a robot — and that constraint is retained
deliberately, because it forbids exactly the things that would rot the design anyway.

The motivation is not portability. It is that a state machine whose permitted transitions
are a **fixed, total table** cannot be driven somewhere nobody specified. On hardware that
stops being a rule you enforce and becomes a wire that is not there. If the same machine
one day drives an actuator instead of a booking, "the robot can only do what this state
permits" is the property worth having.

### Which door the claim is about, and why that is the strong version

**The proposal door**, and the system-event door with it. Not the fact door, and PR review
caught an earlier draft of this ADR claiming otherwise.

`resolve_fact` reads the persisted intent — its kind, its status, its canonical plan — and the
same `(state, fact)` pair can be a convergence, a contradiction or a handoff depending on what
was in flight. `EffectAbsent` at `Booked` against a *cancellation* intent already recorded
`Absent` is `Converged`: a cancellation that did not happen leaves the booking booked, and
re-applying that absence is the re-apply-by-design case ADR-012 exists for. That door is not a
combinational table and must not be exported as one. `docs/topology.json` now marks each door
`fixed_table: true | false` and names the axes it varies.

**That scoping is not a retreat.** The proposal door is the only one an untrusted proposer can
reach: ADR-012 made facts and system events separate types that proposer-facing crates cannot
name, so an agent submits proposals or nothing. "The robot can only do what this state permits"
is therefore a claim about the proposal door, and *there* it is exactly true — `Undefined` is
decided from `(state, proposal)` before any guard reads the aggregate.

`resolve_system_event` takes no context at all, so that door is a fixed table too, and for a
sharper reason: there is nothing else in reach.

### What makes it possible today

The seam already exists, and it is ADR-004 plus the `Undefined`/`Denied` split:

| | Meaning | Fixed-function form |
|---|---|---|
| `Undefined` | no edge exists from this state for this input | a table — combinational, data-independent |
| `Denied(e)` | the edge exists; a guard refused it this time | comparators over the guard's inputs |

`resolve_proposal` decides `Undefined` from the `(state, proposal)` pair **before** any guard
reads the aggregate, and says so in a comment at the point where it matters. That is what makes
the proposal door extractable as a table at all, and `docs/topology.json` is the extract —
with each door labelled for what it is.

### What this forbids

Four things, and each is already true — the value of writing them down is that a future
change would otherwise break the property without anyone noticing:

1. **`Undefined` must never depend on data — on the proposal door.** The moment whether a
   *behaviour* exists depends on the aggregate's contents, the menu stops being a table and
   becomes a program. Guards may depend on data; the menu may not. The fact door is exempt
   because it is not a menu: it interprets evidence against a persisted intent, and that is
   what it is for.
2. **The domain performs no I/O, reads no clock, and uses no randomness.** Context is
   *given* to it (ADR-013). This is why ADR-016 §2 keeps the deadline comparison on the
   council's side rather than the domain's — that was argued as a provenance matter, and it
   is the same constraint seen from another angle.
3. **States stay finite and enumerable.** Ten variants today. A state carrying unbounded
   data that transitions *branch on* would end the property; a state carrying unbounded data
   that only guards read would not.
4. **Guards stay comparisons over enumerable inputs.** Fee against a ceiling, capacity
   against a headcount, a boolean flag. Not "ask a service whether this is allowed" — a
   guard that reached outside would be a transition deciding its own admissibility, which
   ADR-013 already refuses for a different reason.

### When data becomes a state, and when it stays a guard

Rule 1 forbids a behaviour whose *existence* depends on data, which invites the obvious
worry: does every data condition now become a state? Eight states for "verified AND deposit
paid AND insurance confirmed"? No, and the line is sharp:

> **Promote data to a state when it changes *which behaviours exist*.**
> **Leave it as a guard when it only changes *whether a behaviour succeeds*.**

Applied to what already exists:

| Condition | Changes the behaviour set? | Form |
|---|---|---|
| the slot has been verified | yes — `Book` appears | a **state**, `AwaitingBooking` |
| the fee exceeds the principal's ceiling | no — `Book` exists and is denied | a guard |
| the room is not wheelchair accessible | no | a guard |
| the room is too small | no | a guard |

Three guards, one state. The heuristic reproduces the current design without having been
consulted about it, which is the reason to trust it.

**The distinction in plainer terms.** A state decides *what can be asked for here* — the
menu. A guard decides *whether this particular ask succeeds* — the answer. A cash machine
showing "insert card" is not refusing your withdrawal; there is no withdrawal screen. Once
your card is in, "insufficient funds" is a refusal: the option was there, you took it, the
answer was no.

So the test is: **if the data were fixed, would a new option appear on the menu, or would the
same option start working?** Verifying a slot makes `Book` *appear* — a new menu. Topping up
a fee ceiling makes the existing `Book` *succeed* — the same menu, a different answer.

**Two further rules, which settle the cases the first one leaves open:**

- **Two candidate states with identical menus are one state.** This is what stops promotion
  from exploding: "verified" plus "deposit paid" plus "insurance confirmed" is not eight
  states, because `VerifiedWithoutDeposit` and `VerifiedWithDeposit` would offer the same
  four behaviours. Same menu, so one state and a guard on `Book`.
- **State belongs to the resource, never to the caller.** A booking's state is a fact about
  the booking; authority is a fact about who is asking. The same booking in the same state
  offers `Book` to a principal who may book and refuses one who may not — so if authority
  were a state, the state would change depending on who looked at it. Anything that varies by
  requester is therefore a guard by construction, which is why `may_book` and the fee ceiling
  are guards and not states.

**When the three still leave it ambiguous, choose the state.** The two errors are not
symmetric. Wrongly choosing a guard produces the failures below — a booking at a price nobody
approved, a room too small — and no test necessarily catches either. Wrongly choosing a state
makes the topology noisier and the design more tedious to read. One is a silent correctness
failure; the other is untidiness.

**Why the extra state is worth its cost, twice over.** Both of these are defects this project
has already had, and both are structurally impossible under a state rather than test-covered
under a guard.

*A 22-seat room booked for 25 people.* The slot is verified for 20 attendees; the headcount
is then raised to 25. As a state, `UpdateRequirements` leaves `AwaitingBooking` for
`NeedsRevalidation` and `Book` **stops existing** — re-verification is the only way forward,
and it refuses. As a guard, the aggregate still carries availability for the same room, so
`Book` still exists and still passes; preventing it requires remembering to clear a field
whenever requirements change, and forgetting once is silent.

*The fee that moved.* A slot is verified at £45 against a £100 ceiling. The council later
raises it to £90. As a guard, the only fee available is the one just read: £90 is under £100,
so the booking succeeds at a price the principal never approved. The guard asked *"is it
under the ceiling?"* when the question that mattered was *"is it the price that was agreed?"*
— and it **cannot ask that**, because there is nowhere for the agreed price to live except
the availability record that just changed. As a state, `AwaitingBooking` carries
`verified_fee`, `Book` compares £90 against £45, and the refusal sends the booking back to
re-verification where the new price is decided on deliberately.

That is the general form, and it is the reason this rule exists rather than merely tidying
the graph: **a state can carry evidence of its own precondition; a conditional inside a
behaviour cannot.** The failure is not that the guard is harder to write correctly — it is
that the correct check is unwriteable, because the value it needs was never kept.

The failure mode to watch for in review is small and quiet: someone adds `if data.is_some()`
inside a behaviour under time pressure. No test fails. `docs/topology.json` still generates
and still looks total — it has simply stopped being true of anything but the fixture.

### What is explicitly out of scope

**External effects have no fixed-function analogue and do not need one.** Calling the
council is at the boundary's edge, not inside the transition logic: the topology records
*that* an edge reaches outside, never how. A hardware target would substitute an actuator
command and inherit ADR-014 unchanged — record what you are about to command before
commanding it, because a crash mid-motion leaves you needing to know what you asked for.
Higher stakes than a room booking, identical discipline.

**Guard synthesis is not attempted.** Exporting the comparisons and their data sources is a
further step nobody has asked for. The topology is the part that carries the safety claim.

### What it costs

Almost nothing, because everything it forbids was already forbidden for other reasons. The
one real cost is a standing constraint on future design: a tempting shortcut where a
behaviour's *existence* depends on data — "`Book` only exists once a venue is verified,
which we can tell from `availability.is_some()`" — is closed. That case must be modelled as
a distinct state, which is what `AwaitingBooking` already is.

That is not a workaround. It is the thesis: if a behaviour comes and goes with the data, the
data is a state and should be named as one. The section above draws the line, so the
prohibition comes with somewhere to go rather than only somewhere not to.

### Alternatives rejected

**Say nothing and keep the property by accident.** It survives exactly until someone has a
good reason to make `Undefined` conditional, and then it is gone with no test failing —
`docs/topology.json` would still generate, and would still be total. It would simply no
longer be true of anything but the fixture. The property needs a stated owner.

**Target hardware now.** Nothing in the POC needs it, no requirement asks for it, and
building a synthesis path for a booking system would be the clearest possible case of
solving a problem nobody has.

## ADR-019 — Giving up is a pursuit decision, not a state and not an outcome

Decided 2026-08-24 with the project owner, during slice E's planning. This amends the
`reconciliation_failed` reachability argument in ADR-012 and supersedes the `NeedsHuman`
transition drawn in spec §7 (three lines of its ASCII diagram — the spec names `NeedsHuman` in
no invariant, no acceptance test, and no Definition-of-Done item).

### Context: the state that ate the reason

When reconciliation exhausts its retry budget, the design to date moved the booking to
`BookingState::NeedsHuman` and cleared `active_effect`. Three review rounds against slice E's
plan established what that costs, concretely:

- `NeedsHuman` is a unit struct with **zero outbound edges across all three doors** — 56 cells,
  not one arrow — and no fields. It cannot say *why* a human is needed, cannot say *which effect*
  it gave up on, and cannot say *which state* it interrupted. A late `BookingExists` from
  `BookingInProgress` means `Booked`; the same fact from `CancellationRequested` means "now
  cancel it". After the move to `NeedsHuman` those are indistinguishable, so a late fact either
  gets discarded (the booking is stranded while the council holds a live room) or lands on the
  wrong side of a cancellation the user already made.
- No finite lease can rescue this with concurrency control: a reconciler can send a request,
  lose its lease, **die**, and the request still lands at the council. Whatever design exists
  must therefore be correct when authoritative facts arrive *after* someone gave up.

By ADR-018's own rule — promote data to a state when it changes **which behaviours exist** —
`NeedsHuman` is not a state today, and the accurate form of the argument matters: it is not that
the menus involved are empty (`BookingInProgress` has a deliberately pending `Cancel` cell that
slice F opens, so they are not), it is that **escalation changes no menu.** Every behaviour an
in-flight state has before giving up, it must still have after — a user may cancel a booking we
are unsure about — and `NeedsHuman` offered nothing those states lack. A promotion that changes
no behaviour is a label wearing a state's costume, and this one's costume destroyed information.

### Decision

**1. Exhaustion does not move the booking.** The state stays `BookingInProgress`,
`CancellationRequested` or `CancellingBooking`, and `active_effect` stays set — because that is
what is true. The council may well hold the booking; asserting anything else is the overclaim
this project exists to refuse. A late authoritative fact then lands through the **existing**
fact-door arms, whose per-state meanings are exactly the distinctions `NeedsHuman` erased.

**2. Exhaustion is recorded on the effect intent, on its own axis.** Not as an
`EffectStatus`. The status column's other values are preparation (`Prepared`), our own
ambiguity (`Unknown`), or provider determinations (`Confirmed`, `Rejected`, `Absent`) —
`Abandoned` was the one value that was a fact about *us*, and slice C2 already spent a review
round keeping it from being confused with `Absent`. The confusion is structural: one column,
two provenances. So the pursuit facts get their own columns on `effect_intents`:

```text
attempts_started    INTEGER   calls begun (bounds the loop across crashes)
attempts_finished   INTEGER   calls that returned control, answer or not
next_attempt_after_ms INTEGER when the reconciler may ask again
escalated_at_ms     INTEGER   NULL until we gave up
escalation_attempts INTEGER   the count at the moment we gave up
```

The status stays `Unknown` — which is the truth, and which is what keeps the intent
finalisable through the ordinary path when the answer eventually arrives. **No store guard is
weakened**: `finalize_effect`'s `EffectStillActive` gate, the terminal-contradiction check and
`Booking::coherent` all stand exactly as they are, because nothing about this write goes near
them. `EffectStatus::Abandoned` is removed — not because the remaining column is a pure
provenance partition (`Unknown` is ours too: a knowledge state), but because `Abandoned` was a
*decision* wearing an outcome's terminality, and its terminality is what refused the late fact.
Statuses describe what is known; decisions about pursuit live on the pursuit axis.

**3. Giving up means chasing slowly, not never asking again.** Escalation pushes
`next_attempt_after_ms` far out (hours, not seconds) and flags the intent for a human. The
asking never stops, because the *exit and the stop condition must not be the same query*: the
council is pull-only, so a fact only ever arrives because something performed a lookup. A
design in which abandonment silenced the reconciler would replace an ending with a promise its
own stop condition guarantees is never kept.

ADR-016 is what makes this converge rather than spin: past the effect's deadline, the first
lookup that reaches the council gets a definitive answer — `BookingExists` or a tombstoned
`DefinitivelyAbsent` — and either one finalises the intent through the existing arms. Every
story ends the moment the council is reachable again. The only story that never ends is a
council that is unreachable forever, and no design ends that one without asserting something
nobody established.

**4. Escalation touches only the intent row.** No booking commit, no version bump, no
`audit_events` row. The marker columns are the durable record, and they are queryable:
*"escalated and unresolved"* is one indexed predicate, which is the human queue. This also
makes escalation free of the aggregate's compare-and-set — a repeated escalation cannot starve
a late fact's commit by churning the version, because it does not touch the version.

The write is conditional (`WHERE escalated_at_ms IS NULL AND status IN ('Prepared','Unknown')`),
so it is once-only and a lost race against a settling fact is a no-op rather than an error.

**5. The system-event door stays, and gains an honest range.** The domain still classifies
`ReconciliationExhausted` — is this state waiting on this effect, is the aggregate coherent —
because that is a domain question (ADR-012's provenance argument survives untouched). What
changes is the answer's shape: the door returns *"record this against the effect"* rather than
a `TransitionPlan`, because `TransitionPlan`'s two variants both carry a next state and the
truthful next state is "none". A plan type that must lie to say "nothing moves" is the wrong
type. This follows FactResolution's precedent: when a door's range grew, the range got its own
type rather than a bolted-on variant every other door must refuse.

**6. The human queue is a question queue, not an ownership ledger.** A booking appears in it
because a question needs a person (*"what happened to this effect?"*) and leaves it when the
question is answered — including when a late fact answers it and automation resumes. In M4
nobody is notified, because no human channel exists until **M6** (not M5 — an earlier draft of
this decision misdated it, and the correction matters because the whole "promote it later"
argument hangs on when a human can actually act).

**7. `BookingState::NeedsHuman` is retained, unreachable, and conditionally disposable.** It is
kept only until the human-behaviour set is designed. ADR-018's heuristic predicts promotion
*only if* escalation changes which behaviours exist — and if M6/M7's human actions turn out to
attach to *any* in-flight booking rather than only to given-up ones (the likelier design), the
menus never differ, the promotion never arrives, and the variant is deleted rather than
promoted. Its empty menu must not be read through ADR-018's collapsing rule as "merge it into
`Cancelled`" — both menus are empty, but that rule is for live states, and conflating "we do
not know" with "it is cancelled" is the exact confusion `Abandoned`/`Absent` already refused.

### What this must never be read as

The escalation reason is a fact about **our accounting** and nothing else. Its vocabulary is
"N attempts produced no answer" — never "the council is gone", never "the deadline passed",
never anything that could invite a reader (or a future match arm) to treat abandonment as
absence. Only the council determines absence (ADR-016 §2), and the entire point of keeping the
booking in-flight is that we are *still waiting to be told*.

### What it costs

**An in-flight state can now be old.** Before, `BookingInProgress` implied active pursuit;
now it may be hours old with a marker. Queries that need the difference have it — the marker —
but the state name alone no longer carries recency, and anything that assumed it did is wrong.

**The audit trail thins at the aggregate level.** A booking whose effect was escalated shows
no aggregate event for it; the record lives on the intent row. Answering "what happened to
BKG-1001" requires the join. Accepted deliberately: the alternative was a version-bumping
no-op commit whose audit row could name neither the effect nor the reason.

**Slice F must respect the marker.** `BookingInProgress + Cancel → CancellationRequested`
(F's cell) will be proposable on an escalated booking. That is correct — a user may cancel a
booking we are unsure about — but F's cancellation handling inherits an intent being chased
slowly, and must not assume the cadence.

### Alternatives rejected

**Keep `NeedsHuman` and make it remember** (effect id, originating state, reason). Rejected:
a state carrying a copy of the state it came from is one fact in two places, the defect this
project has now caught eight times, and every recovery path would need to re-derive which
story it interrupted from stored copies rather than from the state itself.

**`EffectStatus::Abandoned` as the marker.** Rejected as unwritable, proven against the real
store: `finalize_effect` refuses any write whose aggregate still names the effect
(`EffectStillActive`), and a terminal `Abandoned` makes the late `BookingExists` — the
decision's whole point — die as `ContradictoryFinalisation`. Making `Abandoned` non-terminal
instead would split `is_terminal` into two meanings and weaken the one predicate the
contradiction check depends on.

**Stop asking after giving up.** Rejected: the exit and the stop condition become the same
query, and the design's only exit is an event it has just guaranteed will never be produced.

## ADR-020 — In-flight cancellation, and recovery that finishes what is still wanted

Decided 2026-08-25 with the project owner, during slice F planning (four review rounds:
redesign → fix → fix → build as planned). Closes M4's last topology cell and gives recovery
the execution leg the pursuit axis was built to fence.

### The edge: `BookingInProgress + Cancel → CancellationRequested`

The one PENDING cell since PR #3, now in LOCKED. The transition is **local**: nothing is
sent and no effect is minted, because you cannot cancel what may not exist. The state keeps
waiting on the SAME booking intent and records **who asked to cancel**
(`CancellationRequested.cancelled_by`) — the cancelling authority is only in hand at this
proposal, and the cancellation effect is minted later, by the existing fact-door handoff, if
and only if the booking is found. The canceller is never reconstructed from the booking's
principal: booker and canceller need not be the same person.

`BookingEffect::CancelBooking` gains `principal`, copied from the state by the handoff arm
and supplied directly by the `Booked + Cancel` arm. This is a deliberate stored-plan format
break in B3b's precedent: a pre-F `CancelBooking` row fails to decode rather than decoding
to an unattributable plan (the gap ADR-017's attribution rule left flagged in the code). The
council's wire body is unchanged — attribution is a local record. The proposal's `reason`
remains **discarded**, exactly as on the `Booked` arm: `TransitionAudit` records the
driver's name, not its payload, and this slice makes no audit-schema change. Recorded as an
accepted gap, not an oversight.

### The pursuit table: what recovery may still cause

Whether an in-flight effect is still *wanted* is a fixed, total, per-state answer — ADR-018
promotion applied: it changes which pursuit behaviour exists, so it is state, never a guard.

| state | in-flight intent | pursuit |
|---|---|---|
| `BookingInProgress` | Book | send and resolve — the booking is wanted |
| `CancellationRequested` | Book | **resolve only** — the desire was withdrawn; recovery must never *cause* the booking, only learn its fate |
| `CancellingBooking` | Cancel | send and resolve — the cancellation is wanted |

Proposing `Cancel` mid-flight therefore *withdraws the booking's wantedness* the moment
`CancellationRequested` commits. Withdrawal is best-effort and deadline-bounded: a send
decided against a stale load can still land, and the handoff arms exist to cancel exactly
what then exists.

### The dispatch rule: query first, resend what is still wanted

A claimed `Prepared` intent (never attempted) is **sent** — the mark
(`note_attempt_started`, before the wire) then the call, as Phase B always worked. A claimed
`Unknown` intent is **queried first**; then:

- a definitive, verifier-passing answer settles through the unmodified fact door;
- an **authenticated `NotYetVisible` bound to this attempt** — signature verified against
  the pinned council key AND `reply.effect_intent_id == attempt.id` — while the state's
  pursuit says the effect is still wanted → **resend the persisted plan under the same
  identity and expiry**. Provider idempotency (slice D) converges the delayed-first-request
  race; ADR-016's deadline bounds the window;
- everything else — bad or missing signature, wrong identity, `ProtocolConflict`,
  `Unavailable`, garbage, timeout, a dead socket — is an unusable reply and drives nothing.

`NotYetVisible` deliberately does not enter the fact door: it is a pursuit signal, not a
fact, and no `VerifiedProviderFact` variant exists for it. The resend privilege rests on the
council's signed, identity-bound word — never on the loop's opinion — and the classification
lives where the pinned key already lives.

**Why the rule is not "execute iff `Prepared`":** the attempt mark is durable before the
wire (ADR-014 one level in), so a crash between the mark and the send leaves an `Unknown`
intent the council never heard of. An ask-only `Unknown` path rides that intent to its
deadline and — for a cancellation — `CancellingBooking + EffectAbsent → Booked`: the
cancellation silently lost. The mark-before-send gap cannot be closed by moving the mark;
recovery must be able to resend. (Slice F plan review, round 1, CRITICAL.)

**Accounting:** each wire call is an attempt. A turn that queries and then resends records
two `attempts_started` and two `attempts_finished` — ADR-019's "calls begun / calls that
returned control" contract holds verbatim; two externally fallible calls are two durable
marks and two crash windows.

### The owner's product decision: recovery finishes the job

Asked 2026-08-25: a booking that crashed before the call — durable intent, council
verifiably holding nothing — is **completed** by recovery (query → authenticated not-yet →
resend → `Booked`). The intent is the owner-authorized durable record, and recovery
completes whatever the current state still wants. This is sound under and compatible with
ADR-014/016/019 but **not compelled by them**; it replaces slice E's recorded rule that an
undelivered booking expires and returns to re-proposable, and it narrows the reconciler's
former self-description from "asks without causing" to "causes only what the state still
wants, on the council's signed word". The deadline-passed variant is preserved: a create
that outlives its deadline before recovery runs still fails closed to `AwaitingBooking`.

### Alternatives rejected

**Execute iff `Prepared`, ask iff `Unknown`.** Rejected — the stranded-cancellation window
above. **Resend on any failed query.** Rejected: resending on silence acts on nothing;
the privilege requires the council's signed, identity-bound statement. **An unsigned or
wrong-identity `NotYetVisible` as authorization.** Rejected and negatively tested: a signed
not-yet for identity A must never authorize resending identity B. **Carrying the canceller
on the successor only.** Rejected: by handoff time the cancelling authority is out of
scope; the state is the only honest carrier across the ambiguous window.

## ADR-021 — The wire: M5's decisions before M5's code

Decided 2026-09-01, during M5 planning (six review rounds: redesign → four fix rounds →
build as planned; an independent paper-reviewer contributed the HTTP-contract findings).
Recorded before implementation, per the plan's own ordering rule.

### The application boundary comes before the HTTP adapter

Two structures carry the M5 gate's "handlers do not mutate directly", so it is a fact
about what code can express rather than a review item:

- **`BookingApi`**, the facade in `townhall-service`: handlers hold it and nothing else,
  and its complete mutation surface is `create(BookingId, BookingRequirements)` and
  `propose_at(id, expected_version, proposal, authority)`. Reads: projection (with the
  domain's exported behaviour menu), audit projection (service-owned type, not the
  store's), catalogue/availability ports, and the reconcile trigger.
- **The two-crate split**: `crates/townhall-http` (handlers, DTOs, the status mapping,
  the loop driver; depends on service/domain/types plus axum/serde/tokio and never on a
  storage or provider crate) and `services/townhall-server` (the composition-root
  binary; instantiates the store, denial log, council client, config, authority
  resolver). Cargo has no per-target dependency table, so the boundary needs two crates
  to be real — the same crate-graph enforcement ADR-012 uses for evidence types. A
  source scan remains as a secondary tripwire only.

### `propose_at`: the caller's expected version joins the trusted turn

Spec §9.2 demands the stale writer fail. A handler-side load/compare/propose cannot
deliver that (the stale request is silently rebased between the compare and the turn's
own load), so the expected version travels into the coordinator: refused before
classification when it does not match the load, enforced by the CAS at exactly that
version after it, both surfacing as a typed `PreconditionFailed { current }` → 412 with
the fresh ETag.

**Replay means stale, on this surface.** M4's replay-first prepare (the
lost-acknowledgement guarantee) returns a rival's committed intent without reaching the
CAS — under an expected version, a replayed prepare means *this caller performed no
mutation and the world already moved*, and the honest HTTP answer is 412, never a 202
claiming work this request did not do. The store's replay contract and the versionless
in-process `propose` (which correctly answers `Unresolved` to its own retries) are
unchanged; the two entry semantics are pinned side by side by one paused-race test.

### The cadence-persistence repair (a live defect since slice E)

`note_attempt_finished` persisted `MIN(cadence, now + MAX_CADENCE_MS)` where callers pass
a **duration** — the stored "next attempt" was milliseconds after 1970, always past,
always due. The retry cadence never gated a retry; escalation's write (`now + cadence`)
was correct, which is why every existing assertion held, and every test advanced its
clock past the cadence anyway — the missing assertion was the not-yet-due boundary
itself. One parameter, two meanings: the recurring defect in its purest form. The
parameter becomes a validated duration end to end; the store persists
`now + min(cadence_ms, MAX_CADENCE_MS)` under its own clock; clock-pinned tests assert
the stored timestamp exactly and the due/not-due boundary on both sides. `Retry-After`
derives from a new store-owned `retry_hint_ms` (the store's clock — computing `row − now`
in the service would mint a second clock, ADR-016 §1's own lesson), converted to whole
seconds by ceiling with a 1s floor. Cadences, lease, and budgets become one validated
`PursuitConfig` shared by coordinator and reconciler; the single sanctioned zero is the
re-classification budget, as a test seam.

### Status-mapping decisions the spec table does not settle

- **Provider unavailability is 503, not 502**, including the catalogue routes — §10.2's
  own row is the contract; deviating to 502 would be a private improvement on it.
- **`FeeExceeded` names its ceiling**: `FeeExceededAuthority` (403) vs
  `FeeExceededRequirement` (422), authority winning when both are exceeded — and the two
  stable names keep ADR-017's denial dedup from merging a grant story with a data story.
- **Unavailable availability is not missing availability**: the source answers three
  ways, and `Denied(FactsUnavailable)` (could not ask — 503, before any durable intent)
  is distinct from `VenueFactsMissing` (asked and answered nothing — 422). The domain
  distinguishes two shapes of its input, not the network. `Unresolved` stays 202: after
  Phase A there is a durable intent and the chase owns it.
- **`ServiceError::UnexpectedPlan` is 500**: an internal invariant failure, never
  dressed as provider trouble.

### The reconcile trigger is exempt from `If-Match`, by classification

Spec §10 lists the demo/admin reconcile endpoint and §10.1 requires `If-Match` on
mutations. The trigger routes to `due`/`attend` — it asserts no expected state, its claim
is atomic, and its facts are version-fenced below — so an HTTP precondition would be
theatre. Recorded as a deliberate exception: `If-Match` is required on `behaviours/*`
except `reconcile`; a **present** `If-Match` where no precondition applies (create,
reconcile) is refused 400 rather than ignored. ADR-012's supersession of
`BookingProposal::Reconcile` stands — the endpoint is not a proposal and never was.

### Amendment to ADR-017 point 4: the private-field half

ADR-017 deferred the verdict's anti-forgery ceremony — "private fields and no
`Deserialize`" — to M5. The no-`Deserialize` half lands as static assertions
(`BoundaryOutcome`, `VerifiedProviderFact`, `VerifiedAuthority`; `Verified<T>` already
had one). The private-field half is **amended away, with reasons**: Rust cannot make an
enum's variant fields private without destroying the trusted half's legitimate
construction (the coordinator builds outcomes) and the mapper's exhaustive match — and
the forgery the ceremony targeted is already unrepresentable three other ways: the crate
graph (untrusted crates cannot name `bld-kernel`), the serde assertions (a verdict cannot
arrive over a wire), and the facade (the server sees only what it returns).
`VerifiedAuthority`'s constructor ceremony belongs to M7, with its issuer.

### DevAuthority, contained until M7

A fixed two-token allowlist (`dev-lucy` permissive, `dev-marco-restricted` refused
booking and cancellation authority) — nothing pattern-derived, so "unknown bearer" is a
real 401. Behind a cargo feature AND a mandatory startup flag; absent either, the server
refuses to start. `X-BLD-Delegation` is reserved and refused loudly (400) until M7's
envelope exists. M7 replaces the resolver in the composition root.

### Costs accepted

The facade is a second surface over the coordinator to keep honest forever. The
two-crate split adds a crate whose whole job is wiring. 412-on-replay is stricter than
202 for a client retrying its own request with a stale tag — the client re-reads and
sees the in-flight truth, which is the point of preconditions. The reconcile exemption
is a documented hole in a blanket rule, chosen over a meaningless precondition.
