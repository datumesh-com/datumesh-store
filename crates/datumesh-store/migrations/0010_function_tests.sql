-- datumesh-store schema, migration 10 (additions only; a store release later; 0.5.1,
-- CONTRACT-build2-part3-calculus): function tests, the index of descriptor inputs, and the
-- index of the bindings by hash.
--
-- function_tests: a canonical test line of a function, one row per (fn_hash, case). Append
-- friendly: an insert of an existing (fn_hash, case) does nothing (the store writes with
-- ON CONFLICT DO NOTHING, so the first writer's signed_by and test_json stay); a run sets
-- last_run_at, last_result and last_detail of its row; set_function_branches replicates the
-- branch counts on every row of a function. Not hashed, not signed by the store: a record of
-- runs that no receipt reads. No trigger.
--
-- descriptor_inputs: one row per fully qualified name in a descriptor's `inputs` (its
-- canonical JSON array). put_descriptor writes them in the descriptor's transaction. The
-- back-fill below does the same for the descriptors stored before this migration, from
-- their JSON (a descriptor with no inputs array indexes nothing).
--
-- The index on name_bindings (fn_hash, fn_name) serves bound_names. Nothing else changes.

CREATE TABLE function_tests (
    fn_hash text COLLATE "C" NOT NULL CHECK (fn_hash ~ '^[0-9a-f]{64}$'),
    "case" text COLLATE "C" NOT NULL CHECK ("case" ~ '^[0-9a-f]{16}$'),
    -- Canonical (JCS) test line.
    test_json bytea NOT NULL,
    -- The host or key id that wrote the row (the first writer's).
    signed_by text COLLATE "C" NOT NULL CHECK (octet_length(signed_by) BETWEEN 1 AND 256),
    -- The last run: an instant, its result and its detail; all NULL until the test has run.
    last_run_at text COLLATE "C" CHECK (last_run_at ~ '^[0-9]{4}-[0-9]{2}-[0-9]{2}T[0-9]{2}:[0-9]{2}:[0-9]{2}\.[0-9]{6}Z$'),
    last_result text COLLATE "C" CHECK (last_result IN ('pass', 'fail', 'error')),
    last_detail text,
    -- The function's branches (the same on each row of the function); at most i32::MAX.
    branches_total integer NOT NULL DEFAULT 0 CHECK (branches_total >= 0),
    branches_covered integer NOT NULL DEFAULT 0 CHECK (branches_covered >= 0 AND branches_covered <= branches_total),
    PRIMARY KEY (fn_hash, "case")
);

CREATE TABLE descriptor_inputs (
    fn_hash text COLLATE "C" NOT NULL REFERENCES descriptors (fn_hash),
    input_name text COLLATE "C" NOT NULL CHECK (octet_length(input_name) BETWEEN 1 AND 512),
    PRIMARY KEY (fn_hash, input_name)
);

-- function_readers: the descriptors that read a name.
CREATE INDEX descriptor_inputs_input_name ON descriptor_inputs (input_name, fn_hash);

-- bound_names: the names bound to a hash.
CREATE INDEX name_bindings_fn_hash ON name_bindings (fn_hash, fn_name);

-- Back-fill: the inputs of the descriptors stored before this migration.
INSERT INTO descriptor_inputs (fn_hash, input_name)
SELECT d.fn_hash, i.name
  FROM descriptors d
  CROSS JOIN LATERAL jsonb_array_elements_text(convert_from(d.bytes, 'UTF8')::jsonb -> 'inputs') AS i(name);
