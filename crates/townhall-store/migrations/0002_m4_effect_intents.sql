-- M4 slice A: durable effect identity.
--
-- The UNIQUE key is the load-bearing part. It is (booking_id, operation_kind,
-- source_version), not the effect id alone: without it a lost acknowledgement
-- lets a retry mint a SECOND intent for the same operation, and two external
-- effects follow. See ADR-014.
--
-- expires_at_ms is ADR-016: absence is only definitive once the council has
-- tombstoned an intent past this deadline, and the value is sent to the
-- council on both create and lookup.
CREATE TABLE IF NOT EXISTS effect_intents (
    effect_intent_id    TEXT PRIMARY KEY NOT NULL,
    booking_id          TEXT NOT NULL,
    operation_kind      TEXT NOT NULL,
    source_version      INTEGER NOT NULL CHECK (source_version >= 0),
    canonical_plan_json TEXT NOT NULL,
    status              TEXT NOT NULL,
    expires_at_ms       INTEGER NOT NULL,
    provider_reference  TEXT,
    last_error          TEXT,
    created_at_ms       INTEGER NOT NULL,
    updated_at_ms       INTEGER NOT NULL,
    UNIQUE (booking_id, operation_kind, source_version),
    FOREIGN KEY (booking_id) REFERENCES bookings(id)
);

CREATE INDEX IF NOT EXISTS idx_effect_intents_booking
    ON effect_intents(booking_id, status);
