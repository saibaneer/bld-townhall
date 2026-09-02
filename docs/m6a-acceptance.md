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

1. **A5's deterministic pre-CAS barrier is not implemented — and cannot be.**
   The PR review judged the naming of this residue insufficient and asked for
   the deterministic witness. Working the schedule through settles it: the
   discriminating interleaving for check-then-insert needs a caller parked
   *between* the check and the insert, and any hook that can park there exists
   only in an implementation that has the gap — parking outside the call cannot
   order the two halves of an operation the correct implementation performs as
   one. So the guarantee is made structural instead:
   `ReplayWindow::insert_if_absent` is a single `Mutex`-guarded `entry()` (the
   check and write are one operation), and the API exposes **no read at all** —
   pinned by `the_replay_window_exposes_no_read_to_race_against`, a source scan
   that fails if a query method ever appears — so the caller-side
   check-then-insert cannot be written either. The 16-thread race remains as a
   belt. A model checker (loom) would be the fully deterministic witness and
   costs a cfg-gated lane normal CI never runs; adopted only if this surface
   grows.
2. **The gateway's `IN_FLIGHT` set is a hardcoded const**, and the coverage
   claim is stated exactly (the review caught the first version overstating
   it): only `BookingInProgress` is functionally exercised, by the fault-run
   journey; `CancellationRequested` and `CancellingBooking` are pinned by
   nothing but the const itself. Named as debt; the real fix is a
   domain-exported name list, a domain change M6 does not make.
3. **428 and the malformed-query 400 are unreachable through the gateway's own
   API** — `propose_at` always sends `If-Match`, and typed lookups cannot
   express a bad query. Unrepresentability is the design; the classifications
   are unit-tested against constructed responses in `tests/classify.rs`, and
   the raw-wire cases live in M5.1's server suite.

## Found during the build, and by its review

- **The seam test caught `"Cancel it"` classifying as `CANCEL` + reference
  `"it"`** before M6B was built on it. Fix: a resource argument must *look like
  a reference* — one clause, contains-a-digit, no richer, since anything more
  would be the channel learning the council's namespace.
- **The fault-arming witness was nearly fake.** `fault_id` is an index —
  legitimately `0`. The witness is now `fault_fired() == 1`, asked after the
  turn.
- **The PR review (no HIGHs survived it unfixed) found:** the redaction stopped
  at the leaf types while every wrapper derived `Debug` (all four now render
  masked, asserted by content); the promised `sms-<digest>` booking id existed
  nowhere (now `InboundIdentity::booking_id()`, SHA-256 over length-prefixed
  fields, driven by the duplicate-create test); five assertions that could not
  fail or bypassed their subject (each now driven through the gateway, with
  the plain 503 reaching `Unavailable`, request ids read from the gateway's own
  recording, convergence bounded against a killed council, and the never-re-POST
  claim witnessed by a **recording proxy** — council rows cannot witness it,
  because idempotency hides the second POST).
- **The review's 429 finding uncovered real behaviour:** a contended turn may
  have already committed — the verbatim retry came back `Stale {current: 3}`.
  The client retry loop was therefore wrong by the wire's own contract
  ("re-read and retry") and is **removed**: 429 surfaces immediately as
  `Contended`, and the test now proves the re-read shows a moved version and a
  booking mid-book. `RetryPolicy` lost its contention knob; fresh reads are the
  caller's discipline, not a client loop's.
- **The classifier now polices the wire** rather than reading it charitably: a
  naked `202 {}`, a denial without its name, a hybrid 409, and a tagless 412
  are all `Unrecognized`, unit-tested in `tests/classify.rs` against shapes a
  well-behaved server cannot be made to produce.
- **`VenueRow` had a field the wire never sent** and lacked two it does — green
  because undriven. Corrected, and `venues()`/`slot()` added (the gate
  journey's step 2 is `GET /venues`; their absence was why the 503 test had to
  bypass the gateway).
- **Deepseek (restored on the owner's word) found one real gap in eight
  findings:** the plan's promised assert-the-wait on convergence was missing.
  Its first pass also produced 461 template-looped findings; the retry protocol
  (hard cap, `think:false`) recovered it.
