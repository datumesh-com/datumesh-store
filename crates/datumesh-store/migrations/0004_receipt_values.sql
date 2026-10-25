-- datumesh-store schema, migration 4 (0.3.2): additions only.
--
-- receipts.value_type, receipts.value: the receipt's typed value (its `type` and the lexical
-- form of its `value`; booleans "true" and "false"), so that `slice_values` reads the
-- members' values from a column instead of parsing every receipt. Backfilled here from the
-- stored canonical bytes; a row's columns must equal them (CHECK constraints); an insert that
-- does not name them (a 0.3.1 writer, and put_receipt) gets them from the bytes (a trigger).
--
-- receipts_fn_hash_asof: receipts by function and asof, for `slice_changed_after`: a stored
-- version of a cell answers differently after A only if its receipt's asof (never earlier
-- than its tx) is after A.

CREATE FUNCTION datumesh_receipt_value_type(bytes bytea) RETURNS text
    LANGUAGE sql IMMUTABLE STRICT PARALLEL SAFE
    AS $$ SELECT (convert_from(bytes, 'UTF8')::json) ->> 'type' $$;

CREATE FUNCTION datumesh_receipt_value(bytes bytea) RETURNS text
    LANGUAGE sql IMMUTABLE STRICT PARALLEL SAFE
    AS $$ SELECT (convert_from(bytes, 'UTF8')::json) ->> 'value' $$;

ALTER TABLE receipts ADD COLUMN value_type text COLLATE "C";
ALTER TABLE receipts ADD COLUMN value text COLLATE "C";
UPDATE receipts SET value_type = datumesh_receipt_value_type(bytes), value = datumesh_receipt_value(bytes);
ALTER TABLE receipts ALTER COLUMN value_type SET NOT NULL;
ALTER TABLE receipts ALTER COLUMN value SET NOT NULL;
ALTER TABLE receipts ADD CONSTRAINT receipts_value_type_is_the_receipts
    CHECK (value_type = datumesh_receipt_value_type(bytes));
ALTER TABLE receipts ADD CONSTRAINT receipts_value_is_the_receipts CHECK (value = datumesh_receipt_value(bytes));

CREATE FUNCTION datumesh_receipts_value() RETURNS trigger
    LANGUAGE plpgsql
    AS $$
BEGIN
    NEW.value_type := datumesh_receipt_value_type(NEW.bytes);
    NEW.value := datumesh_receipt_value(NEW.bytes);
    RETURN NEW;
END
$$;

CREATE TRIGGER receipts_value_from_bytes BEFORE INSERT ON receipts
    FOR EACH ROW WHEN (NEW.value_type IS NULL OR NEW.value IS NULL) EXECUTE FUNCTION datumesh_receipts_value();

CREATE INDEX receipts_fn_hash_asof ON receipts (fn_hash, asof);
