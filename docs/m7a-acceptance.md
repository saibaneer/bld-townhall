# M7A acceptance — the authority component, and a grant nobody can mint

M7A is the first of ADR-025's three slices: the trusted verifier and issuer,
their persistence, and the migration of every construction site in the
workspace onto real issuance. The HTTP contract is M7B; the SMS half is M7C.
**M7's own gate is not claimed here** — "valid SMS challenge permits £45 scope"
needs the channel, which is M7C.

What M7A does claim: **there is no way, anywhere in this workspace, to obtain a
`VerifiedAuthority` except by answering a real challenge.** Production code,
demo binaries and tests alike.

## The battery

| Area | Tests | Notes |
|---|---|---|
| Canonical scope | `everything_hashed_is_shown_and_everything_shown_is_hashed` | 13 single-field mutations; each must move BOTH the digest and the preview. **Mutation-verified**: hiding the `purpose` line fails it with "the field is hashed but never shown, so nobody approved it" |
| | `length_prefixing_keeps_two_confusable_scopes_apart`, `the_behaviour_set_hashes_the_same_in_any_order`, `the_behaviour_set_dedupes` | **Mutation-verified**: a delimiter-join fails the first; dropping the sort fails the other two |
| | `a_scope_round_trips_through_its_encoding`, `no_edited_byte_decodes_back_to_the_original_scope`, `a_damaged_scope_buffer_is_refused` | every byte, both bits |
| Envelope codec | `the_round_trip_returns_the_grant_that_was_encoded`, `no_edited_byte_decodes_back_to_the_original_grant`, `a_truncated_envelope_is_refused`, `trailing_bytes_are_refused_rather_than_ignored`, `a_foreign_version_tag_is_refused`, `an_unknown_behaviour_name_is_refused` | compared against the **issued** value, never a hand-built one |
| Issuance | `an_answered_challenge_yields_one_grant_that_permits_the_forty_five_pound_booking` | the gate's £45-inside-£50 half, at the component level |
| | `a_grant_approved_at_the_last_second_still_has_its_full_life` | **Mutation-verified**: expiring the grant with the offer fails it |
| | `a_replayed_approval_does_not_mint_a_second_grant` | and asserts the issued grant is reusable three times over |
| | `two_simultaneous_correct_replies_yield_exactly_one_grant` | 64 rounds, 4 workers. **The only witness for the atomic path** — see "What the mutations found" |
| Denials, each isolated | `an_expired_challenge_is_denied_before_the_code_is_read`, `a_wrong_code_costs_one_attempt`, `the_attempt_bound_spends_the_challenge_and_the_right_code_no_longer_helps`, `an_unknown_challenge_is_denied` | one defect per fixture, each naming its own error |
| | `a_reply_from_another_channel_is_denied_and_costs_lucy_nothing` | **Mutation-verified**: reading the code before the channel fails this and two others |
| | `a_binding_at_a_newer_revision_cannot_answer_an_older_challenge` | state outliving the moment it was true, refused |
| | `a_rejected_challenge_stays_rejected`, `a_rejection_without_the_code_is_refused` | `NO` is terminal; a later `YES` does not revive it |
| Assurance | `the_grant_never_claims_more_assurance_than_the_binding_established`, `dev_assurance_never_clears_an_sms_minimum`, `every_assurance_level_round_trips_through_its_durable_name` | **Mutation-verified**: trusting the challenge's level over the binding's fails the first |
| Resource scope | `a_grant_reaches_only_the_resource_it_names` | one approval must not reach a neighbouring booking |
| Delegation | `a_delegated_grant_separates_the_owner_from_the_requester` | grantor `lucy`, subject `marco`, actor `agent:marco` |
| Revocation | `revocation_takes_effect_at_once_and_twice_is_not_an_error`, `an_expired_grant_no_longer_resolves`, `an_unissued_reference_resolves_to_nothing` | liveness is the resolver's alone |
| Sealing | `neither_the_approval_nor_the_grant_can_cross_a_wire` | and the compiler: `tests/sealing.rs` **cannot** construct either type |
| SQL (10) | `a_grant_round_trips_through_sqlite_unchanged`, `two_simultaneous_replies_over_sqlite_yield_exactly_one_grant`, `the_attempt_count_survives_a_reopened_database`, `revocation_survives_a_reopened_database`, `a_challenges_scope_survives_a_reopened_database` | three genuinely close and reopen the database. **Mutation-verified**: dropping the conditional from the settling UPDATE, and resetting the attempt counter, each fail exactly one |
| | `one_address_cannot_hold_two_live_bindings`, `a_withdrawn_binding_does_not_answer_who_is_this_number`, `re_verifying_a_number_strands_the_challenge_bound_to_its_old_revision` | **Mutation-verified**: returning withdrawn bindings fails the second |
| | `a_challenge_whose_digest_contradicts_its_scope_is_refused` | reaches past the port with raw SQL, because a contradiction cannot be built through the API. **Mutation-verified** |

`--features dev-authority`: **21 tests**, including
`the_whole_journey_is_possible_with_curl_alone`. **M5's gate survives M7A.**

## What the mutations found

Five defects, three of them in code written minutes earlier — and the
fifth in the ADR's own central distinction, which turned out to have no witness
at all. It is recorded under the grantor/subject split below.

**The scope had one deadline, and it was wrong.** Approving in the last second
of the reply window issued a grant that had *already expired* — and every test
that approved immediately would have passed. There are two deadlines now, both
hashed and both shown: how long you have to reply, and how long the permission
lasts.

**`decode` sized an allocation from the bytes it was distrusting.** The edit
battery flipped one byte of a length prefix, `Vec::with_capacity` asked for 72
petabytes, and the process died with SIGABRT. This is why the battery walks
every byte rather than a chosen few.

**The replay test was witnessing the wrong layer.** Replay is guarded twice —
the service checks status before spending an attempt, the store checks it again
inside its atomic settle. Removing *either* left all 20 tests green; only
removing both failed anything. So the atomic layer, the only defence against two
simultaneous correct replies, had **no witness at all**. It has one now, and
that test fails on round 0 when the layer is removed. Both layers carry a
comment saying which case each one owns.

**A reader "grant" that could not keep its own promise, guarded by a vacuous
assertion.** `resolve_reader` returned a `VerifiedAuthority` documented as
naming no resources — impossible, because a grant's resource list comes from an
approved scope and a scope always names one booking. It named a synthetic
`dev-reader-lucy`: authority over an imaginary booking, which is still
authority. The `debug_assert!` protecting it read
`booking.is_none_or(…)` inside a branch where `booking` is `None` by
construction — vacuously true.

The fix was to say what is actually true: **listing your own bookings needs an
identity; touching one needs a grant.** `resolve_reader` returns a
`PrincipalId`, `lookup` takes one, and a reader can authorize nothing because it
is not the kind of thing authorization is made of. No assertion required, and
none written.

## The 24 construction sites

Nothing in the workspace writes a `VerifiedAuthority` literal any more.

| Where | Before | Now |
|---|---|---|
| `townhall-domain` unit tests | 3 helpers + 3 inline literals | `test_grants::issued`, delegating to the testkit issuer |
| `townhall-domain/tests/topology.rs` | one literal | real issuance; the topology it pins does not depend on authority, which is its point |
| `townhall-service/tests/protocol.rs` | `authority()` at 93 sites | **`authority_for(&id)`** — the fixture is resource-scoped, so a test reaching for the wrong booking now fails |
| `council-client` tests | 2 helpers | resource-scoped, issued |
| `bld_driver` (a **binary**) | one literal | its own composition root: raises a challenge, prints the preview, answers it. Approval now precedes `repo.create`, which is §23.1's ordering arriving early |
| `DevAuthority` (a **binary**) | three literals | issues real grants pinned to `AssuranceLevel::Dev` with a 5-minute TTL |

**No `test-support` constructor was added.** The rule ADR-025 set —  a cargo
feature revealing a minting path leaks through unification, so it closes a
backdoor only on paper — held under pressure from 24 sites, a binary, and a
demo driver. `townhall_testkit::issuer` is the entire cost, paid once.

## The grantor / subject split, applied

| Reads | Uses | Because |
|---|---|---|
| `owner` column at create, `load_visible`, `lookup_by_ref`, `lookup_cancellable` | **grantor** | on whose behalf — the owner, and the visibility scope |
| council's `BookingEffect::Book`/`CancelBooking` principal, `cancelled_by`, the denial log | **subject** | who the action is attributed to (ADR-020) |

ADR-022's inherited debt is discharged: the Marco-cancels-Lucy's-booking test
was previously faked by renaming one `principal` field, and is now a genuine
`GrantSpec::delegated("lucy", "marco", …)`.

### The fifth defect: the split had no witness at all

Swapping `access.grantor()` for `access.subject()` on the `owner` column broke
**nothing**. 439 tests, all green.

The reason is worth recording, because it generalises: in every fixture in the
workspace the two principals were the SAME VALUE, so no assertion could tell
them apart. The one delegated case in `protocol.rs` creates its booking under a
non-delegated grant and only cancels under the delegation — so the `owner`
column was never once written from a grant whose grantor and subject differ. A
whole ADR's central distinction, implemented and unwitnessed.

`a_booking_created_under_delegation_belongs_to_the_grantor` is the missing
witness: Marco holds Lucy's delegation and does the creating, so the two roles
are different values at the moment the column is written. It asserts the row is
visible to Lucy through `load_visible` — the predicate that actually decides
visibility, rather than a column read — and NOT visible to Marco, who holds the
very grant that created it. **Mutation-verified**: it is the one test that fails
when the accessors are swapped.

## Deviations from ADR-025, named

**1. The resolver's resource-awareness was pulled forward from M7B.**
`AuthorityResolver::resolve(bearer)` had no booking in hand, but a grant names
its resource, and the curl suite uses 29 different booking ids — so no
fixed-resource dev grant could serve it. Leaving the tree broken was the only
alternative. The signature is now
`resolve(&self, bearer, booking) -> Option<VerifiedAuthority>` plus
`resolve_reader(&self, bearer) -> Option<PrincipalId>`.

This is the smaller half of codex's finding #1 ("two decisions: authenticate the
actor; authorize the presented delegation against that actor, service, resource
and behaviour"). **Still M7B's:** the `Authorization` / `X-BLD-Delegation` split,
un-reserving that header, and binding the actor to an authenticated workload.

**Recorded in ADR-025** under "Amendment, made during M7A", so the plan of
record and the code agree about which slice owns this seam.

**2. The preview renders expiry relatively, not as §13.2's wall clock.**
"Reply within 10 minutes" rather than "17:00 Thu 20 Aug": a calendar date needs
a date library or hand-rolled civil-from-days arithmetic, and the relative form
is both what a person acts on and impossible to get wrong by a timezone. The
absolute deadline still governs — the verifier reads `expires_at_ms`, never the
string.

**3. The preview shows `Purpose:` and `Reference:`, which §13.2's example
omits.** The rule is "everything hashed is shown", not "everything the example
showed". A field covered by the digest but absent from the preview is a field
nobody approved.

**4. The one-time code is stored as issued, not hashed.** ADR-023 established
that an unkeyed digest of a low-entropy value is an encoding of it; a
four-digit code has ten thousand candidates, so a hash column would buy the
appearance of protection and none of it. What bounds the risk is the attempt
count and the reply deadline. A keyed MAC would be real protection and needs a
key to live somewhere — M7B's composition root.

## What M7A does NOT prove

**The curl lane cannot witness the resource guard.** `DevAuthority` mints a
grant naming whichever booking the request named, so its resource check can
never refuse anything there. That is honest for a lane where nobody was asked,
and it means the 21 curl tests witness the *behaviour* guard
(`dev-priya-nobook` refused `Book`) and the *ownership* guard (M5.1's scoped
rows) — never the resource guard. The resource guard's witness is
`a_grant_reaches_only_the_resource_it_names`, where a grant issued for one
booking is asked about another.

**The sealing's witness is a compile error, not a test.** `tests/sealing.rs`
cannot construct a `VerifiedAuthority` or a `VerifiedApproval` at all, which is
stronger than any assertion and unrunnable as one. A `trybuild` lane pinning the
error itself is **deferred and named here** so the absence is on the books.

**The concurrency tests are repeat-count, not exhaustive.** 64 rounds in memory,
8 over SQLite. Loom cannot reach a port behind a generic, so a scheduler that
never loses would pass — mitigated by the round count and by the mutation check
failing on round 0.

## Counts

- **440** workspace tests (439 before the migration — the count held because no
  test was lost, only re-based onto real issuance; +1 is the delegated-create
  witness the mutations proved was missing)
- **+51** new: 41 in `townhall-authority`, 10 in `townhall-store`
- **21** in the `--features dev-authority` lane
- `cargo clippy --workspace --all-targets --all-features -- -D warnings` clean
- migration **0006**: `channel_bindings`, `approval_challenges`, `delegations`
