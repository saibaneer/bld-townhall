-- ADR-017: the audit trail records WHICH PROVENANCE CLASS drove each transition.
--
-- Before this, `proposal` was the only column naming a cause, and `outcome` could
-- only ever hold 'Committed' because `TransitionAudit::committed` was its sole
-- constructor. With three doors and one proposal-shaped column, a fact-driven
-- confirmation had to be recorded as though an intent caused it — so the trail
-- answered "did the model ever cause a confirmed booking?" with yes, wrongly.
--
-- The DEFAULT backfills existing rows as 'Proposal', which is *true* of every row
-- written so far: the proposal door was the only one that existed.
ALTER TABLE audit_events ADD COLUMN driver_kind TEXT NOT NULL DEFAULT 'Proposal';

-- `proposal` was never only a proposal after B3b. Renamed to what it now holds:
-- the name of whichever vocabulary member drove the transition.
ALTER TABLE audit_events RENAME COLUMN proposal TO driver_detail;

-- Free text nobody constrained, gesturing at what `driver_kind` now states.
ALTER TABLE audit_events DROP COLUMN evidence_summary;

-- Phase C needs somewhere to keep WHY. Without it two Rejected outcomes are
-- indistinguishable — both terminal, both referenceless — so "the hall is closed"
-- and "the principal is barred" collapse into one row.
ALTER TABLE effect_intents ADD COLUMN outcome_detail TEXT;

-- What this effect replaced, for the one transition that hands off rather than
-- ends: CancellationRequested + BookingExists finalises the booking intent and
-- creates the cancellation in the same transaction. The successor's uniqueness
-- key identifies only the successor, so without this a replay cannot tell "this
-- exact handoff already happened" from "a different predecessor produced a
-- same-key successor".
ALTER TABLE effect_intents ADD COLUMN supersedes TEXT;
