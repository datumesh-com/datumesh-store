-- datumesh-store schema, migration 15 (additions only; a store release 0.6.4): the sort key of
-- each cell (CONTRACT-0.5.1 S1) and the class bindings' index by class hash (S3).
--
-- cells.sort_key: the cell's arguments in ascending name order, each as its type's name and
-- then its value, encoded so that the bytes compare as the slice order does (SPEC 5.6, 15.13;
-- the Rust twin is slices::cell_sort_key, and the two must agree): a number is a sign byte (1
-- negative, 2 zero, 3 positive), its exponent e (0.d1d2... x 10^e) plus 2^31 in four bytes,
-- its significant digits and a 0 byte, and a negative's bytes after the sign complemented;
-- any other type is its text and a 0 0 terminator (PostgreSQL text holds no 0 byte).
-- The cells-insert trigger (datumesh_receipts_index_cells, replaced here, still counting the new
-- cells per function as migration 12 has it) computes it for new cells; the rows already there
-- are filled below.

CREATE FUNCTION datumesh_sort_item(p_type text, p_value text) RETURNS bytea
    LANGUAGE plpgsql IMMUTABLE
    AS $$
DECLARE
    tag bytea := convert_to(p_type, 'UTF8') || '\x0000'::bytea;
    neg boolean := left(p_value, 1) = '-';
    m text;
    ip text;
    fp text;
    allv text;
    digits text;
    z integer;
    body bytea;
BEGIN
    IF p_type <> 'number' THEN
        RETURN tag || convert_to(p_value, 'UTF8') || '\x0000'::bytea;
    END IF;
    m := CASE WHEN neg THEN substr(p_value, 2) ELSE p_value END;
    ip := split_part(m, '.', 1);
    fp := split_part(m, '.', 2);
    allv := ip || fp;
    digits := rtrim(ltrim(allv, '0'), '0');
    IF digits = '' THEN
        RETURN tag || '\x02'::bytea;
    END IF;
    z := length(allv) - length(ltrim(allv, '0'));
    body := decode(lpad(to_hex(length(ip) - z + 2147483648::bigint), 8, '0'), 'hex')
            || convert_to(digits, 'UTF8') || '\x00'::bytea;
    IF neg THEN
        RETURN tag || '\x01'::bytea
               || decode(translate(encode(body, 'hex'), '0123456789abcdef', 'fedcba9876543210'), 'hex');
    END IF;
    RETURN tag || '\x03'::bytea || body;
END
$$;

-- The per-row helper of migration 3 (no trigger uses it now; the tests put it back and compare)
-- makes the same rows, with the key.
CREATE OR REPLACE FUNCTION datumesh_index_cell(p_cell_id text, p_bytes bytea) RETURNS void
    LANGUAGE plpgsql
    AS $$
DECLARE
    c json := (convert_from(p_bytes, 'UTF8')::json) -> 'cell';
BEGIN
    INSERT INTO cells (cell_id, fn_hash, ns, fn, nargs, sort_key)
        VALUES (p_cell_id, c ->> 'fn_hash', c ->> 'ns', c ->> 'fn',
                (SELECT count(*) FROM json_object_keys(c -> 'args')),
                (SELECT coalesce(string_agg(datumesh_sort_item(a.value ->> 'type', a.value ->> 'value'),
                                            ''::bytea ORDER BY a.key COLLATE "C"), ''::bytea)
                   FROM json_each(c -> 'args') AS a))
        ON CONFLICT DO NOTHING;
    IF FOUND THEN
        INSERT INTO cell_args (cell_id, name, type, value, num, fn_hash)
            SELECT p_cell_id, a.key, a.value ->> 'type', a.value ->> 'value',
                   CASE WHEN a.value ->> 'type' = 'number' THEN (a.value ->> 'value')::numeric END,
                   c ->> 'fn_hash'
              FROM json_each(c -> 'args') AS a;
    END IF;
END
$$;

CREATE OR REPLACE FUNCTION datumesh_receipts_index_cells() RETURNS trigger
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
        INSERT INTO cells (cell_id, fn_hash, ns, fn, nargs, sort_key)
            SELECT cell_id, c ->> 'fn_hash', c ->> 'ns', c ->> 'fn',
                   (SELECT count(*) FROM json_object_keys(c -> 'args')),
                   (SELECT coalesce(string_agg(datumesh_sort_item(a.value ->> 'type', a.value ->> 'value'),
                                               ''::bytea ORDER BY a.key COLLATE "C"), ''::bytea)
                      FROM json_each(c -> 'args') AS a)
              FROM d
            ON CONFLICT DO NOTHING
            RETURNING cell_id
    ),
    bump AS (
        INSERT INTO cell_counts (fn_hash, ns, fn, n)
            SELECT d.c ->> 'fn_hash', d.c ->> 'ns', d.c ->> 'fn', count(*)
              FROM d JOIN ins USING (cell_id)
             GROUP BY 1, 2, 3
        ON CONFLICT (fn_hash, ns, fn) DO UPDATE SET n = cell_counts.n + excluded.n
        RETURNING 1
    )
    INSERT INTO cell_args (cell_id, name, type, value, num, fn_hash)
        SELECT d.cell_id, a.key, a.value ->> 'type', a.value ->> 'value',
               CASE WHEN a.value ->> 'type' = 'number' THEN (a.value ->> 'value')::numeric END,
               d.c ->> 'fn_hash'
          FROM d JOIN ins USING (cell_id), json_each(d.c -> 'args') AS a;
    RETURN NULL;
END
$$;

-- The rows already there: the append-only trigger of cells is lifted for this migration only
-- (its transaction), and put back as it was.
ALTER TABLE cells ADD COLUMN sort_key bytea NOT NULL DEFAULT ''::bytea;
ALTER TABLE cells DISABLE TRIGGER cells_insert_only;
UPDATE cells c SET sort_key = (
    SELECT coalesce(string_agg(datumesh_sort_item(a.type, a.value), ''::bytea ORDER BY a.name COLLATE "C"), ''::bytea)
      FROM cell_args a WHERE a.cell_id = c.cell_id
);
ALTER TABLE cells ENABLE TRIGGER cells_insert_only;
ALTER TABLE cells ALTER COLUMN sort_key DROP DEFAULT;

-- S1: a page of a slice in key order, from the candidates of one function.
CREATE INDEX cells_sort_key ON cells (fn_hash, ns, fn, sort_key);

-- S3: the bindings of a class by its hash (describe walks the parent chain by it).
CREATE INDEX class_bindings_by_hash ON class_bindings (class_hash, class_name, tx);
