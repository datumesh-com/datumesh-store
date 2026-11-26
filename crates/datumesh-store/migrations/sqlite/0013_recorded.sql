-- datumesh-store SQLite schema, migration 13 (0.6.5, CONTRACT-guard-patch G9 and G10; the
-- counterpart of PostgreSQL's migration 17). Additive; the signed tx stays as signed.
--
-- versions.recorded: the instant the store recorded the version at (see 0017_recorded.sql). The
-- column is nullable and has no CHECK: the store names a recorded time on every insert and checks it
-- (instant, recorded >= tx); a raw insert naming none leaves NULL, which means its tx (every read and
-- the order trigger use coalesce(recorded, tx), as PostgreSQL's default trigger fills it in).
-- Existing rows get recorded = tx: the update passes the no-update trigger, which is dropped and
-- created again around it.
DROP TRIGGER versions_no_update;
DROP TRIGGER versions_in_order;
ALTER TABLE versions ADD COLUMN recorded TEXT;
UPDATE versions SET recorded = tx;
CREATE TRIGGER versions_no_update BEFORE UPDATE ON versions
    BEGIN SELECT RAISE(ABORT, 'datumesh: UPDATE on versions refused: the table is append-only'); END;
-- In order by recorded time (G9).
CREATE TRIGGER versions_in_order BEFORE INSERT ON versions
    WHEN EXISTS (SELECT 1 FROM versions v WHERE v.cell_id = NEW.cell_id
                 AND coalesce(v.recorded, v.tx) >= coalesce(NEW.recorded, NEW.tx))
    BEGIN SELECT RAISE(ABORT, 'datumesh: version recorded time is not later than the latest of its cell'); END;
CREATE INDEX versions_recorded ON versions (cell_id, coalesce(recorded, tx));
-- The insertion sequence (decided 26 Nov, G3): existing rows numbered by rowid (insertion order); each
-- later insert takes max + 1 in its write transaction (the single writer), so it only increases.
-- A raw insert naming none leaves NULL (the store always names one).
ALTER TABLE versions ADD COLUMN seq INTEGER;
DROP TRIGGER versions_no_update;
UPDATE versions SET seq = rowid;
CREATE TRIGGER versions_no_update BEFORE UPDATE ON versions
    BEGIN SELECT RAISE(ABORT, 'datumesh: UPDATE on versions refused: the table is append-only'); END;
CREATE INDEX versions_cell_seq ON versions (cell_id, seq);
CREATE INDEX versions_seq ON versions (seq);

-- fn_seq (decided 26 Nov, G3): the highest seq among the versions of each function, one row per function,
-- kept by the trigger below (a derived table, upserted). Existing rows are summarised here.
CREATE TABLE function_seqs (
    fn_hash TEXT PRIMARY KEY CHECK (length(fn_hash) = 64 AND fn_hash NOT GLOB '*[^0-9a-f]*'),
    seq INTEGER NOT NULL
) STRICT;
INSERT INTO function_seqs (fn_hash, seq)
    SELECT c.fn_hash, max(v.seq) FROM versions v JOIN cells c ON c.cell_id = v.cell_id
     WHERE v.seq IS NOT NULL GROUP BY c.fn_hash;
CREATE TRIGGER versions_function_seq AFTER INSERT ON versions
    WHEN NEW.seq IS NOT NULL
    BEGIN
        INSERT INTO function_seqs (fn_hash, seq)
            SELECT c.fn_hash, NEW.seq FROM cells c WHERE c.cell_id = NEW.cell_id
        ON CONFLICT (fn_hash) DO UPDATE SET seq = max(function_seqs.seq, excluded.seq);
    END;
-- The versions recorded after an instant, of any cell (the slices' change check).
CREATE INDEX versions_recorded_all ON versions (coalesce(recorded, tx));

-- G10: a slice's definition, once per distinct slice. Append-only.
CREATE TABLE slice_definitions (
    slice_id TEXT PRIMARY KEY CHECK (length(slice_id) = 64 AND slice_id NOT GLOB '*[^0-9a-f]*'),
    fn_hash TEXT NOT NULL CHECK (length(fn_hash) = 64 AND fn_hash NOT GLOB '*[^0-9a-f]*'),
    slice BLOB NOT NULL
) STRICT;
CREATE TRIGGER slice_definitions_no_update BEFORE UPDATE ON slice_definitions
    BEGIN SELECT RAISE(ABORT, 'datumesh: UPDATE on slice_definitions refused: the table is append-only'); END;
CREATE TRIGGER slice_definitions_no_delete BEFORE DELETE ON slice_definitions
    BEGIN SELECT RAISE(ABORT, 'datumesh: DELETE on slice_definitions refused: the table is append-only'); END;
CREATE TRIGGER slice_definitions_no_replace BEFORE INSERT ON slice_definitions
    WHEN EXISTS (SELECT 1 FROM slice_definitions k WHERE k.slice_id = NEW.slice_id)
    BEGIN SELECT RAISE(ABORT, 'datumesh: INSERT on slice_definitions refused: the key exists and the table is append-only'); END;
