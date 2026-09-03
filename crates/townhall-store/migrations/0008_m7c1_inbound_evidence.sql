-- M7C-1: the receipt seam (ADR-026). Two tables and one column, in one
-- migration because the ADR forbids splitting the evidence schema from the
-- correlation schema across slices — a later slice must never re-open a migration
-- this one applied.
--
-- # What the receipt is for
--
-- Until now `YES 7312` proved a PERSON answered only because the untrusted
-- orchestrator SAID so — it presented `TransportEvidence { verified: true }` it
-- had constructed itself. That is not proof, it is the caller's word (the first
-- ADR-026 draft died on exactly this). The fix is store-mediated: a trusted
-- ingress writes the inbound's transport evidence to `inbound_evidence` under an
-- opaque RECEIPT, and the verifier reads the row back. The proof stops being
-- "what the client sent" and becomes "which row the verifier read" — and against
-- the model/proposer seat, which holds no store handle and no HTTP client, that
-- is structural. (Against a compromised orchestrator PROCESS it is the SMS-demo
-- assurance level §13.1 names; M12's signed webhook closes that last inch.)

-- One inbound message's transport evidence, deposited under a one-use receipt.
CREATE TABLE IF NOT EXISTS inbound_evidence (
    -- The opaque handle the orchestrator forwards in place of the evidence
    -- itself. Unguessable (issuer entropy), so it is not a bearer token anyone
    -- can compute — the same property a `DelegationId` needs.
    receipt             TEXT PRIMARY KEY,
    -- The transport-set identity triple. Set by the ingress from the carrier,
    -- NEVER a caller-chosen field — this is what stops the model seat naming a
    -- row into being by picking a sender.
    provider            TEXT NOT NULL,
    provider_account    TEXT NOT NULL,
    provider_message_id TEXT NOT NULL,
    -- The address the message came from (E.164, normalized). The verifier
    -- resolves this to a live binding rather than trusting a claimed identity.
    claimed_sender      TEXT NOT NULL,
    -- The provider's verified bit and opaque signature, as deposited. SQLite has
    -- no bool, so 0|1. The signature is never logged.
    verified            INTEGER NOT NULL,
    signature           TEXT,
    -- The challenge this evidence answers, BOUND AT DEPOSIT.
    --
    -- This is the whole anti-cross-submit mechanism: a person with two live
    -- challenges (a £45 and a £5000) who answers the £45 produces a valid row for
    -- their number, and the orchestrator — which raised both and knows both
    -- codes — could otherwise present that row against the £5000. Binding the row
    -- to its challenge here, and requiring equality in `submit`, is what refuses
    -- that. Nullable and NOT a foreign key: the row must outlive the challenge's
    -- settlement (it is one-use bookkeeping and audit), so it cannot cascade.
    challenge_id        TEXT,
    -- Set exactly once, in the same transaction that settles the challenge, and
    -- never cleared. One-use: a receipt spends once.
    consumed_at_ms      INTEGER,
    created_at_ms       INTEGER NOT NULL,
    -- The row is bounded litter, not durable: an answer row is consumed at
    -- settlement or swept after this deadline. The sweep is out of this slice,
    -- but the column it reads is here.
    expires_at_ms       INTEGER NOT NULL
);

-- One row per inbound identity, so a carrier REDELIVERY (the replay window is
-- in-memory and does not survive a restart) maps to the SAME receipt rather than
-- a second row — the evidence analogue of idempotent begin.
CREATE UNIQUE INDEX IF NOT EXISTS idx_inbound_evidence_identity
    ON inbound_evidence(provider, provider_account, provider_message_id);

-- For the expiry sweep: only unconsumed rows can expire (a consumed one is spent
-- and kept for audit).
CREATE INDEX IF NOT EXISTS idx_inbound_evidence_unconsumed
    ON inbound_evidence(expires_at_ms) WHERE consumed_at_ms IS NULL;

-- Which challenge a given number is awaiting a reply for.
--
-- # Why correlation is by ADDRESS, not by code
--
-- The reply `YES 7312` carries only a four-digit code, and two live challenges
-- can draw the same one — so the code cannot ROUTE. The address can: a number is
-- the bound channel, and it awaits at most one challenge. `address` as PRIMARY
-- KEY makes that "at most one" a constraint, not a hope — a second challenge
-- raised to the same number supersedes the first via the upsert. The code is
-- then AUTHENTICATION of the answer inside `submit`, never SELECTION of the
-- challenge. This is the same "one active per address" shape as
-- `idx_channel_bindings_live_address` (0006).
CREATE TABLE IF NOT EXISTS awaiting_reply (
    address         TEXT PRIMARY KEY,
    challenge_id    TEXT NOT NULL,
    created_at_ms   INTEGER NOT NULL,
    expires_at_ms   INTEGER NOT NULL
);

CREATE INDEX IF NOT EXISTS idx_awaiting_reply_expiry
    ON awaiting_reply(expires_at_ms);

-- The inbound booking id a challenge was raised FOR, made queryable.
--
-- Approve-first (§23.1) removed ADR-024's incidental dedupe: a redelivered BOOK
-- used to hit `create` and return `Existing`, but now BOOK creates nothing, so a
-- redelivery would raise a SECOND challenge with a fresh code. `PendingScope`'s
-- booking id lives only inside the opaque `scope` BLOB and is unqueryable, so
-- idempotent begin needs it as a column. A PARTIAL unique index (WHERE NOT NULL)
-- lets pre-migration rows, which have no intent recorded, coexist — and makes a
-- redelivered BOOK for the same booking, at ANY lifecycle stage (pending,
-- approved, rejected, expired), resolve to the one existing challenge.
ALTER TABLE approval_challenges ADD COLUMN booking_intent TEXT;

CREATE UNIQUE INDEX IF NOT EXISTS idx_approval_challenges_booking
    ON approval_challenges(booking_intent) WHERE booking_intent IS NOT NULL;
