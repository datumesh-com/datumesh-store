-- datumesh-store schema, migration 3 (0.3.1): additions only.
--
-- Slices (SPEC 4.4, 15.13): sets of cells of one function with arguments open or ranged,
-- and slice-as-lineage, where a computed receipt cites a slice it read as one `inputs`
-- entry `slice:<slice id>:<members hash>` and a manifest lists the slice's members.
--
-- cells, cell_args: the typed-argument index. One row per cell any stored receipt is of
-- (its function hash, ns, fn and argument count) and one per argument (its type and lexical
-- form, a number also as numeric, which compares exactly as a decimal), so that the members
-- of a slice are found with a range scan of an argument of its function, in the argument
-- type's order: numbers numerically, dates and instants chronologically and strings by code
-- point (text under "C", fixed-width dates and instants). Derived from the stored canonical
-- receipt bytes by a trigger on receipts, so every writer's receipts are indexed (a 0.3.0
-- writer too), and backfilled here the same way.
--
-- manifests, manifest_members: manifests by entry, in canonical form, and which manifests
-- each member receipt hash belongs to. slice_edges: the slice entries of each stored
-- receipt's inputs (the receipt-hash edges stay in edges). Neither needs the other first,
-- nor the members stored: a receipt cites a manifest by hash, as it cites inputs.
--
-- All five tables are insert-only: UPDATE, DELETE and TRUNCATE raise, as on versions.

CREATE TABLE cells (
    cell_id text COLLATE "C" PRIMARY KEY CHECK (cell_id ~ '^[0-9a-f]{64}$'),
    fn_hash text COLLATE "C" NOT NULL CHECK (fn_hash ~ '^[0-9a-f]{64}$'),
    ns text COLLATE "C" NOT NULL,
    fn text COLLATE "C" NOT NULL,
    nargs integer NOT NULL CHECK (nargs BETWEEN 0 AND 32)
);
-- An unbounded slice: every cell of a function.
CREATE INDEX cells_fn_hash ON cells (fn_hash, cell_id);

CREATE TABLE cell_args (
    cell_id text COLLATE "C" NOT NULL REFERENCES cells (cell_id),
    name text COLLATE "C" NOT NULL,
    type text COLLATE "C" NOT NULL CHECK (type IN ('number', 'string', 'boolean', 'date', 'instant', 'blob')),
    -- The lexical form (SPEC 3); booleans "true" and "false".
    value text COLLATE "C" NOT NULL,
    -- A number's value, exact; NULL for every other type.
    num numeric CHECK (CASE WHEN type = 'number' THEN num IS NOT NULL AND num = value::numeric ELSE num IS NULL END),
    -- The cell's function hash, for the indexes below.
    fn_hash text COLLATE "C" NOT NULL,
    PRIMARY KEY (cell_id, name)
);
-- A bound or ranged argument of a slice: dates, instants, strings, booleans and blobs by
-- their text, numbers by their value.
CREATE INDEX cell_args_value ON cell_args (fn_hash, name, type, value, cell_id);
CREATE INDEX cell_args_num ON cell_args (fn_hash, name, num, cell_id) WHERE type = 'number';

-- Indexes the cell of a stored receipt (canonical JSON bytes), once per cell.
CREATE FUNCTION datumesh_index_cell(p_cell_id text, p_bytes bytea) RETURNS void
    LANGUAGE plpgsql
    AS $$
DECLARE
    c json := (convert_from(p_bytes, 'UTF8')::json) -> 'cell';
BEGIN
    INSERT INTO cells (cell_id, fn_hash, ns, fn, nargs)
        VALUES (p_cell_id, c ->> 'fn_hash', c ->> 'ns', c ->> 'fn', (SELECT count(*) FROM json_object_keys(c -> 'args')))
        ON CONFLICT DO NOTHING;
    IF FOUND THEN
        INSERT INTO cell_args (cell_id, name, type, value, num, fn_hash)
            SELECT p_cell_id, a.key, a.value ->> 'type', a.value ->> 'value',
                   CASE WHEN a.value ->> 'type' = 'number' THEN (a.value ->> 'value')::numeric END,
                   c ->> 'fn_hash'
              FROM json_each(c -> 'args') AS a;
    END IF;
END
$$;

CREATE FUNCTION datumesh_receipts_index_cell() RETURNS trigger
    LANGUAGE plpgsql
    AS $$
BEGIN
    PERFORM datumesh_index_cell(NEW.cell_id, NEW.bytes);
    RETURN NULL;
END
$$;

CREATE TRIGGER receipts_index_cell AFTER INSERT ON receipts
    FOR EACH ROW EXECUTE FUNCTION datumesh_receipts_index_cell();

-- The backfill: every receipt stored so far.
SELECT datumesh_index_cell(cell_id, bytes) FROM receipts ORDER BY cell_id, hash;

CREATE TABLE manifests (
    entry text COLLATE "C" PRIMARY KEY CHECK (entry = 'slice:' || slice_id || ':' || members_hash),
    slice_id text COLLATE "C" NOT NULL CHECK (slice_id ~ '^[0-9a-f]{64}$'),
    members_hash text COLLATE "C" NOT NULL CHECK (members_hash ~ '^[0-9a-f]{64}$'),
    -- The slice's function hash (of the version slice's cell for a version slice).
    fn_hash text COLLATE "C" NOT NULL CHECK (fn_hash ~ '^[0-9a-f]{64}$'),
    members integer NOT NULL CHECK (members BETWEEN 0 AND 1048576),
    -- Canonical (JCS) manifest JSON.
    bytes bytea NOT NULL
);

CREATE TABLE manifest_members (
    entry text COLLATE "C" NOT NULL REFERENCES manifests (entry),
    -- No foreign key: a member may be a receipt this store does not hold.
    member_hash text COLLATE "C" NOT NULL CHECK (member_hash ~ '^[0-9a-f]{64}$'),
    PRIMARY KEY (entry, member_hash)
);
-- The manifests a member belongs to.
CREATE INDEX manifest_members_member ON manifest_members (member_hash, entry);

CREATE TABLE slice_edges (
    receipt_hash text COLLATE "C" NOT NULL REFERENCES receipts (hash),
    -- No foreign key: the manifest may not be stored (yet).
    entry text COLLATE "C" NOT NULL CHECK (entry ~ '^slice:[0-9a-f]{64}:[0-9a-f]{64}$'),
    PRIMARY KEY (receipt_hash, entry)
);
-- The receipts citing an entry.
CREATE INDEX slice_edges_entry ON slice_edges (entry, receipt_hash);

CREATE TRIGGER cells_insert_only BEFORE UPDATE OR DELETE ON cells
    FOR EACH ROW EXECUTE FUNCTION datumesh_append_only();
CREATE TRIGGER cells_no_truncate BEFORE TRUNCATE ON cells
    FOR EACH STATEMENT EXECUTE FUNCTION datumesh_append_only();
CREATE TRIGGER cell_args_insert_only BEFORE UPDATE OR DELETE ON cell_args
    FOR EACH ROW EXECUTE FUNCTION datumesh_append_only();
CREATE TRIGGER cell_args_no_truncate BEFORE TRUNCATE ON cell_args
    FOR EACH STATEMENT EXECUTE FUNCTION datumesh_append_only();
CREATE TRIGGER manifests_insert_only BEFORE UPDATE OR DELETE ON manifests
    FOR EACH ROW EXECUTE FUNCTION datumesh_append_only();
CREATE TRIGGER manifests_no_truncate BEFORE TRUNCATE ON manifests
    FOR EACH STATEMENT EXECUTE FUNCTION datumesh_append_only();
CREATE TRIGGER manifest_members_insert_only BEFORE UPDATE OR DELETE ON manifest_members
    FOR EACH ROW EXECUTE FUNCTION datumesh_append_only();
CREATE TRIGGER manifest_members_no_truncate BEFORE TRUNCATE ON manifest_members
    FOR EACH STATEMENT EXECUTE FUNCTION datumesh_append_only();
CREATE TRIGGER slice_edges_insert_only BEFORE UPDATE OR DELETE ON slice_edges
    FOR EACH ROW EXECUTE FUNCTION datumesh_append_only();
CREATE TRIGGER slice_edges_no_truncate BEFORE TRUNCATE ON slice_edges
    FOR EACH STATEMENT EXECUTE FUNCTION datumesh_append_only();
