-- datumesh-store SQLite schema, migration 3 (additions only; PostgreSQL's migration 6): classes
-- (SPEC 16.1 to 16.3, 9.7). See migrations/0006_classes.sql.
--
-- classes: the canonical (JCS) flattened class descriptors, by class hash. The hash is not
-- checked against the bytes here (SQLite has no SHA-256 function): the store computes it and
-- is the writer. The bytes are JSON.
--
-- class_bindings: a class name from tx on means the class class_hash; append-only and in tx
-- order per name, as name_bindings. A class name and a function name are one namespace: a
-- trigger on each table refuses the other kind (the store checks first and answers with the
-- typed error).

CREATE TABLE classes (
    class_hash TEXT PRIMARY KEY CHECK (length(class_hash) = 64 AND class_hash NOT GLOB '*[^0-9a-f]*'),
    -- Canonical (JCS) flattened class descriptor JSON.
    bytes BLOB NOT NULL CHECK (json_valid(CAST(bytes AS TEXT)))
) STRICT;

CREATE TABLE class_bindings (
    class_name TEXT NOT NULL,
    tx TEXT NOT NULL CHECK (tx GLOB '[0-9][0-9][0-9][0-9]-[0-9][0-9]-[0-9][0-9]T[0-9][0-9]:[0-9][0-9]:[0-9][0-9].[0-9][0-9][0-9][0-9][0-9][0-9]Z'),
    class_hash TEXT NOT NULL REFERENCES classes (class_hash),
    PRIMARY KEY (class_name, tx)
) STRICT;

-- Append-only: no UPDATE, no DELETE; no insert over an existing key.

CREATE TRIGGER classes_no_update BEFORE UPDATE ON classes
    BEGIN SELECT RAISE(ABORT, 'datumesh: UPDATE on classes refused: the table is append-only'); END;
CREATE TRIGGER classes_no_delete BEFORE DELETE ON classes
    BEGIN SELECT RAISE(ABORT, 'datumesh: DELETE on classes refused: the table is append-only'); END;
CREATE TRIGGER classes_no_replace BEFORE INSERT ON classes
    WHEN EXISTS (SELECT 1 FROM classes k WHERE k.class_hash = NEW.class_hash)
    BEGIN SELECT RAISE(ABORT, 'datumesh: INSERT on classes refused: the key exists and the table is append-only'); END;
CREATE TRIGGER class_bindings_no_update BEFORE UPDATE ON class_bindings
    BEGIN SELECT RAISE(ABORT, 'datumesh: UPDATE on class_bindings refused: the table is append-only'); END;
CREATE TRIGGER class_bindings_no_delete BEFORE DELETE ON class_bindings
    BEGIN SELECT RAISE(ABORT, 'datumesh: DELETE on class_bindings refused: the table is append-only'); END;
CREATE TRIGGER class_bindings_no_replace BEFORE INSERT ON class_bindings
    WHEN EXISTS (SELECT 1 FROM class_bindings k WHERE k.class_name = NEW.class_name AND k.tx = NEW.tx)
    BEGIN SELECT RAISE(ABORT, 'datumesh: INSERT on class_bindings refused: the key exists and the table is append-only'); END;

-- In order: a class binding later than every one of its name.
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

-- References, whether or not the connection enforces foreign keys.
CREATE TRIGGER class_bindings_class_hash_stored BEFORE INSERT ON class_bindings
    WHEN NOT EXISTS (SELECT 1 FROM classes r WHERE r.class_hash = NEW.class_hash)
    BEGIN SELECT RAISE(ABORT, 'datumesh: INSERT on class_bindings refused: no such class is stored'); END;
