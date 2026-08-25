-- M4 slice E: the pursuit axis (ADR-019).
--
-- Giving up is not a state and not an outcome. These columns are where the
-- facts about OUR OWN chasing live — the status column's remaining values are
-- preparation, our knowledge state, or provider determinations, and the one
-- value that was a decision of ours (`Abandoned`) is gone. A database still
-- holding it is refused before this migration runs — see the repository's
-- open-time preflight, which exists because `SELECT RAISE(...)` is not legal
-- SQL outside a trigger (review caught the first draft of this file using it).

-- Calls begun. Bounds the retry loop even across crashes: incremented before
-- the call, so a process that dies mid-ask still spent budget. Conservative by
-- decided choice — the alternative lets a process that always dies mid-call
-- retry forever.
ALTER TABLE effect_intents ADD COLUMN attempts_started INTEGER NOT NULL DEFAULT 0;

-- Calls that returned control, answer or not. Deliberately not "completed":
-- an `Unknown` produced no answer, and a column claiming completion would say
-- more than happened. Recorded so the audit can say "five started, one
-- finished" rather than implying five conversations took place.
ALTER TABLE effect_intents ADD COLUMN attempts_finished INTEGER NOT NULL DEFAULT 0;

-- The reconciler's own pacing. Escalation lengthens this rather than stopping
-- the asking: the council is pull-only, so a design where giving up silences
-- the reconciler has an exit its own stop condition guarantees is never
-- produced (ADR-019 §3).
ALTER TABLE effect_intents ADD COLUMN next_attempt_after_ms INTEGER NOT NULL DEFAULT 0;

-- Who owns this intent right now. Expiry re-opens the row — a crashed owner's
-- work must be recoverable — and the token is what fences the crashed owner's
-- LATE writes: every write of a turn carries the token it claimed with, and a
-- stale token matches nothing.
ALTER TABLE effect_intents ADD COLUMN lease_until_ms INTEGER;
ALTER TABLE effect_intents ADD COLUMN lease_token INTEGER NOT NULL DEFAULT 0;

-- NULL until we gave up. The human queue is one predicate over this:
-- escalated and still unresolved.
ALTER TABLE effect_intents ADD COLUMN escalated_at_ms INTEGER;

-- The attempt count at the moment of giving up, derived IN the escalation
-- write (`escalation_attempts = attempts_started`) so nobody asserts it. The
-- reason vocabulary is about our accounting only — never about the council.
ALTER TABLE effect_intents ADD COLUMN escalation_attempts INTEGER;

-- Dead since slice A: written by nothing, read by nothing, on no struct.
-- Review found it; a column that exists only in the DDL is a fact nobody owns.
ALTER TABLE effect_intents DROP COLUMN last_error;

-- Pre-E `Prepared` rows may lie. The old Phase B could call the council, time
-- out, and leave the intent `Prepared` — so after this migration that row
-- would read "never attempted" about a call that may have booked a room. The
-- predicate is the JOIN, not the booking's state: one booking can hold a stale
-- `Prepared` predecessor beside the intent it actively waits on, and only the
-- active one is suspect. Conservative in the safe direction: `Unknown` gets
-- re-asked; a false "never attempted" does not.
UPDATE effect_intents
   SET status = 'Unknown', attempts_started = 1
 WHERE status = 'Prepared'
   AND effect_intent_id IN (
        SELECT active_effect FROM bookings WHERE active_effect IS NOT NULL
   );

CREATE INDEX IF NOT EXISTS idx_effect_intents_due
    ON effect_intents(next_attempt_after_ms)
    WHERE status IN ('Prepared', 'Unknown');

CREATE INDEX IF NOT EXISTS idx_effect_intents_escalated
    ON effect_intents(escalated_at_ms)
    WHERE escalated_at_ms IS NOT NULL;
