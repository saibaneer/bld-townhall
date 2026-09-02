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

1. **The clean schedule is 14 requests, not the plan's 16 — and shaped
   differently.** Two reasons, both discovered by building rather than
   deciding: (a) each committed turn's *response* carries the fresh version, so
   no separate reload `GET` precedes a proposal that directly follows one — the
   reload rule is satisfied by reading the server's own last answer, one
   round-trip earlier; (b) every freeform turn costs one
   `GET ?cancellable=true` as the proposer's projected context, which the
   plan's table omitted. The schedule is asserted **exactly** (ordered, full
   length, no others), which is what the plan's number existed to guarantee.
2. **The fault run's "exactly 18" became invariants rather than a count.** The
   convergence `GET`s ride the reconciler's real cadence; pinning their number
   would pin a race. What is asserted: exactly **one** book POST (never
   re-POSTed), the fault fired, the ack is a `Reply` and the outcome an
   `Automated` message, one council booking. The council's pause/barrier lane
   remains available if a count is ever worth its determinism cost.
3. **`CONFIRM` stands where M7's approval challenge will.** Named in the
   script's own comments and `Request::Confirm`'s doc — a stand-in, not a
   design: nothing in M6 treats the word as authority, and the server's
   `may_book` guard is what B10 proves still decides.

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
