-- datumesh-store schema, migration 6 (additions only; a store release later): classes
-- (SPEC 16.1 to 16.3, 9.7).
--
-- classes: the canonical (JCS) flattened class descriptors, by class hash. The hash is the
-- SHA-256 of the stored bytes (checked here by sha256(), which PostgreSQL has), and the bytes
-- are JSON. Insert-only, as descriptors.
--
-- class_bindings: a class name from transaction time tx on means the class class_hash. The
-- counterpart of name_bindings, append-only and in tx order per class name, with the same
-- advisory lock (a class name and a function name are one namespace: the lock key is the
-- name's). Binding a class also binds each method N/m of its flattened descriptor, as rows of
-- name_bindings, in the same transaction (the store does that; a binding of a method name is
-- an ordinary function binding).
--
-- One namespace of names: a class name with any function binding is refused, and a function
-- name with any class binding is refused, by the triggers below (the store checks first and
-- answers with the typed error).

CREATE TABLE classes (
    class_hash text COLLATE "C" PRIMARY KEY CHECK (class_hash ~ '^[0-9a-f]{64}$'),
    -- Canonical (JCS) flattened class descriptor JSON; the hash is the SHA-256 of these bytes.
    bytes bytea NOT NULL
        CHECK (encode(sha256(bytes), 'hex') = class_hash),
    CHECK (convert_from(bytes, 'UTF8')::jsonb IS NOT NULL)
);

CREATE TABLE class_bindings (
    class_name text COLLATE "C" NOT NULL,
    tx text COLLATE "C" NOT NULL CHECK (tx ~ '^[0-9]{4}-[0-9]{2}-[0-9]{2}T[0-9]{2}:[0-9]{2}:[0-9]{2}\.[0-9]{6}Z$'),
    tx_at timestamptz NOT NULL CHECK (tx_at = datumesh_instant(tx)),
    class_hash text COLLATE "C" NOT NULL REFERENCES classes (class_hash),
    PRIMARY KEY (class_name, tx)
);

CREATE FUNCTION datumesh_classes_append_only() RETURNS trigger
    LANGUAGE plpgsql
    AS $$
BEGIN
    RAISE EXCEPTION 'datumesh: % on % refused: the table is append-only', TG_OP, TG_TABLE_NAME
        USING ERRCODE = 'restrict_violation';
END
$$;

CREATE FUNCTION datumesh_class_bindings_in_order() RETURNS trigger
    LANGUAGE plpgsql
    AS $$
DECLARE
    latest text COLLATE "C";
BEGIN
    IF current_setting('transaction_isolation') <> 'read committed' THEN
        RAISE EXCEPTION 'datumesh: class bindings are appended only under READ COMMITTED'
            USING ERRCODE = 'restrict_violation';
    END IF;
    PERFORM datumesh_lock('names', NEW.class_name);
    SELECT max(tx) INTO latest FROM class_bindings WHERE class_name = NEW.class_name;
    IF latest IS NOT NULL AND NEW.tx <= latest THEN
        RAISE EXCEPTION 'datumesh: class binding tx % of name % is not later than the latest, %', NEW.tx, NEW.class_name, latest
            USING ERRCODE = 'restrict_violation';
    END IF;
    RETURN NEW;
END
$$;

CREATE FUNCTION datumesh_class_bindings_one_namespace() RETURNS trigger
    LANGUAGE plpgsql
    AS $$
BEGIN
    IF EXISTS (SELECT 1 FROM name_bindings WHERE fn_name = NEW.class_name) THEN
        RAISE EXCEPTION 'datumesh: name % is bound as a function: class and function names are one namespace', NEW.class_name
            USING ERRCODE = 'restrict_violation';
    END IF;
    RETURN NEW;
END
$$;

CREATE FUNCTION datumesh_name_bindings_one_namespace() RETURNS trigger
    LANGUAGE plpgsql
    AS $$
BEGIN
    IF EXISTS (SELECT 1 FROM class_bindings WHERE class_name = NEW.fn_name) THEN
        RAISE EXCEPTION 'datumesh: name % is bound as a class: class and function names are one namespace', NEW.fn_name
            USING ERRCODE = 'restrict_violation';
    END IF;
    RETURN NEW;
END
$$;

CREATE TRIGGER classes_append_only BEFORE UPDATE OR DELETE ON classes
    FOR EACH ROW EXECUTE FUNCTION datumesh_classes_append_only();
CREATE TRIGGER classes_no_truncate BEFORE TRUNCATE ON classes
    FOR EACH STATEMENT EXECUTE FUNCTION datumesh_classes_append_only();

CREATE TRIGGER class_bindings_append_only BEFORE UPDATE OR DELETE ON class_bindings
    FOR EACH ROW EXECUTE FUNCTION datumesh_append_only();
CREATE TRIGGER class_bindings_no_truncate BEFORE TRUNCATE ON class_bindings
    FOR EACH STATEMENT EXECUTE FUNCTION datumesh_append_only();
-- Triggers of one table fire in name order: the lock and the tx order come before the namespace.
CREATE TRIGGER class_bindings_in_order BEFORE INSERT ON class_bindings
    FOR EACH ROW EXECUTE FUNCTION datumesh_class_bindings_in_order();
CREATE TRIGGER class_bindings_one_namespace BEFORE INSERT ON class_bindings
    FOR EACH ROW EXECUTE FUNCTION datumesh_class_bindings_one_namespace();

CREATE TRIGGER name_bindings_one_namespace BEFORE INSERT ON name_bindings
    FOR EACH ROW EXECUTE FUNCTION datumesh_name_bindings_one_namespace();
