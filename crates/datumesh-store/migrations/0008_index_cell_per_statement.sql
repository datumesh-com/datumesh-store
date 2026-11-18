-- datumesh-store schema, migration 8 (additions only; a store release later): the cell index
-- runs once per statement instead of once per receipt (CONTRACT-perf S6). PostgreSQL only.
--
-- The per-row trigger of migration 3 called datumesh_index_cell for every inserted receipt:
-- one plpgsql call, and one INSERT ... ON CONFLICT on cells, per receipt. This statement
-- trigger reads the rows of the whole INSERT (its transition table) and makes the same rows:
-- one cells row per distinct new cell_id (ON CONFLICT DO NOTHING), and the cell_args rows
-- only for the cells that this statement inserted, from the same arguments with the same
-- type, lexical value, number (numeric only for type number) and function hash. Every stored
-- value is the one the per-row trigger stores. No backfill: the rows already there are indexed.
-- datumesh_index_cell() and datumesh_receipts_index_cell() stay in place, unused by triggers.

CREATE FUNCTION datumesh_receipts_index_cells() RETURNS trigger
    LANGUAGE plpgsql
    AS $$
BEGIN
    WITH n AS (
        SELECT r.cell_id, (convert_from(r.bytes, 'UTF8')::json) -> 'cell' AS c FROM new_rows r
    ),
    d AS (
        SELECT DISTINCT ON (cell_id) cell_id, c FROM n ORDER BY cell_id
    ),
    ins AS (
        INSERT INTO cells (cell_id, fn_hash, ns, fn, nargs)
            SELECT cell_id, c ->> 'fn_hash', c ->> 'ns', c ->> 'fn',
                   (SELECT count(*) FROM json_object_keys(c -> 'args'))
              FROM d
            ON CONFLICT DO NOTHING
            RETURNING cell_id
    )
    INSERT INTO cell_args (cell_id, name, type, value, num, fn_hash)
        SELECT d.cell_id, a.key, a.value ->> 'type', a.value ->> 'value',
               CASE WHEN a.value ->> 'type' = 'number' THEN (a.value ->> 'value')::numeric END,
               d.c ->> 'fn_hash'
          FROM d JOIN ins USING (cell_id), json_each(d.c -> 'args') AS a;
    RETURN NULL;
END
$$;

DROP TRIGGER receipts_index_cell ON receipts;

CREATE TRIGGER receipts_index_cells AFTER INSERT ON receipts
    REFERENCING NEW TABLE AS new_rows
    FOR EACH STATEMENT EXECUTE FUNCTION datumesh_receipts_index_cells();
