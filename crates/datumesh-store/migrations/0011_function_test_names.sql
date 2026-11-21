-- datumesh-store schema, migration 11 (additions only; a store release later; 0.5.2,
-- CONTRACT-0.4.9 S1 and S7): the function name of a test, and the keys marked open.
--
-- function_tests.fn_name: the fully qualified name of the function a test was stored for, set
-- once (name_function_tests never changes a name already set). NULL until a test is named:
-- a store of 0.5.1 keeps its rows unnamed, and nothing else in them changes. The index serves
-- the tests of one name of a hash (function_test_summary, function_tests_page).
--
-- open_keys: a dictionary key (of an owner, a function or class name) whose values stop
-- growing: an open key takes no automatic values (CONTRACT-0.4.9 H7). marked_at is the instant
-- it was marked. Rows are only added; nothing deletes a key's values.

ALTER TABLE function_tests ADD COLUMN fn_name text COLLATE "C"
    CHECK (fn_name IS NULL OR octet_length(fn_name) BETWEEN 1 AND 512);

CREATE INDEX function_tests_fn_name ON function_tests (fn_hash, fn_name, "case");

CREATE TABLE open_keys (
    owner text COLLATE "C" NOT NULL CHECK (octet_length(owner) BETWEEN 1 AND 512),
    key text COLLATE "C" NOT NULL CHECK (octet_length(key) BETWEEN 1 AND 128),
    -- The instant the key was marked open (the form of every instant of the store).
    marked_at text COLLATE "C" NOT NULL CHECK (marked_at ~ '^[0-9]{4}-[0-9]{2}-[0-9]{2}T[0-9]{2}:[0-9]{2}:[0-9]{2}\.[0-9]{6}Z$'),
    PRIMARY KEY (owner, key)
);
