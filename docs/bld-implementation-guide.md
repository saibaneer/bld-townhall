# Boundary-Led Development (BLD): Why It Exists and How to Implement It

Status: **Normative implementation guide for coding agents**

This document is intentionally explicit. It exists so that an implementation agent can work on a BLD codebase without relying on intuition, improvisation, or model obedience.

If this guide conflicts with a domain-specific technical specification, stop and surface the contradiction. Do not silently invent a new architecture.

---

## 1. Why BLD Exists

LLMs and other probabilistic systems are useful because they can interpret ambiguity, reason over incomplete instructions, rank options, explain results, and choose among many possible next steps.

Those same properties make them unsuitable as the final authority for consequential state changes.

A model can:

- misunderstand a request;
- hallucinate facts;
- be prompt-injected;
- produce different outputs from the same input;
- become compromised;
- be replaced by a weaker model;
- generate syntactically valid but false evidence;
- ask for a capability it should never possess.

BLD therefore starts from one rule:

> **A probabilistic component proposes; a deterministic boundary disposes.**

The model may suggest what should happen. It does not get to decide what is allowed to happen.

The BLD boundary decides:

- whether a behaviour exists in the current state;
- whether the caller has sufficient authority;
- whether policy and resource limits permit the action;
- what authoritative values must be used;
- which capability may be invoked;
- what evidence is required;
- whether the resulting state may be committed.

The goal is not to make the model perfectly obedient.

The goal is to make unsafe model behaviour unable to escape the permitted system.

---

## 2. The Replacement Test

Use this test constantly:

> **If the current model were replaced with a confused script or a hostile proposer, would the system's invariants still hold?**

If the answer is "no", the safety property is probably still living in the prompt instead of the boundary.

Examples of invalid designs:

```text
"The model knows it must never spend more than £50."
```

```text
"The agent was told not to call the booking API twice."
```

```text
"The LLM only says payment succeeded after the user pays."
```

These are behavioural expectations, not boundaries.

The correct versions are deterministic:

```text
Canonical booking plan cannot exceed verified £50 authority.
```

```text
One stable EffectIntentId maps to at most one provider-side booking.
```

```text
PaymentConfirmed requires Verified<PaymentEvidence> from the payment provider.
```

---

## 3. The Core Mental Model

A BLD system separates five things that ordinary agent applications often blur together:

```text
Human / environment
      ↓
Untrusted input
      ↓
Probabilistic proposer
      ↓
Typed proposal
      ↓
════════════ BLD BOUNDARY ════════════
current authoritative state
verified authority
trusted context / policy / budgets
canonical plan derivation
scoped capability access
verified evidence
commit discipline
═══════════════════════════════════════
      ↓
External consequence
```

The proposer may influence **which permitted behaviour is requested**.

The proposer must not control **the authority, consequential parameters, evidence, or commit**.

---

## 4. The Three Outcomes Every BLD Boundary Must Preserve

Use three distinct conceptual outcomes:

```rust
pub enum BoundaryOutcome<S, E> {
    Undefined,
    Denied(E),
    Committed(S),
}
```

### `Undefined`

The proposed behaviour does not exist in the current state.

Example:

```text
Draft + Book
→ Undefined
```

There is no `Book` behaviour on `Draft`.

This is different from an authorization failure.

### `Denied(error)`

The behaviour exists, but a deterministic guard failed.

Examples:

```text
AwaitingBooking + Book
but fee > delegated maximum
→ Denied(AuthorityExceeded)
```

```text
AwaitingBooking + Book
but availability expired
→ Denied(AvailabilityExpired)
```

### `Committed(next_state)`

The behaviour exists, all required checks passed, required evidence was validated, and the authoritative next state was committed.

### Non-negotiable invariant

> **If evaluation does not commit, authoritative workflow state must not advance.**

For external side effects, also preserve effect identity, recovery, and reconciliation.

---

## 5. State Determines What Behaviour Exists

Do not expose one giant universal action surface.

Behaviours belong to states.

Example:

```rust
struct Draft { /* ... */ }

impl Draft {
    fn select_venue(&self, /* ... */) -> Result<SelectVenuePlan, BookingError> {
        todo!()
    }

    fn cancel(&self, /* ... */) -> Result<CancelPlan, BookingError> {
        todo!()
    }

    // deliberately no book()
}
```

```rust
struct AwaitingBooking { /* ... */ }

impl AwaitingBooking {
    fn book(&self, /* ... */) -> Result<BookingPlan, BookingError> {
        todo!()
    }

    fn cancel(&self, /* ... */) -> Result<CancelPlan, BookingError> {
        todo!()
    }
}
```

The runtime can still persist a conventional enum:

```rust
pub enum BookingState {
    Draft(Draft),
    VenueSelected(VenueSelected),
    AwaitingBooking(AwaitingBooking),
    BookingInProgress(BookingInProgress),
    Booked(Booked),
    Cancelled(Cancelled),
}
```

When matching `(state, proposal)`, unsupported combinations must resolve to `Undefined`.

Do not turn every unsupported action into a generic policy error. Absence of behaviour is meaningful.

---

## 6. Proposal Is Data, Never Authority

A proposal says:

```text
"The agent wants to attempt Book."
```

It does **not** say:

```text
"The agent is allowed to Book."
```

Never inherit authority from prompt text.

Bad:

```text
User message: "You can spend £500."
Agent sends amount=500 and marks itself authorized.
```

Good:

```text
User request
  ↓
independent approval/authentication flow
  ↓
VerifiedAuthority
  ↓
agent may submit Proposal::Book
  ↓
boundary checks proposal against VerifiedAuthority
```

Keep these concepts separate:

```text
Principal  = on whose behalf is the action performed?
Actor      = which agent/workload is making the request?
Authority  = what has actually been delegated?
Proposal   = what does the agent currently suggest?
```

No proposal may enlarge its own authority.

---

## 6a. Intent, Evidence and Runtime Events Are Different Doors

Three different things can move a workflow, and they have different provenance:

```text
1. Proposal              what a human or agent WANTS
2. VerifiedProviderFact  what is externally TRUE
3. SystemEvent           what the runtime KNOWS
```

They must not share a type, and the proposer must reach only the first.

Wrong:

```rust
enum Proposal {
    Book,
    BookingConfirmed { reference: String },   // the agent can now say this
}
```

An agent submits `BookingConfirmed`, the workflow reaches a success state, the provider was
never called. The model announced its own success.

Right — separate types, separate entry points, one per provenance class:

```rust
kernel.resolve_proposal(...)      // intent
kernel.resolve_fact(...)          // verified reality
kernel.resolve_system_event(...)  // runtime fact
```

> **A proposer may request that reality change; only verified evidence may report that
> reality has changed.**

> **Consequential success states must not be reachable from proposer vocabulary.**

Stronger than validating a proposal's contents: a guard can be forgotten at one call site;
a type that does not exist cannot be constructed anywhere.

### Evidence says what is true. State determines what that truth means.

Do not let the verifier emit state-specific transitions. It would have to know the state,
which is the wrong coupling. The verifier establishes an external *fact*; the domain
interprets it against current authoritative state:

```text
BookingExists + BookingInProgress      -> booking_confirmed -> Booked
BookingExists + CancellationRequested  -> booking_found     -> CancellingBooking
```

Same fact. Different legal meaning, because the state changed.

This is what makes a lost race safe. If a cancellation wins the compare-and-set while
verified evidence was in flight, that evidence is **re-evaluated against the new state**,
never discarded — the provider really did act, and losing a CAS does not make it untrue.

> **External evidence represents facts, not transitions.**

> **A verified fact that loses a concurrency race must be re-evaluated against the new
> authoritative state, never discarded.**

### `Verified<T>` is provenance, not blanket trust

It means the external claim passed its verifier. It does *not* mean the claim applies here.
The domain still binds it: does the effect identity match the active effect, do the
resource, parameters and principal match the persisted canonical plan, is the current state
one where this fact applies at all.

```text
AgentClaim<T>   != truth
RawProvider<T>  != truth
Verified<T>     == admissible evidence, still to be bound
```

### Recovery loops need a convergence outcome

A reconciler re-applies the same fact by design, so a repeat is normal:

```text
BookingExists + BookingInProgress  -> Ready(Booked)
BookingExists + Booked             -> Converged
BookingExists + Draft              -> Undefined
BookingExists + BookingInProgress, wrong effect id -> Denied(EffectMismatch)
```

> **Repeated verified facts may converge to an already-satisfied state and must not be
> treated as failure solely because the transition already occurred.**

`Converged` is success because local state already reflects the verified fact — not success
by ignoring something. Do **not** add it to the proposal door: for intent, a silent no-op
hides mistakes, and `Book` when already `Booked` should be `Undefined` or `Denied`.

### Recovery is not a proposal

If a user or model has to ask for recovery, recovery does not happen when the model is
offline, hostile or absent — precisely when it is needed. Uncertain outcomes enqueue a job;
the reconciler asks the provider; the verified answer enters through the fact door.

> **User intent, verified external facts, and deterministic runtime events are distinct
> provenance classes and should not share an untyped transition vocabulary.**

---

## 7. Provenance Matters More Than Shape

A valid-looking object is not necessarily true.

Use this vocabulary mentally or explicitly:

```rust
AgentClaim<T>      // model/user supplied and untrusted
Authoritative<T>   // loaded from a trusted system of record
Verified<T>        // checked against an external authority
```

Examples:

```text
AgentClaim<Money>(£45)
```

does not become the booking fee just because the JSON is valid.

The boundary should reload:

```text
Authoritative<Money>(£45)
```

from the council catalogue.

Similarly:

```text
AgentClaim<Receipt>
```

is not payment evidence.

A field-perfect forged receipt must still fail if its provenance is wrong.

Rule:

> **A type establishes shape. A boundary establishes validity and provenance.**

---

## 8. Derive Canonical Plans Inside the Boundary

The model should propose **behaviour**, not raw consequential provider parameters.

Bad:

```json
{
  "action": "book",
  "venue": "TH-A",
  "fee": 4500,
  "principal": "lucy",
  "provider_endpoint": "/bookings",
  "idempotency_key": "whatever-the-model-made-up"
}
```

Good:

```rust
Proposal::Book
```

The boundary reloads authoritative state and derives:

```rust
BookingPlan {
    effect_intent_id,
    booking_id,
    principal,
    venue_id,
    slot_id,
    attendees,
    fee,
}
```

Consequential fields must come from authoritative state, verified authority, or deterministic derivation.

The closer an action gets to consequence, the smaller the proposal should usually become.

---

## 9. Capabilities Must Be Narrow and Boundary-Reachable

Do not give the LLM direct access to raw consequential tools.

Bad:

```text
Agent tools:
- book_room
- charge_card
- cancel_any_booking
- update_database
```

Good:

```text
Agent tools:
- inspect_resource
- inspect_available_behaviours
- submit_typed_proposal
```

Behind the BLD boundary:

```text
BookingPlan
    ↓
BookingCapability.execute(plan)
```

A capability should accept the canonical plan, not raw model instructions.

The agent should never receive:

- database write handles;
- signing keys;
- payment credentials;
- provider admin APIs;
- arbitrary shell access to the authority plane;
- policy mutation APIs.

---

## 10. Only the Boundary-Controlled Commit Path May Change Authoritative State

Transport handlers must be thin.

Bad Axum handler:

```rust
booking.state = BookingState::Booked(...);
repository.save(booking).await?;
```

Good flow:

```text
HTTP/SMS handler
    ↓
parse request
    ↓
resolve identity / authority
    ↓
construct BoundaryRequest
    ↓
BookingService
    ↓
load authoritative aggregate
    ↓
BLD kernel/domain
    ↓
repository compare-and-set commit
```

The handler does not own mutation authority.

The model does not own mutation authority.

The repository should not invent domain transitions.

The domain determines valid next state; the persistence layer atomically commits it.

---

## 11. Version Every Authoritative Resource

Every committed resource change increments a durable version.

Example:

```text
AwaitingBooking v3
```

Two proposals are made against version `3`:

```text
Book(v3)
Cancel(v3)
```

Only one may win.

Database CAS:

```sql
UPDATE bookings
SET state_payload = ?, version = version + 1
WHERE id = ? AND version = ?;
```

If one transition commits `3 -> 4`, the other must fail as stale.

Important:

> **Cancellation does not prevent races. Versioning + atomic compare-and-set does.**

Over HTTP, expose this with standard `ETag` / `If-Match` semantics.

Never trust a stale proposal forever.

---

## 12. External Effects Need Stable Intent Identity

Database state and an external provider cannot usually be committed atomically.

Therefore never implement consequential effects as:

```text
call provider
→ hope response arrives
→ save state
```

Instead:

```text
A. Persist intended consequence
B. Execute under stable EffectIntentId
C. Verify/reconcile external truth
```

### Phase A — persist before effect

```text
load resource @ version N
→ resolve behaviour
→ derive canonical plan
→ derive stable EffectIntentId
→ persist intent
→ commit in-progress state
→ version N -> N+1
```

### Phase B — execute outside DB transaction

```text
Capability.execute(canonical_plan, EffectIntentId)
```

The provider should make retries with the same effect identity idempotent.

### Phase C — validate/reconcile

```text
provider evidence
→ validate against persisted plan
→ commit final state
```

Timeout means **unknown**, not success and not failure.

Never generate a new EffectIntentId just because a response was lost.

---

## 13. Cancellation Is a New Proposal Against Current State

Cancellation is not "kill the old thread".

It is a new authenticated proposal evaluated against the current authoritative resource.

Examples:

```text
Draft + Cancel
→ Cancelled
```

```text
Booked + Cancel
→ CancellingBooking
→ verified cancellation evidence
→ Cancelled
```

```text
BookingInProgress + Cancel
→ CancellationRequested
```

When an external effect may already have occurred, cancellation becomes compensation/reconciliation.

Do not erase history.

Do not pretend an external effect did not happen.

---

## 14. Human Handoff Is a First-Class State Pattern

BLD does not require agents to autonomously complete every consequence.

For high-value or high-risk actions, the boundary can deliberately stop at a human-owned state.

Example:

```text
OfferSelected
→ CheckoutPrepared
→ AwaitingHumanPayment
→ PaymentConfirmed
→ BookingInProgress
→ Booked
```

The agent may prepare checkout.

The human performs payment directly.

The workflow advances only on verified provider evidence.

Never accept these as payment evidence:

- model says "paid";
- user SMS says "I paid";
- browser success redirect;
- checkout URL creation.

Only verified payment-provider evidence may establish `PaymentConfirmed`.

---

## 15. Communication Channels Are Adapters

SMS, web, WhatsApp, voice, mobile apps, and other channels are not BLD itself.

Use a channel abstraction:

```rust
trait HumanChannel {
    // normalize inbound communication
    // send bounded outbound communication
}
```

Channel responsibilities:

- message transport;
- normalization;
- deduplication;
- delivery status;
- bounded parsing.

Channel non-responsibilities:

- booking state;
- domain policy;
- authority creation from raw text;
- consequential mutation.

A phone number is a routing/authentication signal, not automatically high-assurance identity.

Conversation memory may help resolve "cancel it", but authoritative resource state must be reloaded before consequence.

---

## 16. Resource Budgets Are Separate From Business Authority

BLD should also bound liveness/resource consumption.

Examples:

- number of LLM turns;
- tool calls;
- retries;
- queued work;
- elapsed time;
- message size;
- audit growth.

A resource quota answers:

```text
"May this workflow consume another unit of computation?"
```

Authority answers:

```text
"May this actor perform this business action?"
```

Do not mix them.

A user with plenty of usage credits may still lack permission to book.

A user with zero normal quota may still need zero-cost safety exits such as cancellation, STOP, HELP, or REVOKE.

---

## 17. The Kernel Should Be Small

The reusable kernel should enforce sequencing, not contain domain knowledge.

Conceptually:

```rust
pub enum Resolution<P, E> {
    Undefined,
    Denied(E),
    Ready(P),
}
```

```text
proposal
→ resolve
→ canonical plan
→ execute capability
→ evidence
→ validate
→ audit
→ commit
```

Town-hall rules belong in `TownHallDomain`.

Payment rules belong in the payment domain/service.

SMS belongs in the channel layer.

Rig/model code belongs in the proposer layer.

Do not make the kernel "smart" by importing every application concern into it.

A smaller trusted computing base is easier to review, fuzz, test, and eventually formally analyse.

---

## 18. Dependency Direction

Prefer dependency direction like this:

```text
bld-types
    ↑
bld-kernel
    ↑
domain
    ↑
repository / capability adapters
    ↑
service layer
    ↑
HTTP / SMS / agent adapters
```

Do not reverse the dependency because a later integration is convenient.

Examples of architecture corruption:

```text
townhall-domain imports Axum
```

```text
bld-kernel imports Rig
```

```text
agent-runtime imports repository internals to mutate bookings
```

```text
SMS handler calls council booking API directly
```

All are wrong.

---

## 19. Implement BLD Dependency-First

Do not build the flashy layer first.

Recommended order for a new domain:

### Step 1 — Define the prohibited consequences

Write down what must never happen.

Examples:

```text
Never book above verified maximum fee.
Never book an inaccessible venue when accessibility is required.
Never create two provider bookings for one intended booking.
```

If you cannot state the invariants, do not write the agent yet.

### Step 2 — Define the states

Use explicit finite states.

Example:

```text
Draft
VenueSelected
AwaitingBooking
BookingInProgress
Booked
Cancelled
NeedsHuman
```

### Step 3 — Define typed proposals

Example:

```rust
enum BookingProposal {
    SelectVenue { venue_id: VenueId, slot_id: SlotId },
    VerifySlot,
    Book,
    Cancel,
    Reconcile,
}
```

Avoid an unbounded `Action(String)` escape hatch.

### Step 4 — Draw the complete state × proposal topology

Every pair must be deliberately classified:

```text
valid behaviour
or
Undefined
```

Do not leave "whatever seems reasonable" behaviour.

### Step 5 — Implement state-scoped behaviour methods

A state should expose only behaviours that exist there.

### Step 6 — Separate authority

Define the principal, actor, grants, constraints, expiry, and verification source.

Never infer authority from prompt text.

### Step 7 — Derive canonical plans

Reload authoritative facts and derive consequential parameters inside the boundary.

### Step 8 — Define narrow capability contracts

Capabilities execute canonical plans only.

### Step 9 — Define evidence semantics

Specify what proves the external effect happened and where that evidence comes from.

### Step 10 — Add durable aggregate + versioning

Persist state and use database compare-and-set.

### Step 11 — Add stable external effect identity

Persist intent before provider calls. Add idempotency and reconciliation.

### Step 12 — Add transport adapters

Only now add HTTP, SMS, queue consumers, etc.

### Step 13 — Add the agent

The agent should operate against the already-working deterministic service.

### Step 14 — Add hostile proposer tests

Replace the helpful model with a deterministic attacker.

If the attacker can violate an invariant, fix the boundary rather than the prompt.

---

## 20. Required Test Classes

Happy-path tests are insufficient.

Every BLD implementation should include as many of these as apply.

### Topology tests

Enumerate every `(state, proposal)` pair.

Verify each pair is explicitly legal or `Undefined`.

### Authority tests

Test:

- expired grants;
- wrong principal;
- wrong actor;
- wrong service/audience;
- amount over limit;
- replayed approval;
- revoked delegation.

### Provenance/evidence tests

Use field-perfect forgeries.

The fake should look correct but lack authoritative provenance.

### Concurrency tests

Test two writes against the same version.

Exactly one may commit.

### Idempotency tests

Retry the same intended effect many times.

Exactly one external consequence may exist.

### Crash/recovery tests

Inject failure:

- before provider call;
- after provider commit but before response;
- after response but before local final commit;
- during reconciliation.

### Hostile proposer tests

A deterministic hostile proposer should try:

- illegal transitions;
- stale versions;
- over-budget actions;
- forged evidence;
- repeated retries;
- invalid identifiers;
- direct capability requests;
- attempts to enlarge authority.

The boundary must remain correct regardless of proposer behaviour.

---

## 21. Common Agent Mistakes — Do Not Do These

### Mistake 1 — Put safety in the system prompt

Wrong:

```text
"Never book above £50."
```

Correct:

```text
boundary compares authoritative fee against VerifiedAuthority.max_fee
```

### Mistake 2 — Let the model choose authoritative amounts

Wrong:

```text
proposal contains final charge amount
```

Correct:

```text
boundary reloads authoritative amount and derives plan
```

### Mistake 3 — Treat authentication and authority as the same thing

Knowing who called does not prove what they may do.

### Mistake 4 — Treat a correctly shaped receipt as evidence

Shape is not provenance.

### Mistake 5 — Call provider before persisting effect intent

This makes crash recovery ambiguous and duplicate effects likely.

### Mistake 6 — Generate idempotency keys from request IDs

A retry gets a new request ID. The intended business effect does not.

### Mistake 7 — Hold DB transactions open across network calls

This does not make the DB and provider atomic.

### Mistake 8 — Use a mutex instead of durable versioning

A mutex only protects one process. BLD state must survive multiple workers and restarts.

### Mistake 9 — Let transport handlers mutate domain state

Handlers are adapters, not authority.

### Mistake 10 — Let conversation memory become business state

Always reload authoritative resource state before consequence.

### Mistake 11 — Treat timeout as failure

Timeout means unknown until verified.

### Mistake 12 — Retry with a new effect identity

That can create duplicate real-world effects.

### Mistake 13 — Collapse `Undefined` and `Denied`

The distinction tells us whether behaviour exists at all.

### Mistake 14 — Add a universal escape-hatch tool

An `execute_anything(String)` tool destroys the boundary.

### Mistake 15 — Change architecture silently to make code easier

Write an ADR or stop and ask.

---

## 22. How to Review a BLD Pull Request

Before approving a change, ask:

1. Does this introduce a new state?
2. Does this introduce a new proposal?
3. Which exact state/proposal pairs become legal?
4. Where is authority checked?
5. Are consequential values model-supplied or authoritative?
6. Is the capability reachable only through the boundary?
7. What external evidence proves success?
8. What happens on timeout?
9. What is the stable effect identity?
10. Can a retry duplicate the consequence?
11. Can two concurrent versions both commit?
12. What happens if the proposer is hostile?
13. Does this make a transport/model/framework part of the trusted core unnecessarily?
14. Are new failure paths represented explicitly rather than mapped to success?
15. Is there a deterministic regression test for the claimed invariant?

If the author cannot answer these, the implementation is not ready.

---

## 23. Minimal BLD Definition of Done

A BLD feature is not complete merely because the happy path works.

It is complete when:

- states are explicit;
- proposals are typed and bounded;
- topology is explicit;
- unsupported transitions are `Undefined`;
- authority is independently verified;
- canonical plans use authoritative facts;
- capabilities are narrow;
- evidence has provenance;
- state commits are version-checked;
- external effects have stable identities;
- retries are idempotent;
- ambiguous outcomes reconcile;
- cancellation/compensation preserves history;
- hostile proposer tests pass;
- the model can be replaced without widening authority.

---

## 24. Working Rule for Coding Agents

When uncertain, choose the implementation that creates **less implicit authority**.

Prefer:

```text
explicit state
explicit proposal
explicit denial
explicit unknown
explicit human handoff
explicit reconciliation
```

over:

```text
guess
infer
retry blindly
trust the model
trust conversation history
silently broaden capability
```

BLD should make dangerous behaviour boring:

```text
hostile proposal
    ↓
Undefined / Denied
    ↓
no unauthorized consequence
```

That is the point.

---

## 25. Town-Hall Reference Mapping

For the current reference implementation:

```text
Human channel      → SMS / simulator
Proposer           → Rig agent / local open-source model / HostileProposer
Boundary service   → town-hall BLD service
Kernel             → bld-kernel
Domain             → townhall-domain
Durable state      → townhall-store
External authority → mock council service
Human payment      → Stripe sandbox handoff
```

Current milestone order:

```text
M0 workspace
→ M1 pure kernel
→ M2 in-memory town-hall domain
→ M3 durable aggregate + optimistic concurrency
→ M4 external effect protocol + reconciliation
→ M5 Axum BLD service
→ later channel, authority, metering, payment and agent layers
```

Do not skip forward because a later layer is more visually impressive.

---

## 26. Final Instruction to Any Implementation Agent

You are not being asked to make an AI agent powerful.

You are being asked to build a system in which an AI agent can be useful **without becoming the authority**.

When you add code, preserve this separation:

```text
INTELLIGENCE
interprets / proposes / explains

        ≠

AUTHORITY
permits / derives / verifies / commits
```

If your change makes the model more trusted, gives it broader direct tools, lets it choose authoritative parameters, or lets it declare its own success, you are almost certainly moving away from BLD.

If your change makes states clearer, authority narrower, evidence stronger, effects recoverable, and failure explicit, you are probably moving in the right direction.
