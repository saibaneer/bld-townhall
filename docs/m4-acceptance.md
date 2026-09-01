# M4 acceptance — the 25 required tests, mapped

The guidance's required failure-injection list (`m4-effects-guidance.md` §"Required
failure-injection tests": 1–22 plus 2a–2c) against the tests that actually run, at the
tree this document is committed to. The rule this map is written under: **a citation only
counts if a wrong implementation would fail the cited test, asserted from the right
witness** — a database file, a killed process, a server-side counter — never from a
response that reads the same whether the claim is true or false.

Paths are workspace-relative; `reconciliation.rs` = `crates/council-client/tests/`,
`harness.rs`/`registry.rs` = `services/mock-council/tests/`, `protocol.rs` =
`crates/townhall-service/tests/`, `over_http.rs` = `crates/council-client/tests/`.

| # | Requirement (abbreviated) | Test | What kills the cheat |
|---|---|---|---|
| 1 | crash before provider call — intent exists, provider has nothing | `reconciliation.rs::a_crash_before_the_call_is_finished_by_recovery` and `::a_crash_before_the_call_that_outlives_its_deadline_fails_closed` | The driver process really aborts; both halves read from the two databases before recovery runs (intent durable; council's `effects` empty). First test: recovery completes the booking (owner's ADR-020 decision) — exactly one booking in the council's file. Second: past the deadline it fails closed, bookings zero. |
| 2 | provider rejects before effect — no booking | `registry.rs::a_create_asserting_the_wrong_fee_is_rejected`; workflow half `protocol.rs::a_permanent_refusal_returns_to_awaiting_with_a_fresh_identity` | `ProviderRejected` durable in the council's DB with `booking_count` 0; re-proposal mints a fresh identity. |
| 2a | rejection committed before observable — crash between | `harness.rs::a_rejection_no_one_heard_survives_a_kill` | Real SIGKILL at the armed `after_settle_commit` pause; restart; the retried resolve carries the original reason. |
| 2b | rejection survives clock rollback | `harness.rs::a_rejection_survives_the_clock_winding_back` | Council restarted with its clock a day earlier; the same-id retry is still `ProviderRejected` — recorded state refused it, the clock had no say. |
| 2c | signed wrong-kind fact with the active id | `reconciliation.rs::a_signed_wrong_kind_answer_is_refused_by_the_domain_not_the_wire` | The armed answer is genuinely signed (passes the verifier); the domain refuses it; the denial-log row is read back, not narrated. Cell classification pinned by the domain matrix. |
| 3 | provider commits, response dropped — one booking | `reconciliation.rs::the_dropped_response_converges_to_exactly_one_booking` | Fault consumption asserted; council's file counted before and after recovery: exactly one. "If this is 2, M4 is not complete." |
| 4 | retry after dropped response — same identity, original result | same as 3 (recovery leg) plus `registry.rs::a_retried_create_returns_the_original`, `over_http.rs::a_retry_over_http_returns_the_original` | `due()` returns exactly the original identity; a same-id create retry returns the byte-identical original with count 1. |
| 5 | restart between provider commit and local evidence commit | `reconciliation.rs::a_crash_after_the_councils_commit_adopts_rather_than_duplicates` | Real abort after the call; council count 1 before recovery; adoption asserted from both stores, plus budget honesty (started 2 / finished 1 — the crash is in the ledger). |
| 6 | malformed or field-perfect forged evidence rejected | `over_http.rs::a_field_perfect_response_from_the_wrong_key_is_refused`, `::an_unsigned_response_is_refused`; `reconciliation.rs::garbage_and_delay_become_unknown_never_facts` | An impostor-signed but field-perfect body is refused; garbage and unsigned resolve answers stay `StillUnknown` through the real reconciler. |
| 7 | provider lookup unavailable — stays unknown/in-progress | `reconciliation.rs::an_unreachable_council_leaves_the_booking_unknown_and_in_flight` | The council process is really killed; `attend` twice returns `StillUnknown` with budget spent; state stays `BookingInProgress`. Connection-refused mapped to absence dies here. |
| 8 | two workers, one pending effect — one booking | `protocol.rs::two_coordinators_racing_one_booking_ask_the_council_once` | The council's CALL COUNT is the assertion — relying on provider idempotency to absorb our own double-send is precisely what ADR-014 refuses. |
| 9 | cancellation during ambiguity | `reconciliation.rs::a_cancellation_requested_during_ambiguity_ends_cancelled_when_the_booking_exists` and `::a_cancellation_requested_for_a_booking_nobody_received_fails_closed`; local shape `protocol.rs::a_cancel_mid_flight_commits_locally_and_touches_no_wire` | Two fixtures, deliberately distinct: booking-committed-answer-eaten vs never-delivered. No-wire witness at the proposal (council's request counter and `effects` table). Ending (a): one booking, one cancellation, from the council's file. Ending (b): tombstoned absence → `Cancelled`, bookings pinned at zero — a wanted-table answering "send" here books the room Lucy is cancelling — and no cancel intent ever minted. |
| 10 | reconciliation discovers an unadopted provider effect | `reconciliation.rs::a_crash_after_the_councils_commit_adopts_rather_than_duplicates` (Book) and `protocol.rs::re_observing_a_settled_booking_converges` | The provider effect exists, local state does not reflect it, and reconciliation adopts rather than duplicates. |
| 11 | cancel commits, response dropped — exactly one cancellation | `reconciliation.rs::a_cancellation_whose_answer_is_eaten_converges_to_one_cancellation` | Fault consumption asserted; `cancelled_by` rows counted from the council's file: one, before and after recovery; the request counter shows recovery did NOT re-send what the query answered. |
| 12 | crash between `CancellingBooking` commit and the cancel call — resume same identity | `reconciliation.rs::a_death_between_the_handoff_and_the_cancel_call_is_resumed_under_the_same_identity` (post-mark, a real abort) and `::a_death_before_the_first_mark_leaves_a_prepared_cancel_that_recovery_sends` (pre-mark) | Post-mark: `bld_driver --reconcile --die before-call` aborts at the capability entry of the first send recovery decides on; crash state read raw (`Unknown`, 1/0; council ignorant of the cancel id); the resumed send is the FIRST arrival the council ever counts — same identity by construction. Pre-mark: nothing is in flight in that window, so the crash state IS the committed database, reopened fresh and executed by the first-send leg. |
| 13 | cancel retried — same id returns original, not twice | `reconciliation.rs::a_cancel_retried_under_the_same_identity_returns_the_original` | TWO server-side arrivals by the council's own counter; ONE durable cancellation; the retry's signed body verifies to the durable original (the dropped first answer is unobservable by definition). |
| 14 | pre-deadline "not found" is Unknown, never absence | `reconciliation.rs::a_cancellation_requested_for_a_booking_nobody_received_fails_closed` (pre-deadline leg) and `protocol.rs::a_requested_cancellation_only_asks_and_never_sends` | `StillUnknown`, state unmoved, and — the F-specific half — nothing is SENT under `CancellationRequested`: the wire log and the council's counter both show asks only. |
| 15 | accepted pre-expiry, commit paused past it, concurrent lookup | `harness.rs::a_create_overtaken_by_its_deadline_while_paused_is_refused`; ordering half `registry.rs::nothing_is_discoverable_before_the_settlement_commits` | Commit-time-vs-receipt-time proven against the real process (SETCLOCK past the deadline inside the held write). The concurrent-lookup leg is structurally unrunnable there and is *stated* as such in the guidance (scope note at item 15), with the ordering property proven in-process from a second connection. |
| 16 | our clock ahead — absence never manufactured locally | `reconciliation.rs::a_cancellation_requested_for_a_booking_nobody_received_fails_closed` (pre-deadline leg) and `::garbage_and_delay_become_unknown_never_facts` | Our store clock advances far past the 30s effect TTL in both; the answer stays `StillUnknown` because only the council's tombstone ever means absence — local deadline evaluation would fail these asserts. |
| 17 | post-expiry `EffectAbsent` loses a CAS and is re-applied | `protocol.rs::a_lost_cas_reapplies_the_same_verified_absence` | The CAS loss is forced deterministically (a wrapper commits the competing Cancel before the first finalize); witnessed by the finalize count (2) and the audit trail's ORDER (Proposal/Cancel then Fact/EffectAbsent) — the final state alone is not accepted. |
| 18 | committed just before expiry stays discoverable after | `registry.rs::a_created_effect_stays_discoverable_past_its_deadline` (in-process) and `harness.rs::answers_verify_across_a_restart_with_the_same_key` (across a real restart) | A post-deadline lookup returns the original booking, never a second; the same signing key still verifies after the council's death. |
| 19 | council clock steps back after definitive absence | `registry.rs::a_tombstoned_identity_stays_refused_after_the_clock_winds_back` | The rewound clock would say "plenty of time"; the tombstone refuses anyway. |
| 20 | council crashes before the tombstone commits | `harness.rs::an_uncommitted_answer_dies_with_the_process` | The discriminating read: the council's FILE, between the kill and the restart, holds zero rows for the identity — a council that committed or leaked before its pause fails here, where the wire answer could not catch it. |
| 21 | council crashes after tombstone commit, before responding | `harness.rs::an_answer_no_one_heard_survives_a_kill` | Retried lookup returns the same `DefinitivelyAbsent`, AND the later create attempt is refused by the crash-survived tombstone — one determination, no second row. |
| 22 | expiry-refused create, clock rolls back, same-id retry | `harness.rs::an_expiry_refusal_survives_the_clock_winding_back` | One sequence against the real process: refused for expiry, restarted below the deadline, retried — still refused, one effects row, zero bookings. A council re-judging expiry against its clock books the room here. |

## The machinery gates carried with the map

Beyond the numbered list, the pursuit/recovery machinery that the numbered tests stand on
is itself pinned: lease fencing and claim-is-the-gate in the schema
(`townhall-store` `pursuit::*`), once-only escalation and the rollback skew clamp, the
migration preflight (`migration_gate::*`), per-call attempt accounting with the
ask-before-send order (`protocol.rs::a_query_and_resend_turn_counts_both_attempts_and_asks_first`),
the resend privilege's negative space
(`reconciliation.rs::an_unsigned_not_yet_authorizes_nothing`,
`::a_signed_not_yet_for_someone_else_authorizes_nothing`, the fake's four first-seen
binding tests), the escalated-cancellation end-to-end story
(`protocol.rs::an_escalated_cancellation_is_finished_by_the_late_fact`), and the composed
escalation race under a held call
(`protocol.rs::an_escalation_during_a_held_call_is_fenced_and_the_fact_still_lands`).

Deferred, named, with reasons (unchanged from slice E): the flood-vs-boundary timing gate
(wall-clock assertions in CI; the isolation is structural — the denial log is its own file
with its own writer) and the loop source-scan (no loop binary exists until M5's service).
