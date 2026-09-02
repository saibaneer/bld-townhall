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

## ADR-022 — Bookings acquire an owner, and the wire acquires a lookup

Decided 2026-09-01 with the project owner, during M6 planning (eight review rounds:
revise ×7, then build as planned). Not a spec milestone — M5.1, split out of M6 on the
owner's word after both plan reviewers independently found the second half of it.

### What planning M6 found

Bookings had no owner. `create_booking` resolved a `VerifiedAuthority` purely as a
turnstile and discarded it; `NewBooking` had nowhere to put one; `may_cancel` was a
*capability flag*, never a comparison against a resource. The principal was recorded
into cancellation plans and never read back for admission, because there was nothing to
compare it to.

So **any principal holding `may_cancel` could cancel any booking whose id they could
name.** That was tolerable while one person drove the system with curl, and stops being
tolerable the moment M6 opens a channel anyone can text.

Second finding, reached independently by both reviewers: spec §14.1 requires
cancellation to follow an *"authoritative resource lookup"* and §15.2 cancels by
**council** reference (`CANCEL TH-92718`), but the router indexed only by internal
`BookingId`. Conversation memory cannot substitute — §3.1 makes it a routing aid, and
M6's session state is deliberately non-durable, so after a restart there would be no
candidates at all.

### Ownership is admission; ADR-020 is attribution

The first draft of this proposed a single `principal_id`, checked as "only the owner may
act". That contradicts ADR-020, which says outright that *"booker and canceller need not
be the same person"* — and would have hollowed
`a_refusal_on_a_cancellation_intent_is_attributed_to_the_canceller`, the test PR #16's
HIGH finding demanded, which deliberately has Marco cancel Lucy's booking.

The two answer different questions:

| Question | Answer | Where |
|---|---|---|
| Who is *recorded* as having asked? | never assume the booker; read the authority | domain / persisted plan. **ADR-020, unchanged** |
| Who may *see* it? | its owner | facade |
| Who may *ask* for a transition? | its owner, or a delegate | facade |

A delegate cancelling on Lucy's behalf is precisely the canceller-who-is-not-the-booker
ADR-020 requires recorded correctly, so ownership supplies its real use case rather than
removing it. M5.1's rule is the degenerate case — `requesting == owner` — because with no
delegation type there is no other honest route.

**Enforcement is the facade.** Not the handlers (bypassable by the next in-process
caller — M6's orchestrator is exactly that) and not the store (too late to distinguish
invisible from absent). The coordinator and domain are untouched, which is what keeps
ADR-020's test green *and meaningful*.

### Nullable ownership, and why two earlier arguments were wrong

`ALTER TABLE bookings ADD COLUMN owner_principal TEXT` — nullable, and that is the
security property.

Two drafts used `NOT NULL DEFAULT '@orphan'`. The first claimed `PrincipalId::new`
rejects a leading `@`; it does not — the type is a macro-generated newtype that validates
nothing, and `#[serde(transparent)]` bypasses every constructor anyway. The second argued
unreachability from the server's fixed token allowlist: true today, and the wrong kind of
true, because it depends on a configuration list that widening would silently un-conceal.

`NULL` needs no argument. `owner_principal = ?` never matches it whatever the parameter,
and no `PrincipalId` can serialise to it. The guarantee does not generalise, so the query
form is a rule: **every externally visible query selects from `bookings` under a positive
base-row predicate**, never a negation, never a subquery, never a join whose ownership
condition sits in `ON`. (`NULL NOT IN (…)` is unknown; `NOT EXISTS (… WHERE
owner_principal = ?)` is *true* for an orphan and would include it.)

Legacy rows keep `NULL`: unreachable through `load_visible` for every principal, still
readable through the unscoped `load` that recovery needs, since a reconcile pass has no
principal and must still finish an in-flight effect.

### Scoped load rather than comparison

`load_visible(owner, id)` puts the ownership predicate beside the id in one `WHERE`.
There is deliberately no `if aggregate.owner == access.principal` anywhere on a visible
path: a foreign row does not come back, and `NotFound` was already the 404 path. The bug
where someone forgets *the comparison* therefore cannot exist, because there is no
comparison — and that failure would have been silent, since a missing check looks exactly
like a passing one.

**What this does claim, exactly.** The facade has *no* unscoped read left — scoping every
visible path turned its private unscoped helper into dead code, and the compiler said so,
so it is gone. The repository still exposes an unscoped `load`, deliberately, because
recovery needs it: a timer-driven reconcile pass has no principal to scope by, and a
migrated NULL-owned row must still be attendable.

So the remaining mistake is "reach past the facade to the repository", which is visible at
the call site and blocked by the crate graph for the wire, rather than "forget a comparison
several lines below the load", which is invisible. A smaller target, not an empty one.

### 404, not 403 — and where the concealment stops

Visibility failures answer **404**; 403 confirms the resource exists, which is the oracle
someone guessing council references wants. `ensure_visible` runs in the handler **before**
both header gates, because 400-on-malformed-`If-Match` and 428-on-missing are statements
about a resource too. Reconcile keeps ADR-021's precondition exemption and loses its
visibility exemption — a precondition exemption is not a licence to be an authenticated
existence oracle.

**Accepted residual:** a duplicate `create` on a foreign id still reveals that the id is
taken (409 `identifier unavailable`, no version, no `ETag`, no owner). Under a
caller-chosen globally unique primary key that bit is unavoidable, and 404 would leak the
same bit while misdescribing a `POST` to a collection that does exist. Removing the oracle
needs a different identity allocation (server-generated ids, or owner-scoped uniqueness),
not a different status code. A reviewer proposed the 404 and, on re-review, upheld the 409.

### The lookup surface (a spec §10 amendment)

`GET /booking-intents?booking_ref=…` and `?cancellable=true`, both principal-scoped in
SQL. Neither filter, both, or `cancellable=false` is **400** — an unfiltered listing is not
a surface this milestone offers, and `LookupQuery` is a closed enum so it is not
representable rather than merely rejected. A foreign reference returns an **empty list**,
not 403. Rows order by `created_at_ms`, then `id`. No collection `ETag`: a list has no one
version, and shipping one invites its use as a precondition.

`cancellable` means *"currently offers `Cancel`"*, from the domain's own `proposal_menu()`
— not "not yet cancelled", which is a different and wronger question, since a booking
mid-cancellation is not yet cancelled and must not be offered again. The filter runs after
decode: filtering in SQL would hardcode state names in the store, drifting the moment the
menu changed. Cost accepted: a principal's bookings are all decoded per lookup.

### A third dev token (amending ADR-021's two-token allowlist)

`dev-priya-nobook` — Lucy's £50 ceiling, `may_book: false`, `may_cancel: true`.

Needed because ownership broke the existing 403 test, and the obvious repair does not
work: Marco's £10 ceiling means `verify-slot` refuses *his own* booking with
`FeeExceededAuthority` before `book` could ever ask about his capability, since every
seeded slot costs £45. Priya is restricted in exactly one way, so a refusal on her booking
can only be `BookingAuthorityRequired`.

This makes the suite stronger than it was. Marco is restricted twice over, so the old
assertion only landed on the right error because `resolve_book` happens to check
`may_book` before it binds the facts — reorder those two lines and the test kept passing
while asserting a different guard. The fee-ceiling assertion is kept as its own test, so
splitting the principals lost no coverage.

Priya is also the foreign caller in every visibility test, deliberately: Marco cannot
cancel anything at all, so an implementation that mapped "lacks the capability" to 404 —
checking ownership nowhere — would have satisfied a Marco-based suite.

### Two things the build found that the plan had not

**The facade layer was untested.** Deleting the facade's own ownership check left the
entire wire suite green, because every witness reached it through the handler's preflight.
`the_facade_conceals_a_foreign_booking_without_help_from_a_handler` drives `BookingApi`
directly with no HTTP — the position M6's orchestrator will occupy — and does catch it.

**A witness was vacuous.** A foreign-cancel assertion named `cancelled_at_ms`, a column
that does not exist (it is `cancelled_by`), and passed anyway because the test harness's
SQLite shim ended `.unwrap_or(0)`, turning any malformed query into `0 == 0`. The shim now
panics on a failed query. A witness that cannot fail is worse than none, because it
occupies the space where a real one would go.

### Costs accepted

Every externally visible facade method grows an authority parameter — 23 `NewBooking`
initializers and both `BookingRepository` implementations changed. `audit_events` still
reads by id alone after admission, which is safe because ownership is immutable **through
the repository's commit API** — the aggregate `UPDATE` never names `owner_principal`, so no
committed transition can change hands.

That is an application-level guarantee, not a database constraint, and the distinction is
worth recording rather than blurring: the pool is public, so anything holding it could
rewrite the column directly, and a caller doing so between `audit`'s admission check and
its unscoped `audit_events` query would deliver the trail to a principal who no longer owns
the row. Nothing in this workspace does that, and the tests that orphan rows on purpose
rely on being able to. Enforcing it at the database boundary — a trigger refusing any
change to a non-NULL `owner_principal` — is the fix if ownership ever becomes transferable
by anything other than a migration. The
duplicate-create bit above. And M7 inherits a named debt: ADR-020's attribution now has no
public path exercising it, so M7 owes a **facade-level** delegated-cancellation test in
which Lucy owns the booking, Marco requests it under a verified delegation naming that
booking, his agent is the actor, and the persisted plan records **Marco** — plus the
delegation type itself (grantor, beneficiary, actor, audience, exact resource, permitted
behaviours, constraints, expiry, revocation).

## ADR-023 — The human edge's two crates: the channel that decides nothing, the gateway that owns a socket

Decided 2026-09-02 with the project owner. M6 split into M6A (this) and M6B (the
orchestrator and simulator binary), matching the reviewer's independent
recommendation; six plan review rounds (revise ×5, then build as planned, with
one prescription declined — below).

### The trust split, enforced by two different dependency rules

`townhall-channel` is spec §3.2's *"trusted parser/normalizer only"*: it
normalizes, bounds, dedupes, classifies and transports, and can answer nothing —
its manifest excludes every mutation surface **in dev-dependencies as well as
normal ones**, because its tests need no server and the exemption the gateway
needs would otherwise let a `#[cfg(test)]` module reach the store.
`townhall-gateway` is the *"untrusted driver"*: its only route to a booking is a
socket, its routes are hard-coded **on purpose** (a generic client is M9's gate,
and building it now would claim M9's deliverable), and its DTOs are written
independently of `townhall-http`'s so the wire contract is tested rather than
assumed. The tripwire reads `cargo metadata`'s resolved graph, not manifest text.

### 202 is the fault path — a four-revision-old error, corrected

Plan revision 2 asserted *"202 Accepted is the normal case"*. False: an answering
council settles synchronously (`run_proposal` prepares `BookingInProgress`,
invokes, verifies, settles and returns `Committed` in one call), and the existing
`lucy_books_a_room_over_http` proves it by destructuring the proposal's own
return as `Booked`. 202 requires the answer to go missing. Consequences built in:
every acceptance test **arms the drop fault and asserts it fired** (`consumed ==
1` — the fault id is an index, legitimately `0`, so "it armed" witnesses
nothing); and `propose_at` returns `Accepted` **before** convergence, which is a
separate call, because the person who texted is owed *"Booking now"* immediately
and the outcome later as a differently-classified message. A gateway that
blocked until converged would make that two-message shape unexpressible.

### The channel does not parse `BOOK`, and a word is not a reference

Spec §14 forbids the channel owning booking vocabulary, so `BOOK date=…` is
`Freeform` — the proposer reads its fields, in the position M11's model will
occupy. The seam test (every classified arm, pinned as data for M6B's dispatcher)
caught the first real bug of the slice: `"Cancel it"` classified as `CANCEL`
with reference `"it"`, which would have had the dispatcher telling Lucy a booking
named "it" does not exist instead of asking which booking (§14.1). The fix is one
deliberate clause — a resource argument must contain a digit — and no richer,
because anything more is the channel learning the council's namespace. A missed
digit-free reference degrades to `Freeform`, where the whole text is still in
hand.

### Redaction: a digest of a low-entropy value is an encoding of it

`InboundBody`'s `Debug` renders the length only. From M7 a body can be `YES
7312` — ten thousand candidates — so an unkeyed hash is readable by enumeration,
and a keyed one buys key management for correlation that `InboundIdentity`
already provides safely. The three `Debug` renderings are pinned by equality
against complete strings, not by absence-of-a-guessed-algorithm, which is not a
writable test. A claimed `bld-types` precedent for the accessor ceremony was
checked, found not to exist, and withdrawn.

### `InboundBody` exists because `BoundedString` truncates

`bld_types::BoundedString::truncating` silently drops everything past 512
bytes; reusing it would have capped every SMS at under a third of the documented
1600 **scalars** while returning success. The new type is fallible, and the test
that discriminates is byte-for-byte round-trip of 600 emoji (2400 bytes) — a row
"it returned Ok" cannot check.

### One prescription declined, one deviation accepted

- The reviewer asked for the 128-character GSM basic table copied into the test
  battery's prose beside §6.4's copy. Declined: two hand-maintained copies in one
  document is the drift this project avoids. The table lives once, as
  `const GSM_BASIC: [char; 128]` **in executable form**, length-asserted and
  iterated by the test; prose documents it.
- The plan's deterministic pre-CAS barrier for the dedupe race is not built. The
  check and the write are one `Mutex`-guarded `entry()` call, so the seam the
  barrier would park in does not exist — adding a hook would loosen the structure
  under test. Residue stated in `docs/m6a-acceptance.md`: a check-then-insert
  fails the 16-thread race only probabilistically.

### Deferred, named

Rate limits per principal/channel and the global provider budget: **M8**, with
the ledger that can account for them (this ADR is the recorded deferral an
earlier plan draft wrongly attributed to ADR-022). The in-memory replay window's
restart gap: accepted, because the boundary makes a re-admitted duplicate
harmless (derived create ids collide; a cancel against `Cancelled` is
`Undefined`) — which is also why STOP's suppression must NOT accept the same
posture in M6B: nothing downstream re-suppresses, so a safety exit that forgets
is not one. The durable suppression store lives in M6B's orchestrator on
`std::fs`. The gateway's `IN_FLIGHT` const is named debt until a domain-exported
name list exists.

## ADR-024 — The dispatcher: deterministic routing over one probabilistic seat

Decided 2026-09-02 with the project owner. Completes M6 (with ADR-023's M6A);
the milestone gate — the scripted SMS conversation — passes clean, faulted, and
as the demo binary, which runs the same script through the same runner.

### One seat for the model, everything else decided

`townhall-orchestrator` routes deterministically. Its one probabilistic seat is
the `Proposer` port: projected context in, typed request out, no route to
anything that acts. M6 seats a strict grammar (`ScriptedProposer` — `BOOK k=v…`,
`CONFIRM`, `cancel it`; near-misses are `Unclear`, never guesses); M11 seats a
model through the same trait. `CONFIRM` is named in code and script as the
stand-in M7's approval challenge replaces.

The ordering is the contract, held by hostile ports in the tests: channel
controls answer from ports before the proposer is consulted or any wire is built
(panicking proposer + counting wire, zero of each, REVOKE included); identity
resolution precedes every wire construction (panic-on-touch wire); the balance
port is consulted exactly once and only for BALANCE (sentinel).

### STOP gates the turn; suppression is durable; the boundary is exempt

`run_followups` skips a suppressed follow-up BEFORE any wire exists — §14.1's
"and scheduled agent turns", not just the outbound. Mutation-verified: an
implementation that ran the turn and suppressed only the message fails the
counting-factory witness. The server's own reconciliation is deliberately
untouched — the booking still settles at the council, because STOP silences the
messenger, not the boundary. Suppression lives in `FileSuppression` on
`std::fs` (the crate graph forbids `sqlx` here): the replay window may forget
across restarts because the boundary makes duplicates harmless, but nothing
downstream re-suppresses, so this store must not.

### Sessions hold ids and nothing else

`Session { recent: Vec<BookingId> }` — no version field to go stale. Every
proposal path acts on a version the server just reported: a fresh read, or the
committed turn's own response (which is why the clean schedule is 14 requests,
not the plan's 16 — recorded as a deviation, asserted exactly). "Cancel it"
resolves from `?cancellable=true`; ambiguity asks, naming the candidates, with
session recency only ordering the question. Mutation-verified: a dispatcher
acting on assumed state fails the reload witness.

### The gate's own catch

The journey runner deduped the second script's first message as a carrier retry
— two runs shared the replay window while each restarted its turn numbering.
Correct dedupe plus careless identity generation is indistinguishable from lost
mail; runner identities are process-unique now, and the incident is the best
argument yet for the derived-id discipline it briefly defeated.

### Costs accepted

A `GET ?cancellable=true` per freeform turn (the proposer's context) — one
round-trip per conversational beat at POC scale. The fault run asserts
invariants rather than an exact convergence-GET count, trading the plan's "18"
for freedom from the reconciler's cadence. The demo binary hardcodes the dev
bindings, exactly as the tests do, because it is a composition root for a demo
of those tests' world.

## ADR-025 — Authority: M7's decisions before M7's code

Decided 2026-09-02 with the project owner. Plans M7 (Approval +
VerifiedAuthority), amends ADR-021 and ADR-022, and settles what ADR-021
deferred by name: "`VerifiedAuthority`'s constructor ceremony belongs to M7,
with its issuer."

Three reviewers read the plan independently — codex gpt-5.6-sol with repo
access, deepseek-v4-flash and glm-5.3 with spec and code excerpts. All three
reached the same three answers the plan proposed and refuted all three of the
reasons it gave for them. What follows is the corrected version.

### One `principal` was doing three jobs

Today's envelope carries one `PrincipalId`, and ADR-022 spent it three times:
the booking's owner at create, the visibility predicate in SQL, and the
requester persisted in the cancellation plan. ADR-020 promised the booker and
the canceller need not be the same person; one field cannot keep that promise,
and §13's sketch — also one `principal` — would have carried the ambiguity into
the schema.

M7's envelope separates three things:

- **grantor** — on whose behalf; the booking's owner and the visibility scope;
- **subject** — the principal the action is attributed to (ADR-020's requester);
- **actor** — the authenticated workload that presented the grant.

Lucy books: grantor = subject = `lucy`. Marco cancels under delegation: grantor
= `lucy`, subject = `marco`, actor = Marco's agent. ADR-022's outstanding
facade-level delegated-cancellation test is the witness, and it lands in M7A —
early, because it is the test most likely to expose a wrong envelope before that
envelope becomes schema and wire compatibility debt.

### Widening does not avoid a second vocabulary; it relocates it

The plan's reason for widening rather than wrapping — "two types that both
describe what you are allowed to do is two places to forget a check" — is void.
§9's `delegations` table must persist the envelope for expiry and revocation to
be checkable at all, and ADR-021's surviving half of ADR-017 point 4 forbids the
domain type from being that representation: `VerifiedAuthority` implements
neither `Serialize` nor `DeserializeOwned`, asserted. A row type appears either
way.

Recorded consequences:

- the row representation is **owned by the issuer**, not by the store's decoder;
  the store persists opaque bytes plus only the columns revocation and expiry
  must index;
- the round trip is pinned by a test that issues, persists, reloads and compares
  the **issued** value — never a hand-built one;
- the no-serde assertion is now known to be **insufficient on its own**, and its
  comment says so: it forbids the domain type crossing a wire while the row
  mapping beside it is the real minting path.

Widen anyway. Of the two second vocabularies on offer, a narrow struct plus an
envelope was the worse, because both of its halves would have lived in code and
described scope.

### Nobody mints authority by struct literal, tests included

The plan's reason for a separate crate — "the orchestrator physically cannot
mint a grant" — is also void. All five fields are `pub` today and `DevAuthority`
builds one with a struct literal, so any crate can. A crate boundary is not a
capability boundary while the constructor is public; ADR-022's `load_visible`
removed a capability, and crate placement alone removes nothing.

`VerifiedAuthority` and `VerifiedApproval` move to `townhall-authority` with
private fields, below the domain in the graph, and the only constructor takes a
`VerifiedApproval` that the verifier alone can produce. Private fields in
`townhall-domain` would have blocked the issuer too — the type lives with the
thing that issues it.

Which leaves the 24 construction sites. **No `test-support` constructor is
added.** That is the backdoor this section exists to close, and a cargo feature
that leaks through unification would close it only on paper. Tests obtain
authority the way production does: `townhall-testkit` gains an in-memory issuer
that drives the real challenge → approval → grant path, and the sites migrate to
it. A test whose premise is a forged grant asserts against a fiction — the
project's hard rule, applied to the authority type itself.

### Two headers, because authentication is not delegation

§10.1 always specified both: `Authorization` authenticates the agent or service,
`X-BLD-Delegation` carries or references the verified grant. M5 conflated them —
the bearer *is* the authority — and reserved the delegation header with a 400.

M7 separates them, and the order of work matters: **un-reserve the header before
writing any tamper test.** `authorize()` refuses `x-bld-delegation` as its first
statement, before the bearer is even read, so a test that sends a tampered
envelope and expects a denial passes today against code that performs no check.
Recorded as a trap because the acceptance gate names "tampered" and the gate is
presently satisfiable by nothing.

The separation also removes a live hazard: with one header, revoking a grant
would also remove the caller's ability to read, to request another approval, or
to send REVOKE.

### The authority plane is the server's, and the proposer cannot reach it

Two processes need one authoritative store: the SMS side asks for a challenge,
the server resolves the resulting grant. The simulator currently mints its own
credentials locally, which is a composition-root convenience and not a topology.
Rather than a second service or a deliberately shared database file, the trusted
authority endpoints live **in the server**, on a router `Gateway` does not know:
the gateway keeps its socket and its ignorance, and the orchestrator reaches
issuance through a narrow begin/submit/revoke port returning opaque references.

The orchestrator receives no issuer capability and no dependency on
`townhall-authority`. ADR-023's resolved-dependency tripwire gains that crate by
name, beside the crates it already forbids — the guarantee is an assertion or it
is a hope.

### Approval comes before the durable mutation

§23.1 is normative and M6's dispatcher has it backwards: `BOOK` creates the
intent, searches venues, selects and verifies, and *then* asks for `CONFIRM`.
Replacing the word with `YES 7312` in place would leave approval standing after
four committed versions. M7's order is the spec's — preview, challenge,
approval, and only then create, select, verify, book.

This forces something the plan had not noticed: the `BookingId` must be **minted
at challenge time**. ADR-024 derives it from `message.identity`, and the `YES`
reply is a different message with a different identity, so resuming from it
would create a second booking. The id is derived from the original request,
carried inside the approved scope, persisted with the challenge and reused after
approval. The same move gives "approve the cancellation of that booking"
somewhere to point before a row exists.

### The canonical scope is data; the preview is rendered from it

A hash proves equality and reconstructs nothing. If only the hash is durable,
resuming after a restart requires session memory or re-parsing an old SMS, and
§2's "durable state is not conversational memory" forbids both. The challenge
stores the canonical scope **data** and its versioned hash; the issuer loads
that object rather than accepting replacement scope from its caller; the hash's
encoding is order-fixed, because an unordered behaviour set would hash
differently between runs.

The preview renderer lands in **M7A, not M7C**. The correspondence between what
Lucy was shown and what was hashed is the property that makes approval mean
anything, and it is invisible to every "tampered" test — both strings are
system-generated, so drift between them is silent by construction.

### One challenge, one grant — and a grant is used many times

Two different rules, and conflating them would break the workflow while passing
a test. A challenge is one-time, expiring and attempt-bounded (§9.1); a
delegation is stable until expiry or revocation. Therefore:

- replaying an approval must not mint a second grant;
- **reusing a valid grant across create → select → verify → book → cancel is
  expected**;
- "one booking" is a property of the exact-resource scope, never of HTTP call
  count.

A test that refuses the second presentation of a valid grant would pass while
implementing the wrong semantics. Recorded so that it cannot be written.

### The replay witness needs two messages

`ReplayWindow` is built and loom-tested in `townhall-channel` and is **not wired
into the dispatcher**: M6's redelivery safety is entirely ADR-024's derived id
landing on `AlreadyExists`. An approval reply has no such structural defence, so
a carrier-redelivered `YES` reaches the verifier twice and the second arrival is
indistinguishable from an attack.

Decided: the dispatcher absorbs redelivery by message identity **before** the
verifier, so a legitimate redelivery is idempotent rather than denied — and the
verifier's replay check is therefore witnessed only by **two distinct messages
carrying one code**. A replay test built from one repeated message asserts the
dedupe, not the check, and would keep passing if the check were deleted.

### Revocation blocks the next mutation, not the last one

ADR-014 persists an effect intent before the effect; ADR-019's recovery finishes
it without a requesting principal, and the store's reconciliation path
deliberately has none. Requiring the grant to stay live through reconciliation
would strand that protocol. Explicitly:

- expiry or revocation refuses to **start** an authorized mutation;
- it neither erases nor invalidates an already-committed effect intent;
- recovery proceeds from durable booking state, as it already does;
- any later cancellation is separately authorized.

Tested with revocation racing both before and after Phase A.

### Assurance is enforced, or it is decoration

A stored string reading `Sms` that nothing compares against satisfies a schema
test and enforces nothing. The channel binding establishes an assurance level;
the issuer **caps** the grant's at the binding's; the service carries a minimum
and refuses below it. §13.1's point — that SMS approval suits the town-hall risk
profile and not a higher one — is that cap and nothing else.

### Delegated visibility intersects; it does not substitute

ADR-022's concealment comes from the row predicate `WHERE id = ? AND
owner_principal = ?`. `lookup_cancellable` returns every row for an owner and
lets the domain decide what "cancellable" means. Handing Marco a grant for one
of Lucy's bookings by substituting Lucy as the owner would expose all of them.

Delegated reads intersect in SQL — authorized owner ∩ granted resources ∩
permitted behaviours — and the base-row predicate stays positive. Fetching
Lucy's rows and filtering in application code is the shape ADR-022 rejected.

### `NO 7312` is not REVOKE

Rejecting a pending challenge and revoking an issued grant are different acts,
and today's grammar models neither. §13.2's own preview offers both words, so
`NO` ships with the challenge: it makes that challenge terminal, and a later
`YES` must not revive it. REVOKE keeps ADR-024's position — answered from ports
before the proposer is consulted or any wire is built — but gains a
verified-channel path, must not require the grant it is revoking, and is
idempotent.

### Amendment to ADR-021: the curl lane survives, and is named

ADR-021 recorded that M7 "replaces the resolver in the composition root", which
the trait's own comment reads as "no fallback survives beside it". Keeping
`--dev-authority` for M5's curl-only gate is exactly such a fallback, so it is
amended here rather than retained quietly:

- the dev resolver stays behind the cargo feature **and** the startup flag;
  absent either, the server refuses to start;
- without the flag, a feature-enabled build resolves through the **real**
  resolver — never a silent dev fallback;
- the real resolver **explicitly rejects every `dev-*` token**;
- dev grants are pinned to the lowest assurance and a short expiry, because
  widening would otherwise make a dev token a forged full envelope that reads as
  maximally assured;
- two separate tests, because these are two properties: the flag is unavailable
  in a no-feature build, and the running real resolver refuses a dev token;
- a **no-feature CI lane**, because `services/townhall-server/tests/http.rs`
  carries `#![cfg(feature = "dev-authority")]` on line 1 and a test hidden inside
  that file cannot prove the escape hatch closed.

### The slice boundary, moved

The plan's two slices put the resolver swap in the first, before anything could
issue a grant — a window in which the SMS lane (dev credentials → dev resolver)
and the curl lane both break, and in which nothing exercises the new resolver end
to end. §2's "build dependency-first" requires each slice to be independently
runnable and testable, so there are three:

- **M7A — the authority component.** IDs and types with private construction;
  `channel_bindings`, `approval_challenges`, `delegations` (migration 0006); the
  canonical scope codec and the preview renderer; atomic attempt counting,
  one-challenge-one-grant issuance, expiry, revocation; the testkit issuer and
  the 24-site migration; ADR-022's delegated-cancellation test; the facade's
  exact-resource delegated admission.
- **M7B — the HTTP contract.** `Authorization` and `X-BLD-Delegation` separated
  and the header un-reserved; the real resolver and the trusted authority
  endpoints; the composition root; this ADR's amendment tests and the no-feature
  lane.
- **M7C — the human half.** Approve-first ordering; `YES` and `NO`; verified
  REVOKE; the `CredentialSource` swap **in the same slice as the journey test**,
  because a journey still drawing `dev-lucy` would pass with the code purely
  decorative; Lucy's end-to-end £45 booking and the hostile battery.

### Costs accepted

Twenty-four construction sites migrate to a testkit issuer that runs real
issuance — slower tests, and a testkit that can now refuse. `townhall-domain`
gains a dependency on `townhall-authority`, inverting the intuition that the
domain sits lowest. M6's journey is rewritten rather than extended, and its
script changes visibly for anyone who learned the old order. Three slices where
the roadmap gives M7 one week alongside M8.

### What the reviewers found, credited

codex gpt-5.6-sol: the ordering error (approval before create), the id minted at
challenge time, the absent authority plane, grant reuse against challenge
replay. glm-5.3: the persistence tension that voids the widening argument, the
tamper test satisfiable by the reserved-header 400, the replay witness, and
`DevAuthority` as a forger of the widened envelope. deepseek-v4-flash: this
section's existence — that keeping the curl lane contradicts ADR-021 as written
and needed amending rather than assuming. All three refuted the reasons; none
changed the answers.

### Amendment, made during M7A: the resolver's resource-awareness is M7A's

Amended 2026-09-02 with the project owner, after M7A was built.

The slice boundary above put the resolver in M7B. M7A moved it, and the plan of
record should say so rather than leaving the code and the plan disagreeing.

**What forced it.** `AuthorityResolver::resolve(bearer)` answered a question
that no longer has an answer. A capability could be resolved from a bearer alone
— `may_book` held for any booking its holder could name — but a grant names its
resource, so "what may this bearer do" is only decidable against a booking. The
M5 curl suite uses twenty-nine booking ids, so no fixed-resource dev grant could
serve it, and a resolver that could not see the resource would have had to
return something permissive. There was no version of M7A that both sealed the
envelope and left this seam alone.

**What moves into M7A**, therefore:

```rust
fn resolve(&self, bearer: &str, booking: &BookingId) -> Option<VerifiedAuthority>;
fn resolve_reader(&self, bearer: &str) -> Option<PrincipalId>;
```

This is the smaller half of the plan's finding #1 — "authenticate the actor;
authorize the presented delegation against that actor, service, resource and
behaviour". The authorization half is now resource-scoped. **M7B keeps the rest**:
separating `Authorization` from `X-BLD-Delegation`, un-reserving that header,
and binding the actor to an authenticated workload rather than deriving it from
the subject.

**A reader is an identity, not a grant.** The first attempt had `resolve_reader`
return a `VerifiedAuthority` "naming no resources", which a scope makes
impossible — it named a synthetic booking id, and authority over an imaginary
booking is still authority. The assertion guarding it was written over an
`Option` that was `None` by construction and was vacuously true. Recorded
because the correction is a rule and not a patch: **listing your own bookings
needs to know who you are; touching one needs a grant.** A caller on the reading
path receives something that cannot authorize anything, because it is not the
kind of thing authorization is made of.

**What the dev lane consequently cannot witness.** `DevAuthority` mints a grant
naming whichever booking the request named, so its resource check can never
refuse anything. The curl suite therefore witnesses the behaviour guard
(`dev-priya-nobook` is refused `Book`) and the ownership guard (ADR-022's scoped
rows), and never the resource guard — whose witness lives in
`townhall-authority`, where a grant issued for one booking is asked about
another. Named here so the gap is on the books rather than assumed absent.

### What M7A's mutations found that no reviewer did

Three reviewers read the plan and none of them caught this: the grantor/subject
separation — the reason this ADR exists — had **no witness at all**. Swapping
`access.grantor()` for `access.subject()` on the `owner` column broke nothing
across 439 tests, because every fixture in the workspace used one principal for
both roles, and the single delegated case created its booking under a
non-delegated grant. The distinction was implemented, migrated across
twenty-four sites, argued for at length above, and unobserved.

Recorded as the ADR's own lesson: a design argument is not a witness, and a
reviewer reading a plan cannot tell you which of its claims your tests would
notice losing. Only mutating the implementation can.
