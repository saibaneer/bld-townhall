CREATE TABLE IF NOT EXISTS bookings (
    id                  TEXT PRIMARY KEY NOT NULL,
    version             INTEGER NOT NULL CHECK (version >= 0),
    state_name          TEXT NOT NULL,
    state_json          TEXT NOT NULL,
    requirements_json   TEXT NOT NULL,
    selected_venue_json TEXT,
    availability_json   TEXT,
    booking_ref         TEXT,
    active_effect       TEXT,
    created_at_ms       INTEGER NOT NULL,
    updated_at_ms       INTEGER NOT NULL
);

CREATE TABLE IF NOT EXISTS audit_events (
    sequence            INTEGER PRIMARY KEY AUTOINCREMENT,
    event_id            TEXT NOT NULL UNIQUE,
    booking_id          TEXT NOT NULL,
    from_version        INTEGER NOT NULL CHECK (from_version >= 0),
    to_version          INTEGER NOT NULL CHECK (to_version > from_version),
    from_state          TEXT NOT NULL,
    to_state            TEXT NOT NULL,
    proposal            TEXT NOT NULL,
    outcome             TEXT NOT NULL,
    evidence_summary    TEXT,
    created_at_ms       INTEGER NOT NULL,
    FOREIGN KEY (booking_id) REFERENCES bookings(id)
);

CREATE INDEX IF NOT EXISTS idx_audit_events_booking_sequence
    ON audit_events(booking_id, sequence);
