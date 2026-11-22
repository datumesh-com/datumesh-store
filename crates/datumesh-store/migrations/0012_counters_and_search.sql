-- datumesh-store schema, migration 12 (additions only; a store release later; 0.5.3,
-- CONTRACT-round-0.7-P SP1, SP2, SP6).
--
-- cell_counts: the number of cells of (fn_hash, ns, fn), kept by the statement trigger that
-- inserts the cells rows (datumesh_receipts_index_cells, replaced below): a new cell adds 1,
-- a second version of a cell adds nothing. The count is read by primary key, never by a
-- count(*) over cells. The migration fills it once from cells.
--
-- function_test_counts: the tests of a function per (fn_hash, fn_name), '' for an unnamed
-- test: how many, how many passed, failed, errored or have not run, the latest run, and the
-- branch counts of the hash (kept here, on every row of the hash, not on the test rows). The
-- store keeps it in the transaction of each write to function_tests (put, record, name); the
-- migration fills it once from function_tests. A row is never deleted: a row that empties
-- keeps the hash's branch counts.
--
-- The trigram index on the lower-cased entry names serves the text search of a name.
-- datumesh_word_score, datumesh_entry_total and datumesh_value_total rank a text search in
-- SQL: the same scores as the store's Words (1.0, 0.8, 0.5 per word, in tens, see
-- catalogue.rs), so the search takes its LIMIT after the order.

CREATE TABLE cell_counts (
    fn_hash text COLLATE "C" NOT NULL CHECK (fn_hash ~ '^[0-9a-f]{64}$'),
    ns text COLLATE "C" NOT NULL,
    fn text COLLATE "C" NOT NULL,
    n bigint NOT NULL DEFAULT 0 CHECK (n >= 0),
    PRIMARY KEY (fn_hash, ns, fn)
);

INSERT INTO cell_counts (fn_hash, ns, fn, n)
    SELECT fn_hash, ns, fn, count(*) FROM cells GROUP BY fn_hash, ns, fn;

CREATE TABLE function_test_counts (
    fn_hash text COLLATE "C" NOT NULL CHECK (fn_hash ~ '^[0-9a-f]{64}$'),
    fn_name text COLLATE "C" NOT NULL DEFAULT '' CHECK (octet_length(fn_name) <= 512),
    n bigint NOT NULL DEFAULT 0 CHECK (n >= 0),
    pass bigint NOT NULL DEFAULT 0 CHECK (pass >= 0),
    fail bigint NOT NULL DEFAULT 0 CHECK (fail >= 0),
    error bigint NOT NULL DEFAULT 0 CHECK (error >= 0),
    not_run bigint NOT NULL DEFAULT 0 CHECK (not_run >= 0),
    -- The latest last_run_at of the row's tests (an instant), NULL when none has run.
    last_run_at text COLLATE "C" CHECK (last_run_at IS NULL OR last_run_at ~ '^[0-9]{4}-[0-9]{2}-[0-9]{2}T[0-9]{2}:[0-9]{2}:[0-9]{2}\.[0-9]{6}Z$'),
    branches_total integer NOT NULL DEFAULT 0 CHECK (branches_total >= 0),
    branches_covered integer NOT NULL DEFAULT 0 CHECK (branches_covered >= 0 AND branches_covered <= branches_total),
    PRIMARY KEY (fn_hash, fn_name)
);

INSERT INTO function_test_counts (fn_hash, fn_name, n, pass, fail, error, not_run, last_run_at)
    SELECT fn_hash, COALESCE(fn_name, ''), count(*),
           count(*) FILTER (WHERE last_result = 'pass'),
           count(*) FILTER (WHERE last_result = 'fail'),
           count(*) FILTER (WHERE last_result = 'error'),
           count(*) FILTER (WHERE last_result IS NULL),
           max(last_run_at)
      FROM function_tests
     GROUP BY fn_hash, COALESCE(fn_name, '');

UPDATE function_test_counts c
   SET branches_total = b.t, branches_covered = b.v
  FROM (SELECT fn_hash, max(branches_total) AS t, max(branches_covered) AS v
          FROM function_tests GROUP BY fn_hash) b
 WHERE c.fn_hash = b.fn_hash;

-- The failing tests of a function, by case (a SQL LIMIT over the function's rows).
CREATE INDEX function_tests_last_result ON function_tests (fn_hash, fn_name, last_result, "case");

-- The stored test cells whose function_hash argument is a hash (false_test_cells).
CREATE INDEX cell_args_name_value ON cell_args (name, value, cell_id);

-- The statement trigger of migration 8, which also counts the new cells per (fn_hash, ns, fn).
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
        INSERT INTO cells (cell_id, fn_hash, ns, fn, nargs)
            SELECT cell_id, c ->> 'fn_hash', c ->> 'ns', c ->> 'fn',
                   (SELECT count(*) FROM json_object_keys(c -> 'args'))
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

DO $$
BEGIN
    IF EXISTS (SELECT 1 FROM pg_available_extensions WHERE name = 'pg_trgm') THEN
        CREATE EXTENSION IF NOT EXISTS pg_trgm;
        CREATE INDEX catalog_entries_name_trgm ON catalog_entries USING gin (lower(name) gin_trgm_ops);
    END IF;
END
$$;

-- The score of one word (lower-cased) against one field (lower-cased in the function):
-- 10 when the whole field is the word, 8 when one of its words (split on anything but letters
-- and digits) is, 5 when it is inside the field, else 0. Tens, so that the sums are integers.
CREATE FUNCTION datumesh_word_score(field text, word text) RETURNS integer
    LANGUAGE sql IMMUTABLE
    AS $$
    SELECT CASE
        WHEN lower(field) = word THEN 10
        WHEN position(' ' || word || ' ' IN ' ' || regexp_replace(lower(field), '[^[:alnum:]]+', ' ', 'g') || ' ') > 0 THEN 8
        WHEN position(word IN lower(field)) > 0 THEN 5
        ELSE 0
    END
$$;

-- The total score of an entry's fields for the words, or NULL when a word is in none of them.
CREATE FUNCTION datumesh_entry_total(words text[], f_name text, f_label text, f_description text, f_args text)
    RETURNS integer
    LANGUAGE sql IMMUTABLE
    AS $$
    SELECT CASE WHEN count(*) FILTER (WHERE best = 0) = 0 THEN sum(best)::integer END
      FROM (SELECT GREATEST(datumesh_word_score(f_name, w), datumesh_word_score(f_label, w),
                            datumesh_word_score(f_description, w),
                            COALESCE((SELECT max(GREATEST(datumesh_word_score(a.key, w), datumesh_word_score(a.value, w)))
                                        FROM jsonb_each_text(f_args::jsonb) a), 0)) AS best
              FROM unnest(words) AS w) s
$$;

-- The total score of a dictionary value's fields for the words, or NULL.
CREATE FUNCTION datumesh_value_total(words text[], f_value text, f_label text, f_aliases text[])
    RETURNS integer
    LANGUAGE sql IMMUTABLE
    AS $$
    SELECT CASE WHEN count(*) FILTER (WHERE best = 0) = 0 THEN sum(best)::integer END
      FROM (SELECT GREATEST(datumesh_word_score(f_value, w), datumesh_word_score(f_label, w),
                            COALESCE((SELECT max(datumesh_word_score(x, w)) FROM unnest(f_aliases) AS x), 0)) AS best
              FROM unnest(words) AS w) s
$$;
