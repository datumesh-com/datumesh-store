-- datumesh-store schema, migration 10 (0.6.0, CONTRACT-round-0.7 R1 and R2): withdrawals and
-- closed names (the counterpart of PostgreSQL's migration 14).
--
-- Withdrawals (R1): a withdrawal receipt has no type and no value, so its receipts row has
-- value_type = value = '' (the empty text, which no valued receipt has: a type is a name). The
-- receipts CHECKs compare with json_extract, which is NULL for a withdrawal, so they hold for
-- it; nothing in the table changes. A version whose receipt has value_type = '' is withdrawn.
--
-- Closed names (R2): a binding row whose fn_hash (class_hash) is NULL closes its name from its
-- tx on. SQLite cannot drop a NOT NULL constraint in place, so the two binding tables are
-- rebuilt with the same columns, keys and checks, the hash nullable. Their triggers and index
-- are dropped and created again as they were (the append-only ones, the order, the namespace
-- and the reference checks); a row is copied as it is, written_at included.

DROP TRIGGER class_bindings_no_update;
DROP TRIGGER class_bindings_no_delete;
DROP TRIGGER class_bindings_no_replace;
DROP TRIGGER class_bindings_in_order;
DROP TRIGGER class_bindings_one_namespace;
DROP TRIGGER class_bindings_class_hash_stored;
DROP TRIGGER name_bindings_no_update;
DROP TRIGGER name_bindings_no_delete;
DROP TRIGGER name_bindings_in_order;
DROP TRIGGER name_bindings_fn_hash_stored;
DROP TRIGGER name_bindings_one_namespace;
DROP INDEX name_bindings_fn_hash;

CREATE TABLE name_bindings_new (
    fn_name TEXT NOT NULL,
    tx TEXT NOT NULL CHECK (tx GLOB '[0-9][0-9][0-9][0-9]-[0-9][0-9]-[0-9][0-9]T[0-9][0-9]:[0-9][0-9]:[0-9][0-9].[0-9][0-9][0-9][0-9][0-9][0-9]Z'),
    -- NULL: the name is closed from tx on (0.6.0).
    fn_hash TEXT CHECK (fn_hash IS NULL OR (length(fn_hash) = 64 AND fn_hash NOT GLOB '*[^0-9a-f]*'))
        REFERENCES descriptors (fn_hash),
    written_at TEXT,
    PRIMARY KEY (fn_name, tx)
) STRICT;
INSERT INTO name_bindings_new (fn_name, tx, fn_hash, written_at)
    SELECT fn_name, tx, fn_hash, written_at FROM name_bindings;
DROP TABLE name_bindings;
ALTER TABLE name_bindings_new RENAME TO name_bindings;

CREATE TABLE class_bindings_new (
    class_name TEXT NOT NULL,
    tx TEXT NOT NULL CHECK (tx GLOB '[0-9][0-9][0-9][0-9]-[0-9][0-9]-[0-9][0-9]T[0-9][0-9]:[0-9][0-9]:[0-9][0-9].[0-9][0-9][0-9][0-9][0-9][0-9]Z'),
    -- NULL: the class name is closed from tx on (0.6.0).
    class_hash TEXT CHECK (class_hash IS NULL OR (length(class_hash) = 64 AND class_hash NOT GLOB '*[^0-9a-f]*'))
        REFERENCES classes (class_hash),
    written_at TEXT,
    PRIMARY KEY (class_name, tx)
) STRICT;
INSERT INTO class_bindings_new (class_name, tx, class_hash, written_at)
    SELECT class_name, tx, class_hash, written_at FROM class_bindings;
DROP TABLE class_bindings;
ALTER TABLE class_bindings_new RENAME TO class_bindings;

CREATE INDEX name_bindings_fn_hash ON name_bindings (fn_hash, fn_name);

-- Append-only: no UPDATE, no DELETE; no insert over an existing key.
CREATE TRIGGER name_bindings_no_update BEFORE UPDATE ON name_bindings
    BEGIN SELECT RAISE(ABORT, 'datumesh: UPDATE on name_bindings refused: the table is append-only'); END;
CREATE TRIGGER name_bindings_no_delete BEFORE DELETE ON name_bindings
    BEGIN SELECT RAISE(ABORT, 'datumesh: DELETE on name_bindings refused: the table is append-only'); END;
CREATE TRIGGER class_bindings_no_update BEFORE UPDATE ON class_bindings
    BEGIN SELECT RAISE(ABORT, 'datumesh: UPDATE on class_bindings refused: the table is append-only'); END;
CREATE TRIGGER class_bindings_no_delete BEFORE DELETE ON class_bindings
    BEGIN SELECT RAISE(ABORT, 'datumesh: DELETE on class_bindings refused: the table is append-only'); END;
CREATE TRIGGER class_bindings_no_replace BEFORE INSERT ON class_bindings
    WHEN EXISTS (SELECT 1 FROM class_bindings k WHERE k.class_name = NEW.class_name AND k.tx = NEW.tx)
    BEGIN SELECT RAISE(ABORT, 'datumesh: INSERT on class_bindings refused: the key exists and the table is append-only'); END;

-- In order: a binding later than every one of its name.
CREATE TRIGGER name_bindings_in_order BEFORE INSERT ON name_bindings
    WHEN EXISTS (SELECT 1 FROM name_bindings b WHERE b.fn_name = NEW.fn_name AND b.tx >= NEW.tx)
    BEGIN SELECT RAISE(ABORT, 'datumesh: binding tx is not later than the latest of its name'); END;
CREATE TRIGGER class_bindings_in_order BEFORE INSERT ON class_bindings
    WHEN EXISTS (SELECT 1 FROM class_bindings b WHERE b.class_name = NEW.class_name AND b.tx >= NEW.tx)
    BEGIN SELECT RAISE(ABORT, 'datumesh: class binding tx is not later than the latest of its name'); END;

-- One namespace of names, both ways.
CREATE TRIGGER class_bindings_one_namespace BEFORE INSERT ON class_bindings
    WHEN EXISTS (SELECT 1 FROM name_bindings b WHERE b.fn_name = NEW.class_name)
    BEGIN SELECT RAISE(ABORT, 'datumesh: class name is bound as a function: class and function names are one namespace'); END;
CREATE TRIGGER name_bindings_one_namespace BEFORE INSERT ON name_bindings
    WHEN EXISTS (SELECT 1 FROM class_bindings b WHERE b.class_name = NEW.fn_name)
    BEGIN SELECT RAISE(ABORT, 'datumesh: function name is bound as a class: class and function names are one namespace'); END;

-- References, whether or not the connection enforces foreign keys (a NULL closes: no reference).
CREATE TRIGGER name_bindings_fn_hash_stored BEFORE INSERT ON name_bindings
    WHEN NEW.fn_hash IS NOT NULL AND NOT EXISTS (SELECT 1 FROM descriptors r WHERE r.fn_hash = NEW.fn_hash)
    BEGIN SELECT RAISE(ABORT, 'datumesh: INSERT on name_bindings refused: no such descriptor is stored'); END;
CREATE TRIGGER class_bindings_class_hash_stored BEFORE INSERT ON class_bindings
    WHEN NEW.class_hash IS NOT NULL AND NOT EXISTS (SELECT 1 FROM classes r WHERE r.class_hash = NEW.class_hash)
    BEGIN SELECT RAISE(ABORT, 'datumesh: INSERT on class_bindings refused: no such class is stored'); END;
