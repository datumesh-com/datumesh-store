-- datumesh-store schema, migration 16 (0.6.5, CONTRACT-guard-patch G2): one small row per member
-- set. Additions only; append-only by trigger, as the slice tables are (see 0003_slices.sql).
--
-- member_sets: what a slice read saw, without its members. The entry is held as the manifest's
-- (checked in the database as 'slice:' || slice_id || ':' || members_hash), so a manifest and a
-- member set of one entry coexist: under the store policy both are written (G2), under the
-- member-set-only policy only this row. slice is the slice object's canonical bytes, which the
-- members are rebuilt from (the entry holds only its hash). tx is the latest member or withdrawal
-- tx the read saw (NULL: none, an empty slice), asof the read's as-of. Written once per entry:
-- a later read of the same member set changes nothing.
CREATE TABLE member_sets (
    entry text COLLATE "C" PRIMARY KEY CHECK (entry = 'slice:' || slice_id || ':' || members_hash),
    slice_id text COLLATE "C" NOT NULL CHECK (slice_id ~ '^[0-9a-f]{64}$'),
    members_hash text COLLATE "C" NOT NULL CHECK (members_hash ~ '^[0-9a-f]{64}$'),
    fn_hash text COLLATE "C" NOT NULL CHECK (fn_hash ~ '^[0-9a-f]{64}$'),
    slice bytea NOT NULL,
    members integer NOT NULL CHECK (members BETWEEN 0 AND 1048576),
    tx text COLLATE "C" CHECK (tx IS NULL OR tx ~ '^[0-9]{4}-[0-9]{2}-[0-9]{2}T[0-9]{2}:[0-9]{2}:[0-9]{2}\.[0-9]{6}Z$'),
    asof text COLLATE "C" NOT NULL CHECK (asof ~ '^[0-9]{4}-[0-9]{2}-[0-9]{2}T[0-9]{2}:[0-9]{2}:[0-9]{2}\.[0-9]{6}Z$')
);

CREATE TRIGGER member_sets_insert_only BEFORE UPDATE OR DELETE ON member_sets
    FOR EACH ROW EXECUTE FUNCTION datumesh_append_only();
CREATE TRIGGER member_sets_no_truncate BEFORE TRUNCATE ON member_sets
    FOR EACH STATEMENT EXECUTE FUNCTION datumesh_append_only();
