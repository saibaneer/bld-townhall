-- M8-2: per-principal / per-channel rate limits + the global provider budget
-- (ADR-028, spec §15.1 line 681). Additive; builds ON 0009 and never re-opens it.
--
-- # One table, three ceilings
--
-- A rate is not a quota: M8-1's quota bounds how many units an account may HOLD;
-- these bound how many units may be CONSUMED per time window. One windowed
-- counter serves all three ceilings — per principal, per channel, and the single
-- `global` provider budget — differing only by `counter_key`. The row
-- `(counter_key, window_start_ms)` IS the fixed window: a new window is a new
-- row, so a reset needs no sweep (the fixed-window pattern the denial log uses).
--
-- # The row is the guard
--
-- The check and the increment are one conditional upsert (ADR-028):
--   INSERT ... VALUES (key, window, units)
--   ON CONFLICT (counter_key, window_start_ms)
--   DO UPDATE SET used_units = used_units + units WHERE used_units + units <= max
-- `rows_affected() == 0` on the conflict path IS the over-limit signal, exactly
-- as the quota guard reads its conditional UPDATE. It runs inside the existing
-- reserve() transaction (which already holds SQLite's write lock), so two
-- concurrent turns cannot both slip past one ceiling, and a later guard failure
-- rolls the whole turn back — a rate token is spent only by a turn that takes
-- the hold.
CREATE TABLE IF NOT EXISTS usage_rate_counters (
    -- 'principal:<id>' | 'channel:<provider>|<account>' | 'global'
    counter_key     TEXT    NOT NULL,
    -- now_ms - (now_ms % window_ms): the window this count belongs to.
    window_start_ms INTEGER NOT NULL,
    -- Units consumed in this window under this key. Never negative; only ever
    -- incremented, and only up to the ceiling the upsert's WHERE enforces.
    used_units      INTEGER NOT NULL,
    PRIMARY KEY (counter_key, window_start_ms)
);
