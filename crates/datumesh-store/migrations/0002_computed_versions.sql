-- datumesh-store schema, migration 2 (0.2.0): additions only.
--
-- receipts.asof: the receipt's asof (and asof_at, the same instant as timestamptz, like the
-- other instants), so that a stored version answers as of A only when its receipt's asof
-- is also <= A. Backfilled here from the stored canonical bytes: the top-level "asof"
-- member of the receipt JSON, parsed as json (not jsonb: no number is converted). A row's
-- asof must equal that member (a CHECK constraint); an insert that does not name the
-- column (a 0.1.x writer) gets it from the bytes (a trigger).
--
-- computed_versions: computed versions of cells, derived rather than facts, so they are
-- added in any order (historical backfill computes an older version after a newer one).
-- Keyed on (cell_id, tx, asof): with late-signed inputs two computed receipts of one cell
-- can share a tx and differ in asof. Insert-only (UPDATE, DELETE and TRUNCATE raise, as on
-- versions), but with no tx-order trigger.

-- The asof of a receipt's canonical JSON text.
CREATE FUNCTION datumesh_receipt_asof(bytes bytea) RETURNS text
    LANGUAGE sql IMMUTABLE STRICT PARALLEL SAFE
    AS $$ SELECT (convert_from(bytes, 'UTF8')::json) ->> 'asof' $$;

ALTER TABLE receipts ADD COLUMN asof text COLLATE "C";
ALTER TABLE receipts ADD COLUMN asof_at timestamptz;
UPDATE receipts SET (asof, asof_at) = (SELECT a, datumesh_instant(a) FROM (SELECT datumesh_receipt_asof(bytes) AS a) x);
ALTER TABLE receipts ALTER COLUMN asof SET NOT NULL;
ALTER TABLE receipts ALTER COLUMN asof_at SET NOT NULL;
ALTER TABLE receipts ADD CONSTRAINT receipts_asof_check
    CHECK (asof ~ '^[0-9]{4}-[0-9]{2}-[0-9]{2}T[0-9]{2}:[0-9]{2}:[0-9]{2}\.[0-9]{6}Z$');
ALTER TABLE receipts ADD CONSTRAINT receipts_asof_at_check CHECK (asof_at = datumesh_instant(asof));
ALTER TABLE receipts ADD CONSTRAINT receipts_asof_is_the_receipts CHECK (asof = datumesh_receipt_asof(bytes));

CREATE FUNCTION datumesh_receipts_asof() RETURNS trigger
    LANGUAGE plpgsql
    AS $$
BEGIN
    NEW.asof := datumesh_receipt_asof(NEW.bytes);
    NEW.asof_at := datumesh_instant(NEW.asof);
    RETURN NEW;
END
$$;

CREATE TRIGGER receipts_asof_from_bytes BEFORE INSERT ON receipts
    FOR EACH ROW WHEN (NEW.asof IS NULL) EXECUTE FUNCTION datumesh_receipts_asof();

CREATE TABLE computed_versions (
    cell_id text COLLATE "C" NOT NULL CHECK (cell_id ~ '^[0-9a-f]{64}$'),
    tx text COLLATE "C" NOT NULL CHECK (tx ~ '^[0-9]{4}-[0-9]{2}-[0-9]{2}T[0-9]{2}:[0-9]{2}:[0-9]{2}\.[0-9]{6}Z$'),
    tx_at timestamptz NOT NULL CHECK (tx_at = datumesh_instant(tx)),
    asof text COLLATE "C" NOT NULL CHECK (asof ~ '^[0-9]{4}-[0-9]{2}-[0-9]{2}T[0-9]{2}:[0-9]{2}:[0-9]{2}\.[0-9]{6}Z$'),
    asof_at timestamptz NOT NULL CHECK (asof_at = datumesh_instant(asof)),
    receipt_hash text COLLATE "C" NOT NULL REFERENCES receipts (hash),
    PRIMARY KEY (cell_id, tx, asof)
);
-- computed_asof: the greatest asof <= A of a cell, ties the greatest tx (a backward scan).
CREATE INDEX computed_versions_asof ON computed_versions (cell_id, asof, tx);

CREATE TRIGGER computed_versions_insert_only BEFORE UPDATE OR DELETE ON computed_versions
    FOR EACH ROW EXECUTE FUNCTION datumesh_append_only();
CREATE TRIGGER computed_versions_no_truncate BEFORE TRUNCATE ON computed_versions
    FOR EACH STATEMENT EXECUTE FUNCTION datumesh_append_only();
