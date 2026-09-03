# M7B acceptance — two headers, and a grant nobody can assume

M7B is ADR-025's second slice: the HTTP contract, the real resolver, the
approval endpoints, and the composition root. M7A built a component that could
issue grants; M7B is where a grant becomes the only way to change anything.

**M7's gate is still not claimed** — "valid SMS challenge permits £45 scope"
needs the channel, which is M7C.

What M7B claims, stated carefully because a first draft of this document
overclaimed it: **a change requires a challenge, answered with a code, against a
binding that exists in the database at the revision claimed.** Exercised end to
end over HTTP in a build with no dev feature compiled in.

What it does NOT claim is that a PERSON answered. The code reaches the person by
passing through the workload that asked for it, so a workload holding the
credential can relay it to itself. Closing that needs evidence from the channel
adapter, and the channel arrives with M7C. Review found the first draft of this
slice claiming otherwise, in a test whose name said so — see below.

## The battery

| Area | Tests | Notes |
|---|---|---|
| Envelope authentication | `a_well_formed_envelope_without_the_key_is_not_a_grant`, `a_tag_cannot_be_transplanted_between_grants` | **Mutation-verified**: skipping the check fails the first; signing `out[..8]` instead of the whole body passes the first and fails only the second |
| The real flow, no dev feature | `a_change_requires_a_challenge_answered_against_a_live_binding` | seven steps in one sweep: refused without a grant → challenge → wrong code (403, attempts left) → right code → a reference that is **not** the booking id → the change lands → the walk to `Booked` reusing one grant → revoked, and the booking it made still stands. **Renamed** — see "what review found" |
| The binding is checked against a row | `a_binding_nobody_has_made_cannot_answer_its_own_challenge`, `a_binding_re_verified_since_the_challenge_can_no_longer_answer_it` | **Mutation-verified**: both fail with the store lookup reverted |
| The approved headcount binds | `a_grant_cannot_seat_more_people_than_were_approved` | 500 under a grant for 20 refused, naming the NUMBER not the behaviour; 12 still commits. **Mutation-verified** |
| A reference is not a bearer token | `a_grant_resolves_only_for_the_actor_it_names` | what the scoped revoke rests on |
| `NO` is terminal | `a_declined_request_cannot_be_approved_afterwards` | a later `YES` answers **410 Gone** |
| ADR-025 amendment, property 1 | `the_real_resolver_refuses_a_dev_token` | all three `dev-*` tokens refused by a **running** resolver — and the real credential admitted, so the refusals are about the token |
| ADR-025 amendment, property 2 | `the_dev_authority_flag_does_not_exist_in_this_build` | `#[cfg(not(feature = "dev-authority"))]`, because that IS the property |
| The actor is authenticated | `a_delegated_grant_separates_the_owner_from_the_requester` | asserts the actor is the workload **and** explicitly not `agent:{subject}` |
| Every proposal is checked | the domain suite, 97 tests | **Mutation-verified** across four fixtures that had been wider than they looked |
| The M5 gate | 21 tests, `--features dev-authority` | `the_whole_journey_is_possible_with_curl_alone`, now with the two headers spelled out because a person would have to type them |

**443 workspace tests + 21 in the feature lane.** Clippy clean under
`-D warnings --all-features`.

## What the work found

### The M5 gate had never run in CI

`services/townhall-server/tests/http.rs` carries
`#![cfg(feature = "dev-authority")]` on line 1, and `dev-authority` is not a
default feature. So `cargo test --workspace` — CI's only test step — compiled
**zero** tests from that file. Twenty-one of them, including
`the_whole_journey_is_possible_with_curl_alone`, which *is* M5's acceptance
gate.

It had been that way since the day the file was written (`8d3dabc`, M5), and
nobody noticed because the suite passes whenever anyone runs it by hand.

codex flagged the shape of this in review — *"a test hidden in that
feature-gated file cannot prove closure"* — and it was read as "add a
no-feature lane". The sharper reading was the reverse: CI was running **only**
the no-feature lane, so the gate was the invisible half. Both steps now exist.

**A gate that only runs when somebody remembers to run it is not a gate.**

### Five of seven behaviours never consulted the grant

Inherited from when authority was two capability flags. `Book` and `Cancel`
asked; `SelectVenue`, `VerifySlot`, `ChangeVenue`, `UpdateRequirements` and
`RevalidateVenue` did not.

Lucy approves *"book one meeting room, 20 attendees, max £50."* An agent holding
that grant sends `UpdateRequirements` with 500 attendees. Nothing checked her
grant — the fee ceiling still bound, so the money was safe and the booking was
not.

One central check in `resolve_proposal` now, with `BookingProposal::behaviour()`
a total match so a new proposal cannot default to permitted. Ordered so that
whether a behaviour EXISTS still does not depend on authority: `Undefined` stays
`Undefined`, which is the topology suite's whole point.

### The actor was a string somebody formatted

A grant's actor was `format!("agent:{subject}")` — an identity nothing had
authenticated, invented at issuance. The preview a person reads says *"Agent:
TownHallAgent may book one meeting room"*; if the actor were settled when the
challenge is ANSWERED, a different workload could answer and receive a grant
naming itself, and Lucy's approval of one agent would have authorized another.

So the challenge persists the authenticated actor (migration `0007`) and the
grant inherits it. The column defaults to `''` and fails closed: a row predating
it yields a grant matching no authenticated caller.

Found by the no-feature lane on its first run, at step 5.

### A reader "grant" that could not keep its own promise

Recorded in full in the M7A record; repeated here because the fix is what shaped
this slice. `resolve_reader` returned a `VerifiedAuthority` documented as naming
no resources, which a scope makes impossible. The correction — **listing needs
an identity, touching needs a grant** — is what the whole read/change split
grew from.

## What review found after M7B was written

codex gpt-5.6-sol reviewed the finished slice and found six things. All six were
real; five are fixed here and one is deferred with a reason. The first is the
most serious defect this project has had.

### 1. The workload could approve its own request — and the test proved it

`POST /approvals` returns the preview, and the code is inside the preview
because the person has to be told it. The same workload could then post that
code back. The "wrong channel" check compared `challenge.binding` against the
reply's `from` — **and both came from the caller**. A caller sending the same
pair twice passed. Possession of a workload credential was enough to mint an
arbitrary in-policy grant with no phone involved.

**The end-to-end test passed for exactly that reason.** It was called
`a_booking_needs_an_approval_that_somebody_actually_answered`, and it read the
code out of the HTTP response to its own request and posted it back with the
same credential — with a comment saying "reading it back out here is exactly
what she does". It is not: Lucy reads a code off a phone, an agent reads it off
its own API response, and telling those apart was the test's entire purpose.

**Fixed as far as M7B can.** The verifier now checks the claimed binding against
a **row**: the principal must be currently bound, at the revision claimed. A
binding cannot be invented, and one withdrawn or re-verified since the challenge
was raised no longer answers it.

**Not fixed, and named rather than implied:** this still does not prove a PERSON
answered. The code travels through the workload, and a workload holding the
credential can relay it to itself. Proving otherwise needs evidence from the
channel adapter — an inbound message it actually received — and the channel
arrives with M7C. The test is renamed to
`a_change_requires_a_challenge_answered_against_a_live_binding`, and its header
states the limit.

The fix reached every grant in the workspace: the testkit issuer, `bld_driver`
and `DevAuthority` all now bind a channel before answering. That is the right
ripple — a demo cannot skip the check by being a demo; it supplies the row
itself, which is what makes it a stand-in rather than a bypass.

### 2. The approved headcount was not carried into the grant

The preview promises `Attendees: <= 20`; the grant kept only the fee ceiling and
the booking id. So a grant naming `UpdateRequirements` could take the booking to
500 and continue under the same approval — the money bounded and the booking
not. Lucy approves a room for twenty and ends up with one for five hundred.

`AuthorityConstraints` carries `max_attendees` now, checked centrally beside the
behaviour check, with its own error name (`AttendeesExceedApproval`, 403 — a
grant story, not a data story). A ceiling, so asking for fewer is not a
widening.

### 3, 5, 6 — two false comments and an unscoped revoke

- `may_read_for`'s comment claimed the leak was "whose bookings exist". It is
  full projections, council references, requirements, headcounts, venue status
  and audit histories. Corrected; the behaviour is unchanged because the design
  decision was the owner's.
- Any authenticated workload could revoke any delegation. Now scoped to the
  actor the delegation names, answering identically for "not yours" and "no such
  delegation".
- `b5`'s fixture comment claimed an approval the dev lane never required.
  Corrected, and the property it claimed now has a real witness elsewhere.

### 4 — deferred: the resolver blocks an Axum worker

`RealAuthority` spawns a thread with its own runtime per resolver call and
blocks on `join`. It does not leak and does not deadlock in isolation, but under
concurrency it can occupy every runtime worker and stall unrelated handlers.
Making `AuthorityResolver` async through Axum's handlers is the right fix and a
real refactor; it is scoped separately rather than bolted on.

## Deviations, named

**1. `REVOKE` over SMS is still a stand-in.** The dispatcher answers
*"Delegations arrive with M7; there is nothing to revoke yet"*, which is now
false on this branch — delegations exist, and `POST /delegations/{id}/revoke`
works. ADR-025 assigned verified `REVOKE` to M7C with the rest of the SMS half,
and half-implementing it here would mean a revoke path that authenticated
nobody. Left alone deliberately; M7C removes the message.

**2. The actor allowlist is still fixed.** `RealAuthority::authenticate` maps
one bearer to one `ActorId`. Authenticating a workload credential needs a
credential store and the POC has none — spec §5's "agent/service authentication
as required by the POC" is deliberately thin. What changed is that it is now a
SEPARATE question from authorization: this map answers only "which actor", and
every grant is looked up and checked against that answer. A real deployment
replaces the map and nothing else.

**3. Reading is identity-scoped, and the leak is bounded rather than closed.**
`may_read_for` checks that a live channel binding exists for the named
principal, not that this particular actor serves that particular channel — a
binding records `(address → principal)` and nothing about workloads. So a stolen
workload credential can discover WHOSE bookings exist. It cannot change one.
M7C's read grant, issued at binding time, closes it by making reading revocable
in its own right. **Decided with the project owner**, who chose this over
building the read grant now.

**4. The dev lane cannot witness the resource guard.** `DevAuthority` mints a
grant naming whichever booking the request named. Corrected from the M7A record,
which said this flatly: it turns out the reference and the URL path are two
separate inputs, so when they DISAGREE the guard does fire — discovered when a
scripted edit gave one test a grant for `BKG-A17B` while it acted on `BKG-A17`
and the domain refused. The guard's deliberate witness is still
`a_grant_reaches_only_the_resource_it_names`.

## What M7B does not prove

**The endpoints are not authorized beyond authentication.** Any workload the
resolver knows can raise a challenge naming any grantor. What bounds it is that
raising a challenge does nothing on its own — a person still has to answer from
the bound channel — but "who may ask Lucy for approval" is a question this slice
does not answer. Named here rather than left to be discovered.

**`--authority-key` is a CLI argument.** A real deployment wants it out of the
process table. Out of scope for a POC, and stated so the next person does not
assume it was considered and accepted as fine.

**The concurrency tests remain repeat-count, not exhaustive** — loom cannot
reach a port behind a generic.

## Counts

- **443** workspace tests (default features)
- **21** in the `--features dev-authority` lane, **now run by CI for the first
  time**
- **4** in the no-feature authority lane
- migration **0007**: `approval_challenges.actor`
- `cargo clippy --workspace --all-targets --all-features -- -D warnings` clean
- `target/` down from 49 GB to 2.7 GB, and 446,906 object files to 2,481, via
  `[profile.dev]`
