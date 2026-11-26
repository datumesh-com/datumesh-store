-- datumesh-store schema, migration 17 (0.6.5, CONTRACT-guard-patch G9 and G10): the recorded time
-- of each version and the slice definitions kept once per slice. Additive; the signed tx stays as
-- signed and no receipt changes.
--
-- versions.recorded: the instant the store recorded the version at, set by the store at insert
-- (max of the receipt's signed tx and the store's recording clock; the signed tx when the store has
-- no clock). Every as-of selection uses it; the range of a version slice stays on tx. Existing rows
-- get recorded = tx (the update passes the append-only trigger, disabled for it alone).
ALTER TABLE versions ADD COLUMN recorded text COLLATE "C"
    CHECK (recorded ~ '^[0-9]{4}-[0-9]{2}-[0-9]{2}T[0-9]{2}:[0-9]{2}:[0-9]{2}\.[0-9]{6}Z$' AND recorded >= tx);
ALTER TABLE versions DISABLE TRIGGER versions_append_only;
UPDATE versions SET recorded = tx;
ALTER TABLE versions ENABLE TRIGGER versions_append_only;
ALTER TABLE versions ALTER COLUMN recorded SET NOT NULL;
CREATE INDEX versions_recorded ON versions (cell_id, recorded);
-- The versions recorded after an instant, of any cell (the slices' change check).
CREATE INDEX versions_recorded_all ON versions (recorded);

-- A raw insert that names no recorded time (the store always names one) records at its tx. The
-- trigger's name sorts before versions_in_order, so it runs first (BEFORE triggers run in name order).
CREATE FUNCTION datumesh_versions_default_recorded() RETURNS trigger
    LANGUAGE plpgsql
    AS $$
BEGIN
    IF NEW.recorded IS NULL THEN
        NEW.recorded := NEW.tx;
    END IF;
    RETURN NEW;
END
$$;
CREATE TRIGGER versions_default_recorded BEFORE INSERT ON versions
    FOR EACH ROW EXECUTE FUNCTION datumesh_versions_default_recorded();

-- In order by recorded time (G9): a version later recorded than every one of its cell's.
CREATE OR REPLACE FUNCTION datumesh_versions_in_order() RETURNS trigger
    LANGUAGE plpgsql
    AS $$
DECLARE
    latest text COLLATE "C";
BEGIN
    IF current_setting('transaction_isolation') <> 'read committed' THEN
        RAISE EXCEPTION 'datumesh: versions are appended only under READ COMMITTED'
            USING ERRCODE = 'restrict_violation';
    END IF;
    PERFORM datumesh_lock('versions', NEW.cell_id);
    SELECT max(recorded) INTO latest FROM versions WHERE cell_id = NEW.cell_id;
    IF latest IS NOT NULL AND NEW.recorded <= latest THEN
        RAISE EXCEPTION 'datumesh: version recorded % of cell % is not later than the latest, %', NEW.recorded, NEW.cell_id, latest
            USING ERRCODE = 'restrict_violation';
    END IF;
    RETURN NEW;
END
$$;

-- G10: a slice's definition, once per distinct slice (not per date). Append-only.
CREATE TABLE slice_definitions (
    slice_id text COLLATE "C" PRIMARY KEY CHECK (slice_id ~ '^[0-9a-f]{64}$'),
    fn_hash text COLLATE "C" NOT NULL CHECK (fn_hash ~ '^[0-9a-f]{64}$'),
    slice bytea NOT NULL
);
CREATE TRIGGER slice_definitions_insert_only BEFORE UPDATE OR DELETE ON slice_definitions
    FOR EACH ROW EXECUTE FUNCTION datumesh_append_only();
CREATE TRIGGER slice_definitions_no_truncate BEFORE TRUNCATE ON slice_definitions
    FOR EACH STATEMENT EXECUTE FUNCTION datumesh_append_only();
