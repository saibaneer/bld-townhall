-- The council's own database. Separate from the BLD side's, because a provider
-- that shared our storage could not be crashed independently, and slice E's
-- recovery tests need exactly that.

-- The catalogue: the source of every fact the council owns.
--
-- The council reads fee, capacity, accessibility and availability from here and
-- never from a request. A request may *assert* a fee — that assertion is checked
-- against this table and refused on disagreement — but the value that lands in a
-- booking is always this one.
CREATE TABLE venue_slots (
    venue_id     TEXT    NOT NULL,
    slot_id      TEXT    NOT NULL,
    fee_pence    INTEGER NOT NULL,
    capacity     INTEGER NOT NULL,
    accessible   INTEGER NOT NULL CHECK (accessible IN (0, 1)),
    available    INTEGER NOT NULL CHECK (available  IN (0, 1)),

    -- Bumped by EVERY mutation of this row, without exception.
    --
    -- A grant binds to this number, so a field the bump rule misses is a field a
    -- stale grant can still vouch for. The case that motivates it: change only
    -- accessibility, leaving fee, capacity and availability untouched. A version
    -- that tracked "the fields a booking checks" would not move, the stale grant
    -- would verify, and an inaccessible room would be booked for someone who
    -- needs an accessible one.
    --
    -- Hence a trigger rather than discipline at call sites.
    row_version  INTEGER NOT NULL DEFAULT 1,

    PRIMARY KEY (venue_id, slot_id)
);

CREATE TRIGGER venue_slots_bump_row_version
AFTER UPDATE ON venue_slots
FOR EACH ROW WHEN NEW.row_version = OLD.row_version
BEGIN
    UPDATE venue_slots
       SET row_version = OLD.row_version + 1
     WHERE venue_id = NEW.venue_id AND slot_id = NEW.slot_id;
END;

-- One row per effect identity the council has ever heard of, in one of four
-- states. "Never heard of it" is the absence of a row, and is part of the machine:
-- Unseen -> Open, and Unseen -> terminal, are both real transitions.
CREATE TABLE effects (
    effect_intent_id  TEXT PRIMARY KEY,

    -- Bound on first sight alongside the deadline, and immutable after.
    --
    -- Explicit rather than parsed out of the identity: a council that inferred
    -- the kind from our id format would be coupled to it, and a booking identity
    -- and a cancellation identity are two different effects.
    operation_kind    TEXT NOT NULL CHECK (operation_kind IN ('Book', 'Cancel')),

    -- Immutable after first sight, because a caller who could shorten a deadline
    -- could force premature absence and cancel a booking that was about to
    -- succeed.
    expires_at_ms     INTEGER NOT NULL,

    -- Open      heard of, nothing settled. A resolve before expiry lands here.
    -- Created   a booking or cancellation exists. Discoverable forever.
    -- Absent    permanently closed, nothing was created. A durable tombstone,
    --           not a clock reading — which is what makes absence survive a clock
    --           that steps backwards.
    -- Rejected  authoritatively refused, with the reason kept. Distinct from
    --           Absent because a rejection whose response was lost must resolve
    --           as the same rejection, not as "nothing happened".
    state             TEXT NOT NULL CHECK (state IN ('Open', 'Created', 'Absent', 'Rejected')),

    booking_reference TEXT,
    reason            TEXT,
    first_seen_ms     INTEGER NOT NULL,
    settled_at_ms     INTEGER,

    -- Per-state nullability, exhaustively. A single CHECK on `state` alone would
    -- admit a tombstone with no reason and a creation carrying a rejection reason.
    CHECK (
         (state = 'Open'     AND booking_reference IS NULL     AND reason IS NULL     AND settled_at_ms IS NULL)
      OR (state = 'Created'  AND booking_reference IS NOT NULL AND reason IS NULL     AND settled_at_ms IS NOT NULL)
      OR (state = 'Absent'   AND booking_reference IS NULL     AND reason IS NULL     AND settled_at_ms IS NOT NULL)
      OR (state = 'Rejected' AND booking_reference IS NULL     AND reason IS NOT NULL AND settled_at_ms IS NOT NULL)
    )
);

CREATE TABLE bookings (
    booking_reference TEXT PRIMARY KEY,

    -- UNIQUE makes "two bookings for one identity" unrepresentable rather than
    -- merely avoided. Idempotency then does not rest on the handler remembering
    -- to check.
    created_by        TEXT NOT NULL UNIQUE REFERENCES effects(effect_intent_id),

    venue_id          TEXT    NOT NULL,
    slot_id           TEXT    NOT NULL,
    attendees         INTEGER NOT NULL,

    -- Read from venue_slots in the same statement that inserts this row, never
    -- taken from the request. A council that stored the asserted fee and read it
    -- back would be signing the caller's own claim.
    fee_pence         INTEGER NOT NULL,

    principal         TEXT    NOT NULL,
    cancelled_by      TEXT    UNIQUE REFERENCES effects(effect_intent_id),
    created_at_ms     INTEGER NOT NULL,

    FOREIGN KEY (venue_id, slot_id) REFERENCES venue_slots(venue_id, slot_id)
);

CREATE INDEX bookings_by_creator ON bookings(created_by);
CREATE INDEX bookings_by_canceller ON bookings(cancelled_by);
