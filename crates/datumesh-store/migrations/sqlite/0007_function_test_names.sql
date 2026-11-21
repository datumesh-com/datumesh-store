-- datumesh-store SQLite schema, migration 7 (additions only; PostgreSQL's migration 11; 0.5.2,
-- CONTRACT-0.4.9 S1 and S7): the function name of a test (NULL until named) and the keys
-- marked open. See migrations/0011_function_test_names.sql for what each is for.

ALTER TABLE function_tests ADD COLUMN fn_name TEXT
    CHECK (fn_name IS NULL OR length(CAST(fn_name AS BLOB)) BETWEEN 1 AND 512);

CREATE INDEX function_tests_fn_name ON function_tests (fn_hash, fn_name, "case");

CREATE TABLE open_keys (
    owner TEXT NOT NULL CHECK (length(CAST(owner AS BLOB)) BETWEEN 1 AND 512),
    key TEXT NOT NULL CHECK (length(CAST(key AS BLOB)) BETWEEN 1 AND 128),
    marked_at TEXT NOT NULL CHECK (marked_at GLOB '[0-9][0-9][0-9][0-9]-[0-9][0-9]-[0-9][0-9]T[0-9][0-9]:[0-9][0-9]:[0-9][0-9].[0-9][0-9][0-9][0-9][0-9][0-9]Z'),
    PRIMARY KEY (owner, key)
) STRICT;
