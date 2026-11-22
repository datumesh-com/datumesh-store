-- datumesh-store SQLite schema, migration 8 (additions only; PostgreSQL's migration 12; 0.5.3,
-- CONTRACT-round-0.7-P SP1 and SP2). See migrations/0012_counters_and_search.sql for what each
-- table is for. A cell's insert adds 1 to cell_counts by a trigger on cells (as PostgreSQL's
-- statement trigger does), so the stores before this migration write cells as they did. The
-- store keeps the function test counts in the transaction of each write: put, record and name
-- of a function test move them; the branch counts are per hash, on every row of the hash. Rows
-- are never deleted. The text search needs no table: its scores are computed by functions of
-- the connection (see sqlite.rs).

CREATE TABLE cell_counts (
    fn_hash TEXT NOT NULL CHECK (length(fn_hash) = 64 AND fn_hash NOT GLOB '*[^0-9a-f]*'),
    ns TEXT NOT NULL,
    fn TEXT NOT NULL,
    n INTEGER NOT NULL DEFAULT 0 CHECK (n >= 0),
    PRIMARY KEY (fn_hash, ns, fn)
) STRICT;

INSERT INTO cell_counts (fn_hash, ns, fn, n)
    SELECT fn_hash, ns, fn, count(*) FROM cells GROUP BY fn_hash, ns, fn;

-- Every new cell: the trigger is made here, so no store before this migration counts it.
CREATE TRIGGER cells_count_insert AFTER INSERT ON cells
BEGIN
    INSERT INTO cell_counts (fn_hash, ns, fn, n) VALUES (NEW.fn_hash, NEW.ns, NEW.fn, 1)
        ON CONFLICT (fn_hash, ns, fn) DO UPDATE SET n = n + 1;
END;

CREATE TABLE function_test_counts (
    fn_hash TEXT NOT NULL CHECK (length(fn_hash) = 64 AND fn_hash NOT GLOB '*[^0-9a-f]*'),
    fn_name TEXT NOT NULL DEFAULT '' CHECK (length(CAST(fn_name AS BLOB)) <= 512),
    n INTEGER NOT NULL DEFAULT 0 CHECK (n >= 0),
    pass INTEGER NOT NULL DEFAULT 0 CHECK (pass >= 0),
    fail INTEGER NOT NULL DEFAULT 0 CHECK (fail >= 0),
    error INTEGER NOT NULL DEFAULT 0 CHECK (error >= 0),
    not_run INTEGER NOT NULL DEFAULT 0 CHECK (not_run >= 0),
    last_run_at TEXT CHECK (last_run_at IS NULL OR last_run_at GLOB '[0-9][0-9][0-9][0-9]-[0-9][0-9]-[0-9][0-9]T[0-9][0-9]:[0-9][0-9]:[0-9][0-9].[0-9][0-9][0-9][0-9][0-9][0-9]Z'),
    branches_total INTEGER NOT NULL DEFAULT 0 CHECK (branches_total >= 0),
    branches_covered INTEGER NOT NULL DEFAULT 0 CHECK (branches_covered >= 0 AND branches_covered <= branches_total),
    PRIMARY KEY (fn_hash, fn_name)
) STRICT;

INSERT INTO function_test_counts (fn_hash, fn_name, n, pass, fail, error, not_run, last_run_at)
    SELECT fn_hash, COALESCE(fn_name, ''), count(*),
           total(last_result = 'pass'), total(last_result = 'fail'), total(last_result = 'error'),
           total(last_result IS NULL), max(last_run_at)
      FROM function_tests
     GROUP BY fn_hash, COALESCE(fn_name, '');

UPDATE function_test_counts
   SET branches_total = (SELECT max(t.branches_total) FROM function_tests t WHERE t.fn_hash = function_test_counts.fn_hash),
       branches_covered = (SELECT max(t.branches_covered) FROM function_tests t WHERE t.fn_hash = function_test_counts.fn_hash);

CREATE INDEX function_tests_last_result ON function_tests (fn_hash, fn_name, last_result, "case");
