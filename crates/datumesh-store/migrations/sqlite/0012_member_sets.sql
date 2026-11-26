-- datumesh-store SQLite schema, migration 12 (0.6.5, CONTRACT-guard-patch G2): one small row per
-- member set (the counterpart of PostgreSQL's migration 16). See 0016_member_sets.sql.
-- Append-only by trigger: UPDATE, DELETE and a replacing insert raise. A member set is written
-- by insert-or-nothing (the store's INSERT ... WHERE NOT EXISTS, so no row is replaced).
CREATE TABLE member_sets (
    entry TEXT PRIMARY KEY CHECK (entry = 'slice:' || slice_id || ':' || members_hash),
    slice_id TEXT NOT NULL CHECK (length(slice_id) = 64 AND slice_id NOT GLOB '*[^0-9a-f]*'),
    members_hash TEXT NOT NULL CHECK (length(members_hash) = 64 AND members_hash NOT GLOB '*[^0-9a-f]*'),
    fn_hash TEXT NOT NULL CHECK (length(fn_hash) = 64 AND fn_hash NOT GLOB '*[^0-9a-f]*'),
    slice BLOB NOT NULL,
    members INTEGER NOT NULL CHECK (members BETWEEN 0 AND 1048576),
    tx TEXT CHECK (tx IS NULL OR tx GLOB '[0-9][0-9][0-9][0-9]-[0-9][0-9]-[0-9][0-9]T[0-9][0-9]:[0-9][0-9]:[0-9][0-9].[0-9][0-9][0-9][0-9][0-9][0-9]Z'),
    asof TEXT NOT NULL CHECK (asof GLOB '[0-9][0-9][0-9][0-9]-[0-9][0-9]-[0-9][0-9]T[0-9][0-9]:[0-9][0-9]:[0-9][0-9].[0-9][0-9][0-9][0-9][0-9][0-9]Z'),
    -- The members' distinct licence URLs, sorted, as JCS text (decided 26 Nov, G3).
    licences TEXT NOT NULL CHECK (substr(licences, 1, 1) = '['),
    -- The slice's seq read before its members (decided 26 Nov, G3); 0 when not set.
    seen_seq INTEGER NOT NULL DEFAULT 0 CHECK (seen_seq >= 0)
) STRICT;

CREATE TRIGGER member_sets_no_update BEFORE UPDATE ON member_sets
    BEGIN SELECT RAISE(ABORT, 'datumesh: UPDATE on member_sets refused: the table is append-only'); END;
CREATE TRIGGER member_sets_no_delete BEFORE DELETE ON member_sets
    BEGIN SELECT RAISE(ABORT, 'datumesh: DELETE on member_sets refused: the table is append-only'); END;
CREATE TRIGGER member_sets_no_replace BEFORE INSERT ON member_sets
    WHEN EXISTS (SELECT 1 FROM member_sets k WHERE k.entry = NEW.entry)
    BEGIN SELECT RAISE(ABORT, 'datumesh: INSERT on member_sets refused: the key exists and the table is append-only'); END;
