-- M8-1: the zero-price usage ledger (ADR-027, spec §16). Three tables, additive
-- only, in one migration. Metering bounds RESOURCE consumption; it grants no
-- authority and moves no money — every unit is priced at £0 (§16 preamble). A
-- later slice (M8-2: rate limits + the global provider budget) builds ON these
-- tables and must never re-open this migration.
--
-- # Why the ledger lives here, behind the boundary
--
-- The usage ledger is a TRUSTED resource-accounting component (§9/§16). Its rows
-- live in the same store as bookings and the authority rows, over the one shared
-- pool — NOT in the sms-simulator process that holds the untrusted-facing model
-- seat. The endpoints that write it (`/usage/*`) take the transport-evidence
-- triple and resolve the principal, the intent id, and the unit cost SERVER-side
-- (the /revocations pattern), so a compromised dispatcher cannot name a victim's
-- account or a unit count into being — the same anti-forgery property the
-- authority plane has.

-- One usage account per principal — the funded-account holder ADR-026 anchors
-- approval to. Carries the quota ceiling and the two running totals the meter
-- moves, denormalized so the quota check is one row read, not a ledger scan.
CREATE TABLE IF NOT EXISTS usage_accounts (
    id             TEXT PRIMARY KEY,           -- UsageAccountId
    -- The principal this account meters. UNIQUE: one account per principal, so a
    -- reservation resolves the account by the same PrincipalId a binding names.
    principal      TEXT NOT NULL UNIQUE,
    -- 'active' | 'suspended'. A suspended account meters nothing (M8-2's lever);
    -- M8-1 only ever writes 'active'.
    status         TEXT NOT NULL,
    -- The configured quota, in UNITS (never pence — units are not money, §16).
    limit_units    INTEGER NOT NULL,
    -- Units held by LIVE reservations, and units settled by Debit events.
    -- Remaining quota = limit_units - debited_units - reserved_units; the
    -- conditional UPDATE guard on reserve is what stops it going negative, and
    -- the lazy expiry of stranded reservations is what stops it locking up.
    reserved_units INTEGER NOT NULL DEFAULT 0,
    debited_units  INTEGER NOT NULL DEFAULT 0,
    created_at_ms  INTEGER NOT NULL,
    updated_at_ms  INTEGER NOT NULL
);

-- The live-hold tracker: one row per reserved intent, carrying the state machine
-- (live -> settled | released) and the expiry that makes the release policy
-- DETERMINISTIC (§16.2's "system failure before consumption rescinds according
-- to deterministic policy"). The dispatcher runs in a separate process that can
-- crash between reserve and debit; a stranded 'live' reservation past its expiry
-- is reclaimed by the next reserve on that account, so quota self-heals rather
-- than leaking until lockout.
CREATE TABLE IF NOT EXISTS usage_reservations (
    -- Keyed by the intent, which IS the reservation's identity — a retried turn
    -- (same inbound message) recovers this row rather than holding quota twice.
    usage_intent_id TEXT PRIMARY KEY,
    account_id      TEXT NOT NULL,
    units           INTEGER NOT NULL,          -- units held while 'live'
    -- 'live' | 'settled' (debited) | 'released' (rescinded or expired). The
    -- transitions are conditional UPDATEs (WHERE state='live'), so settle and
    -- release are each idempotent and cannot double-move the account totals.
    state           TEXT NOT NULL,
    expires_at_ms   INTEGER NOT NULL,
    created_at_ms   INTEGER NOT NULL
);

-- Find this account's stranded reservations to reclaim, cheaply.
CREATE INDEX IF NOT EXISTS idx_usage_reservations_live
    ON usage_reservations(account_id, expires_at_ms) WHERE state = 'live';

-- The append-oriented event log (§16.1). Every reserve/debit/release/refund/
-- adjustment is a row; the account totals above are the folded state. The log is
-- the audit trail — a Debit is never edited or deleted, only reversed by a
-- Refund or corrected by an Adjustment.
CREATE TABLE IF NOT EXISTS usage_ledger (
    entry_id        INTEGER PRIMARY KEY,       -- rowid; the log is append-only, so the row order IS its identity
    account_id      TEXT NOT NULL,             -- UsageAccountId
    -- 'Reserve' | 'Debit' | 'Release' | 'Refund' | 'Adjustment' (§16.1). M8-1
    -- exercises Reserve/Debit/Release; Refund (post-settlement reversal) and
    -- Adjustment (the ONLY sanctioned way remaining may go negative, audited)
    -- are migrated but unused seams.
    kind            TEXT NOT NULL,
    -- Signed units the event moves. SQLite INTEGER is i64.
    units           INTEGER NOT NULL,
    -- The intent this event meters. Every Reserve/Debit/Release names its intent,
    -- including the Release the expiry sweep logs per reclaimed reservation.
    -- NULL only for an Adjustment, which answers no single inbound.
    usage_intent_id TEXT,
    created_at_ms   INTEGER NOT NULL
);

-- Meter-once AS A CONSTRAINT, not only a convention (§16.2: "the same
-- UsageIntentId cannot be metered twice"). Only the SETTLING Debit is unique per
-- intent — a redelivered turn's second Debit hits this index and is collapsed to
-- a no-op, a backstop to the reservation state machine even across a restart
-- that emptied the replay window. PARTIAL (WHERE ... IS NOT NULL) because the
-- sweep's aggregate Release and an Adjustment carry a NULL intent.
CREATE UNIQUE INDEX IF NOT EXISTS idx_usage_ledger_debit_intent
    ON usage_ledger(usage_intent_id) WHERE kind = 'Debit' AND usage_intent_id IS NOT NULL;

CREATE INDEX IF NOT EXISTS idx_usage_ledger_account
    ON usage_ledger(account_id);
