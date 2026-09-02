# M6B acceptance — conversation routing, and M6's gate

**The milestone gate holds:** *"Scripted SMS conversation can create/read/cancel
booking without real telecom provider or LLM."* The script is
`services/sms-simulator/scripts/lucy-journey.txt`; the demo binary and the gate
test run it through the same `journey::run`, so the demo and the test cannot
drift — asserted by the test actually executing the binary.

## The battery

| Plan | Test | Notes |
|---|---|---|
| B1 | `m6_gate_the_scripted_journey_clean`, `…_with_the_answer_lost`, `the_demo_binary_is_the_same_journey` | clean: the **complete ordered wire schedule**, exact length, plus council-side witness. Faulted: ack-then-`Automated` two-message shape, fault proven fired, **one** book POST. **Deviation below** |
| B2/B3 | `b2_…`, `b3_…` | the ambiguity question names both candidates; zero POSTs in the observation window (snapshotted after setup); paired with the unambiguous cancel |
| B4 | `b4_cancel_it_survives_a_session_wipe` | candidates come from the wire, not memory |
| B5 | `b5_confirm_reloads_and_follows_the_menu` | out-of-band bump → the walk follows the RELOADED menu (revalidate → verify → book, one POST each, none duplicated), audit departs from the bumped version. **Mutation-verified**: a skip-the-reload dispatcher fails it |
| B6 | `b6_stop_is_channel_control_not_cancellation` | version + audit-length unchanged, council untouched; `Automated` suppressed while `STATUS` delivered |
| B7 | `b7_stop_survives_a_restart` | `FileSuppression` on `std::fs`, rebuilt from the same path |
| B7b/B8 | `b7b_stop_skips_the_turn_and_start_restores_it` | **zero converge calls** while suppressed (counting factory), the server's reconciler settles anyway, START restores the turn. **Mutation-verified**: run-the-turn-suppress-the-message fails it |
| B9 | `b9_an_unbound_address_is_refused_before_any_wire_call` | panic-on-touch wire |
| B10 | `b10_a_token_in_the_body_upgrades_nobody` | hostile proposer, Priya's own booking at `AwaitingBooking`, `BookingAuthorityRequired`, **zero council bookings**; paired with Lucy succeeding |
| B11 | `b11_control_commands_reach_nothing` | panicking proposer + counting wire: zero of each, incl. REVOKE |
| B12 | `b12_balance_consults_the_port_and_answers_honestly` | sentinel; exactly once; only for BALANCE |
| B13 | `b13_unrecognized_text_is_judged_once_and_mutates_nothing` | the proposer's own count proves the text was judged, not swallowed |
| B14 | `b14_delivery_failure_does_not_roll_back` | the failure armed for the very reply carrying the committed booking's news |
| B15 | `b15_conversations_do_not_bleed_across_principals` | M5.1's ownership reaching the channel |
| B16 | (regression, as labelled in the plan) | `topology_matches_the_pinned_matrix` et al. still pass; M6B added no states or proposals |
| tripwires | `tests/boundary.rs` | resolved graph; and the testkit stays out of every NORMAL graph |

## Deviations from the plan, named

1. **The clean schedule differs from the plan's table for THREE reasons, not the
   two first claimed** (the review caught the omission): (a) committed turn
   responses carry the fresh version, replacing pre-proposal reload `GET`s;
   (b) every freeform turn reads `?cancellable=true` as the proposer's context;
   and (c) **the demo script cannot contain the plan's out-of-band bump** — a
   bump is a *test actor's* move, not a message, so the moved-world leg lives as
   its own gate test (`m6_gate_the_journey_with_the_world_moving_under_it`),
   with the bump, the `STATUS` showing the moved count, and the
   revalidate → verify → book walk counted one POST each in the post-bump
   window. The clean schedule itself is asserted as **whole request lines,
   compared for equality** — method, full path, query — after the review found
   fragment-matching passing a changed query shape.
2. **The fault run asserts invariants rather than the plan's "exactly 18"** —
   pinning convergence-GET counts pins the reconciler's cadence into a race —
   but now over the FULL journey: BOOK, CONFIRM under a dropped answer (ack as
   a `Reply`, outcome as `Automated` — both **classes asserted by the runner**,
   not inferred), STATUS of the settled truth, then *cancel it* under a second
   dropped answer, ending with one cancel POST and the council record
   cancelled.
3. **`CONFIRM` stands where M7's approval challenge will.** Named in the
   script's own comments and `Request::Confirm`'s doc; the server's `may_book`
   guard is what disposes, and B10 proves it. (The review concurred: holds.)

### Follow-up semantics, stated exactly

Queued in memory, drained explicitly, **at-most-once**: a suppressed follow-up
is skipped forever (deliberate — the human said stop; the booking's truth stays
reachable through `STATUS`); a binding that has **drifted by drain time** is
dropped before any wire exists (re-resolved against the directory, so one
principal's reference can never land on another principal's phone — the
review's sharpest scenario); a process death loses queued notifications (the
booking still settles server-side). What is NOT accepted: silent suppression
persistence failure — see `FileSuppression`, whose `suppress` now persists
first, commits to memory second, and whose failure reaches the human as "NOT
stopped" rather than as a confirmation with an expiry date.

## Found during the build

- **The journey runner's dedupe collision.** Two script runs share the
  channel's replay window, and each run restarting its turn numbering made the
  second script's first message a "carrier retry" — silently dropped. The gate's
  own fault leg caught it (a missing reply); identities are now process-unique.
  The failure mode is worth remembering: correct dedupe plus careless identity
  generation looks exactly like lost mail.
- **The B5 assertion matched `"book"` inside `"booking-intents"`**, counting
  every request as a book POST. Caught by its own failure output; the match is
  now on `/behaviours/{name}`.
