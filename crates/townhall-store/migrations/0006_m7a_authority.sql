-- M7's three tables: who a channel belongs to, what was asked, what was granted.
--
-- Spec §9 names all three. ADR-025 settles what each one may and may not know.
--
-- # The rule that shapes every column here
--
-- The store persists the authority envelope as OPAQUE BYTES plus the handful of
-- columns revocation and expiry must index. It never interprets the envelope.
--
-- The alternative — a fully-typed delegation row this store decodes — was the
-- plan's original shape, and ADR-025 records why it is worse. The envelope
-- cannot implement `Deserialize` (ADR-017 point 4, kept by ADR-021: a grant that
-- can arrive as JSON can be minted by anything that can write JSON), so a typed
-- row would be a SECOND description of what a principal may do: a mirror of the
-- envelope, free to drift from it, and the no-serde assertion would keep
-- passing while the mirror quietly became the real minting path.
--
-- So the codec lives beside the issuer in `townhall-authority`, and the columns
-- below carry only what a WHERE clause needs.

-- Which channel belongs to whom, and how well we know it.
--
-- # Why the version column exists
--
-- A binding is what makes "the reply came from the number we texted" mean
-- anything, and bindings move: a number gets re-verified, reassigned, or
-- withdrawn. A challenge bound to a principal alone — or to a normalized phone
-- string alone — would still verify after the binding beneath it had changed.
-- That is this project's recurring defect (state outliving the moment it was
-- true), and the fix is the same one ADR-024 used for the dispatcher's session:
-- record the revision, and compare it.
CREATE TABLE IF NOT EXISTS channel_bindings (
    id              TEXT PRIMARY KEY,
    -- E.164, normalized by `townhall-channel` before it ever reaches here.
    address         TEXT NOT NULL,
    principal       TEXT NOT NULL,
    -- Incremented whenever the verification evidence or the status changes.
    version         INTEGER NOT NULL,
    -- 'active' | 'withdrawn'. A withdrawn binding is kept, not deleted: a
    -- challenge that names it must still be able to say why it was refused.
    status          TEXT NOT NULL,
    -- `AssuranceLevel::name()`. The issuer CAPS a grant at this value, so it is
    -- read on every approval rather than only recorded.
    assurance       TEXT NOT NULL,
    -- What the provider told us, as a short opaque summary. Never the message
    -- body, and never a digest of a low-entropy value (ADR-023: an unkeyed
    -- digest of a four-digit code is an encoding of it).
    evidence        TEXT,
    verified_at_ms  INTEGER,
    created_at_ms   INTEGER NOT NULL,
    updated_at_ms   INTEGER NOT NULL
);

-- One address may bind at most one ACTIVE principal at a time.
--
-- A partial index rather than a plain UNIQUE on `address`: withdrawn rows are
-- history and must be allowed to accumulate, while two live bindings for one
-- number would make "which principal texted?" ambiguous — and the ambiguity
-- would resolve differently depending on row order.
CREATE UNIQUE INDEX IF NOT EXISTS idx_channel_bindings_live_address
    ON channel_bindings(address) WHERE status = 'active';

CREATE INDEX IF NOT EXISTS idx_channel_bindings_principal
    ON channel_bindings(principal);

-- One approval request: what was asked, of whom, and how it ended.
CREATE TABLE IF NOT EXISTS approval_challenges (
    id                  TEXT PRIMARY KEY,
    -- The one-time code.
    --
    -- Stored as issued, not hashed. That is a deliberate and narrow decision:
    -- ADR-023 established that an unkeyed digest of a low-entropy value is an
    -- encoding of it, and a four-digit code has ten thousand candidates, so a
    -- hash column would buy the APPEARANCE of protection and no protection.
    -- What actually bounds the risk is `attempts_used` against MAX_ATTEMPTS and
    -- the reply deadline; what protects the column is that it is inside the
    -- database. A keyed MAC would be real protection and needs a key to live
    -- somewhere, which is M7B's composition root, not this migration's.
    code                TEXT NOT NULL,
    -- The canonical scope as DATA, in the issuer's own length-prefixed
    -- encoding. Data rather than only a digest because approval must resume
    -- after a restart, and spec §2 forbids resuming from conversational memory.
    scope               BLOB NOT NULL,
    -- The digest of `scope`, denormalized so a tamper check never re-derives
    -- the thing it is checking.
    scope_hash          TEXT NOT NULL,
    -- Which binding, at which revision, may answer.
    --
    -- The principal rather than the binding row's id, because `BindingRef` is
    -- (principal, version) — the thing the verifier compares. Not a foreign
    -- key: the challenge must outlive a binding's withdrawal in order to
    -- explain its own refusal.
    binding_principal   TEXT NOT NULL,
    binding_version     INTEGER NOT NULL,
    -- On whose behalf a grant would be issued, and who it would be attributed
    -- to. Two columns because ADR-025 separates them and ADR-020 requires it.
    grantor             TEXT NOT NULL,
    subject             TEXT NOT NULL,
    -- The level this challenge was raised at. The issued grant is capped at
    -- MIN(this, the binding's).
    assurance           TEXT NOT NULL,
    -- 'pending' | 'approved' | 'rejected' | 'exhausted'.
    --
    -- Rejection is a STATUS, not a deletion: `NO 7312` must be terminal, and a
    -- deleted row would make a later `YES` look like an unknown challenge —
    -- the same denial for a different reason, and the distinction is what the
    -- audit needs.
    status              TEXT NOT NULL,
    attempts_used       INTEGER NOT NULL DEFAULT 0,
    created_at_ms       INTEGER NOT NULL,
    -- The reply deadline. Denormalized from `scope` for the expiry sweep only;
    -- the VERIFIER reads the scope's copy, so the two can never disagree about
    -- what a person approved.
    expires_at_ms       INTEGER NOT NULL,
    settled_at_ms       INTEGER
);

CREATE INDEX IF NOT EXISTS idx_approval_challenges_pending
    ON approval_challenges(expires_at_ms) WHERE status = 'pending';

CREATE INDEX IF NOT EXISTS idx_approval_challenges_grantor
    ON approval_challenges(grantor);

-- One verified authority grant.
CREATE TABLE IF NOT EXISTS delegations (
    id              TEXT PRIMARY KEY,
    -- Indexed: "everything granted over Lucy's bookings".
    grantor         TEXT NOT NULL,
    -- Indexed: "everything Marco holds".
    subject         TEXT NOT NULL,
    service         TEXT NOT NULL,
    -- The challenge this came from. At most one row may name any given
    -- challenge, which is spec §17's "one challenge -> at most one delegation"
    -- as a constraint rather than as a promise the application keeps.
    challenge_id    TEXT NOT NULL UNIQUE,
    expires_at_ms   INTEGER NOT NULL,
    -- Set once, never cleared.
    revoked_at_ms   INTEGER,
    -- The issuer's encoding. This store does not read it.
    envelope        BLOB NOT NULL,
    created_at_ms   INTEGER NOT NULL
);

CREATE INDEX IF NOT EXISTS idx_delegations_subject_live
    ON delegations(subject, expires_at_ms) WHERE revoked_at_ms IS NULL;

CREATE INDEX IF NOT EXISTS idx_delegations_grantor
    ON delegations(grantor);
