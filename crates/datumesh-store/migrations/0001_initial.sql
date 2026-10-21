-- datumesh-store schema, migration 1.
--
-- Hashes, cell ids, names and instants are text with the "C" collation, so that comparisons
-- are byte comparisons: instants (fixed width, SPEC 3.3) then compare chronologically as
-- text, which is how every as-of query compares them. Each instant is also kept as
-- timestamptz (the *_at columns), which round-trips every instant of the specification's
-- range exactly; the text is authoritative and the timestamptz is for people and tools.
--
-- versions and name_bindings are append-only: UPDATE, DELETE and TRUNCATE raise, and an
-- insert must be later than every row already there for the same cell or name. The insert
-- check takes a per-cell (per-name) transaction-scoped advisory lock, so concurrent writers
-- are serialised; it requires READ COMMITTED, where the check after the lock sees every
-- committed row.

-- An instant as timestamptz. Immutable in effect: the text always ends in Z, so neither
-- TimeZone nor DateStyle changes the result.
CREATE FUNCTION datumesh_instant(t text) RETURNS timestamptz
    LANGUAGE sql IMMUTABLE STRICT PARALLEL SAFE
    AS $$ SELECT t::timestamptz $$;

-- Serialises writers of one cell's versions or one name's bindings until the end of the
-- transaction.
CREATE FUNCTION datumesh_lock(kind text, key text) RETURNS void
    LANGUAGE sql VOLATILE STRICT
    AS $$ SELECT pg_advisory_xact_lock(hashtextextended(kind || ':' || key, 0)) $$;

CREATE FUNCTION datumesh_append_only() RETURNS trigger
    LANGUAGE plpgsql
    AS $$
BEGIN
    RAISE EXCEPTION 'datumesh: % on % refused: the table is append-only', TG_OP, TG_TABLE_NAME
        USING ERRCODE = 'restrict_violation';
END
$$;

CREATE TABLE descriptors (
    fn_hash text COLLATE "C" PRIMARY KEY CHECK (fn_hash ~ '^[0-9a-f]{64}$'),
    -- Canonical (JCS) descriptor JSON.
    bytes bytea NOT NULL
);

CREATE TABLE receipts (
    hash text COLLATE "C" PRIMARY KEY CHECK (hash ~ '^[0-9a-f]{64}$'),
    -- Canonical (JCS) receipt JSON, sig included.
    bytes bytea NOT NULL,
    cell_id text COLLATE "C" NOT NULL CHECK (cell_id ~ '^[0-9a-f]{64}$'),
    tx text COLLATE "C" NOT NULL CHECK (tx ~ '^[0-9]{4}-[0-9]{2}-[0-9]{2}T[0-9]{2}:[0-9]{2}:[0-9]{2}\.[0-9]{6}Z$'),
    tx_at timestamptz NOT NULL CHECK (tx_at = datumesh_instant(tx)),
    -- No foreign key: a receipt may arrive before (or without) its function's descriptor.
    fn_hash text COLLATE "C" NOT NULL CHECK (fn_hash ~ '^[0-9a-f]{64}$'),
    publisher text COLLATE "C" NOT NULL
);
CREATE INDEX receipts_cell_tx ON receipts (cell_id, tx);
CREATE INDEX receipts_fn_hash ON receipts (fn_hash);

CREATE TABLE blobs (
    sha256 text COLLATE "C" PRIMARY KEY CHECK (sha256 ~ '^[0-9a-f]{64}$'),
    bytes bytea NOT NULL,
    length bigint NOT NULL CHECK (length = octet_length(bytes)),
    -- Unknown when stored through put_blob, which takes only the content.
    media_type text COLLATE "C"
);

CREATE TABLE versions (
    cell_id text COLLATE "C" NOT NULL CHECK (cell_id ~ '^[0-9a-f]{64}$'),
    tx text COLLATE "C" NOT NULL CHECK (tx ~ '^[0-9]{4}-[0-9]{2}-[0-9]{2}T[0-9]{2}:[0-9]{2}:[0-9]{2}\.[0-9]{6}Z$'),
    tx_at timestamptz NOT NULL CHECK (tx_at = datumesh_instant(tx)),
    receipt_hash text COLLATE "C" NOT NULL REFERENCES receipts (hash),
    PRIMARY KEY (cell_id, tx)
);
CREATE INDEX versions_receipt_hash ON versions (receipt_hash);

CREATE TABLE edges (
    receipt_hash text COLLATE "C" NOT NULL REFERENCES receipts (hash),
    -- No foreign key: an input may be a receipt this store does not hold.
    input_hash text COLLATE "C" NOT NULL CHECK (input_hash ~ '^[0-9a-f]{64}$'),
    kind text COLLATE "C" NOT NULL CHECK (kind IN ('receipt', 'slice')),
    PRIMARY KEY (receipt_hash, input_hash)
);
CREATE INDEX edges_input_hash ON edges (input_hash, receipt_hash);

CREATE TABLE namespaces (
    host text COLLATE "C" PRIMARY KEY,
    -- Canonical (JCS) key document, checked as SPEC 9.2 requires for this host.
    keydoc bytea NOT NULL,
    validated_at text COLLATE "C" NOT NULL CHECK (validated_at ~ '^[0-9]{4}-[0-9]{2}-[0-9]{2}T[0-9]{2}:[0-9]{2}:[0-9]{2}\.[0-9]{6}Z$'),
    validated_at_at timestamptz NOT NULL CHECK (validated_at_at = datumesh_instant(validated_at)),
    suspended_at text COLLATE "C" CHECK (suspended_at ~ '^[0-9]{4}-[0-9]{2}-[0-9]{2}T[0-9]{2}:[0-9]{2}:[0-9]{2}\.[0-9]{6}Z$'),
    suspended_at_at timestamptz CHECK (suspended_at_at IS NOT DISTINCT FROM datumesh_instant(suspended_at))
);

CREATE TABLE name_bindings (
    fn_name text COLLATE "C" NOT NULL,
    tx text COLLATE "C" NOT NULL CHECK (tx ~ '^[0-9]{4}-[0-9]{2}-[0-9]{2}T[0-9]{2}:[0-9]{2}:[0-9]{2}\.[0-9]{6}Z$'),
    tx_at timestamptz NOT NULL CHECK (tx_at = datumesh_instant(tx)),
    fn_hash text COLLATE "C" NOT NULL REFERENCES descriptors (fn_hash),
    PRIMARY KEY (fn_name, tx)
);

-- Append-only: inserts in tx order per cell / name, never an update, delete or truncate.

CREATE FUNCTION datumesh_versions_in_order() RETURNS trigger
    LANGUAGE plpgsql
    AS $$
DECLARE
    latest text COLLATE "C";
BEGIN
    IF current_setting('transaction_isolation') <> 'read committed' THEN
        RAISE EXCEPTION 'datumesh: versions are appended only under READ COMMITTED'
            USING ERRCODE = 'restrict_violation';
    END IF;
    PERFORM datumesh_lock('versions', NEW.cell_id);
    SELECT max(tx) INTO latest FROM versions WHERE cell_id = NEW.cell_id;
    IF latest IS NOT NULL AND NEW.tx <= latest THEN
        RAISE EXCEPTION 'datumesh: version tx % of cell % is not later than the latest, %', NEW.tx, NEW.cell_id, latest
            USING ERRCODE = 'restrict_violation';
    END IF;
    RETURN NEW;
END
$$;

CREATE FUNCTION datumesh_names_in_order() RETURNS trigger
    LANGUAGE plpgsql
    AS $$
DECLARE
    latest text COLLATE "C";
BEGIN
    IF current_setting('transaction_isolation') <> 'read committed' THEN
        RAISE EXCEPTION 'datumesh: name bindings are appended only under READ COMMITTED'
            USING ERRCODE = 'restrict_violation';
    END IF;
    PERFORM datumesh_lock('names', NEW.fn_name);
    SELECT max(tx) INTO latest FROM name_bindings WHERE fn_name = NEW.fn_name;
    IF latest IS NOT NULL AND NEW.tx <= latest THEN
        RAISE EXCEPTION 'datumesh: binding tx % of name % is not later than the latest, %', NEW.tx, NEW.fn_name, latest
            USING ERRCODE = 'restrict_violation';
    END IF;
    RETURN NEW;
END
$$;

CREATE TRIGGER versions_append_only BEFORE UPDATE OR DELETE ON versions
    FOR EACH ROW EXECUTE FUNCTION datumesh_append_only();
CREATE TRIGGER versions_no_truncate BEFORE TRUNCATE ON versions
    FOR EACH STATEMENT EXECUTE FUNCTION datumesh_append_only();
CREATE TRIGGER versions_in_order BEFORE INSERT ON versions
    FOR EACH ROW EXECUTE FUNCTION datumesh_versions_in_order();

CREATE TRIGGER name_bindings_append_only BEFORE UPDATE OR DELETE ON name_bindings
    FOR EACH ROW EXECUTE FUNCTION datumesh_append_only();
CREATE TRIGGER name_bindings_no_truncate BEFORE TRUNCATE ON name_bindings
    FOR EACH STATEMENT EXECUTE FUNCTION datumesh_append_only();
CREATE TRIGGER name_bindings_in_order BEFORE INSERT ON name_bindings
    FOR EACH ROW EXECUTE FUNCTION datumesh_names_in_order();
