# M6A acceptance — the channel core and the town-hall gateway

Not the M6 milestone gate (that is M6B's scripted-SMS conversation); M6A is the
half it stands on. PLAN-M6A's battery, mapped to the tests that discharge it,
with deviations named rather than smoothed over.

## The two gates

| Gate | Test | Witness |
|---|---|---|
| (a) the wire journey | `m6a_gate_a_full_journey_through_the_gateway_alone` | creation → `Booked` → `Cancelled` through the gateway alone, **clean** (200 throughout — an answering council settles synchronously) and **faulted** (202 with the drop-response fault armed and *proven fired*, then convergence). Council-side witness: two bookings, both cancelled |
| (b) the channel contract | `m6a_gate_b_the_complete_channel_contract` | one continuous in-process run: normalize, bound, dedupe, classify into all three arms, segment, truncate, suppress, report honestly |
| the seam | `the_seam_m6b_consumes_is_exactly_this` | every arm and payload M6B's dispatcher will match on, pinned as data |

## The battery

| Plan | Test | Notes |
|---|---|---|
| A1 (+A2 merged) | `a1_addresses_normalize_against_a_configured_region` | incl. the trunk-zero rejection and the different-region discriminator |
| A2 | `a2_inbound_body_rejects_and_preserves` | byte-for-byte round trip of 600 emoji — the row a `BoundedString` reuse fails |
| A3 | `a3_segment_counting_is_exact` | iterates the literal `GSM_BASIC: [char; 128]` (length asserted) and the full extension table; `£` basic, `€ × 81`, `Ж` → UCS-2, 35/36 emoji |
| A4 | `a4_outbound_truncation_reserves_room_for_its_marker` | recounted by the test's own counter |
| A5 | `a5_dedupe_keys_on_identity_atomically`, `a5_concurrent_redelivery_admits_exactly_one` | **deviation, see below** |
| A6 | `a6_classification_is_strict_and_shallow` | `BOOK …` is `Freeform`; `"STOP the booking"` is a sentence |
| A7 | `a7_resource_arguments_are_carried_not_interpreted` | incl. `CANCEL it` → `Freeform` — **found by the seam test**, see below |
| A8 | `a8_delivery_outcomes_are_driven_not_just_typed` | all three receipts driven through configured simulator behaviour |
| A9 | `a9_debug_renderings_are_exactly_these` | equality against complete strings, asserted with a `YES 7312` body |
| A10 | `a10_create_round_trips_every_field` | independent DTOs — drift fails here, not downstream |
| A11/A12 | `a11_a12_duplicate_create_distinguishes_owner_from_stranger` | the two 409s as two variants |
| A13 | `a13_propose_sends_the_version_it_was_given` | |
| A14 | `a14_the_status_contract_is_keyed_on_more_than_the_number`, `a14_the_two_503_shapes_are_distinguished` | both 403 guards **by name**, both 422s, **both 503s** (council killed mid-test), 404-for-invisible, 401 |
| A14b | `a14b_request_ids_survive_the_round_trip` | echo-verbatim and mint-when-absent, both |
| A15/A16 | `a15_a16_acceptance_returns_before_convergence` | the fault **proven fired** (`consumed == 1`), the booking still in flight when `propose_at` returns, the first poll waits ≥ `Retry-After` |
| A17 | `a17_contention_backs_off_and_gives_up_typed` (429, via `--reclassify-attempts 0`), `a17_convergence_is_bounded` | both bounded ends typed |
| A18 | `a18_ownership_reaches_the_client` | M5.1 as the client sees it, refusal paired with the owner succeeding |
| A19 | `boundary.rs` | `cargo metadata` resolved graph; the channel's check covers dev-deps too |

## Deviations from the plan, named

1. **A5's deterministic pre-CAS barrier is not implemented.** The plan wanted
   caller 1 parked immediately before the CAS. The built `insert_if_absent` is a
   single `Mutex`-guarded `entry()` — the check and the write are structurally
   one operation, so there is no seam to park in without adding a test-only hook
   whose existence would loosen the very structure under test. The witness is
   the sequential contract plus a 16-thread race asserting exactly one
   `Accepted`. Honest statement of the residue: a check-then-insert
   implementation would fail the race only probabilistically. Accepted because
   making it deterministic costs the structure that makes the bug unwritable.
2. **The gateway's `IN_FLIGHT` set is a hardcoded const.** The crate has no
   domain dependency, so it cannot ask `pursuit()`. Functional coverage exists —
   the gate's fault run converges through `BookingInProgress` — but a new
   in-flight state added to the domain would not fail a gateway test by itself.
   Named as debt; M6B's dispatcher tests deepen the functional coverage, and the
   real fix (a domain-exported name list) is a domain change M6 does not make.
3. **428 is unreachable through the gateway** — `propose_at` always sends
   `If-Match`, so the gateway cannot produce the missing-precondition case
   through its own API. The status is classified (`PreconditionRequired`) but
   exercised only at the wire level in M5.1's suite.

## Found during the build

- **The seam test caught `"Cancel it"` classifying as `CANCEL` + reference
  `"it"`** before M6B was built on it. Fix: a resource argument must *look like
  a reference* — one clause, contains-a-digit, deliberately no richer, since
  anything more would be the channel learning the council's namespace. A missed
  digit-free reference degrades gracefully to `Freeform`.
- **The fault-arming witness was nearly fake.** `fault_id` is an index —
  legitimately `0` — so `assert!(id > 0)` was wrong, and its passable variants
  would have let a test expect a 202 whose fault never fired. The witness is now
  `fault_fired() == 1`, asked after the turn.
- **Deepseek (restored to the loop on the owner's word) found one real gap** in
  eight findings: the plan promised an assertion that convergence waits at least
  `Retry-After`, and the first build forgot it. Added. Six of the other seven
  were false against the code; one is an M6B placement note (the durable
  suppression store lives in the orchestrator, on `std::fs`, no new dependency).
