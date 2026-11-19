-- datumesh-store SQLite schema, migration 6 (additions only; PostgreSQL's migration 10; 0.5.1,
-- CONTRACT-build2-part3-calculus): function tests, the index of descriptor inputs, and the
-- index of the bindings by hash. See migrations/0010_function_tests.sql for what each is for.
-- The back-fill of descriptor_inputs reads the descriptors' JSON with the JSON functions of
-- SQLite; a descriptor with no inputs array indexes nothing.

CREATE TABLE function_tests (
    fn_hash TEXT NOT NULL CHECK (length(fn_hash) = 64 AND fn_hash NOT GLOB '*[^0-9a-f]*'),
    "case" TEXT NOT NULL CHECK (length("case") = 16 AND "case" NOT GLOB '*[^0-9a-f]*'),
    -- Canonical (JCS) test line.
    test_json BLOB NOT NULL,
    signed_by TEXT NOT NULL CHECK (length(CAST(signed_by AS BLOB)) BETWEEN 1 AND 256),
    last_run_at TEXT CHECK (last_run_at IS NULL OR last_run_at GLOB '[0-9][0-9][0-9][0-9]-[0-9][0-9]-[0-9][0-9]T[0-9][0-9]:[0-9][0-9]:[0-9][0-9].[0-9][0-9][0-9][0-9][0-9][0-9]Z'),
    last_result TEXT CHECK (last_result IN ('pass', 'fail', 'error')),
    last_detail TEXT,
    branches_total INTEGER NOT NULL DEFAULT 0 CHECK (branches_total >= 0),
    branches_covered INTEGER NOT NULL DEFAULT 0 CHECK (branches_covered >= 0 AND branches_covered <= branches_total),
    PRIMARY KEY (fn_hash, "case")
) STRICT;

CREATE TABLE descriptor_inputs (
    fn_hash TEXT NOT NULL REFERENCES descriptors (fn_hash),
    input_name TEXT NOT NULL CHECK (length(CAST(input_name AS BLOB)) BETWEEN 1 AND 512),
    PRIMARY KEY (fn_hash, input_name)
) STRICT;

CREATE INDEX descriptor_inputs_input_name ON descriptor_inputs (input_name, fn_hash);

CREATE INDEX name_bindings_fn_hash ON name_bindings (fn_hash, fn_name);

-- Back-fill: the inputs of the descriptors stored before this migration.
INSERT INTO descriptor_inputs (fn_hash, input_name)
SELECT d.fn_hash, j.value
  FROM descriptors d, json_each(CAST(d.bytes AS TEXT), '$.inputs') AS j
 WHERE j.type = 'text';
