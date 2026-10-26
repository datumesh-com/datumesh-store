-- datumesh-store SQLite schema, migration 1 (0.4.0): the schema of the PostgreSQL migrations
-- 1-4 in one, for a store in one file. Differences from PostgreSQL, each for a reason:
--
-- * Types: STRICT tables, TEXT for hashes, ids, names and instants (compared as bytes:
--   SQLite's default BINARY collation is PostgreSQL's "C"), BLOB for bytes. There are no
--   timestamptz columns (the *_at columns): the text is authoritative there too, and SQLite
--   has no such type.
-- * Checks: the hex, instant and entry forms are GLOB patterns instead of regular
--   expressions (SQLite has no REGEXP by default); the same strings pass.
-- * cell_args.sort_key instead of cell_args.num (numeric): SQLite has no exact decimal type,
--   so a number's index key is a text encoding that sorts as the decimal does (sign class,
--   then the decimal exponent offset by 500 in three digits, then the significant digits;
--   for a negative number the exponent and the digits complemented and a terminator "~"
--   appended). For every other type the key is the lexical form. One index on (fn_hash,
--   name, type, sort_key, cell_id) serves both the text and the number ranges. The store
--   computes the key (no SQL function needed, so a raw connection inserting rows would
--   have to compute it too: the store is the writer).
-- * Derived columns: receipts.asof, value_type, value and the typed-argument index (cells,
--   cell_args) are written by the store, in the same transaction as the receipt, rather
--   than by triggers parsing the bytes (there is no earlier SQLite writer to serve); the
--   receipts' asof, value_type and value are still checked against the bytes (json_extract).
-- * Append-only: UPDATE and DELETE are refused by triggers on the same tables as on
--   PostgreSQL (SQLite has no TRUNCATE; DELETE without WHERE fires the delete triggers),
--   and also on receipts, edges and descriptors, which the insert-only tables reference:
--   SQLite enforces foreign keys only on connections that turn them on, so the referenced
--   rows protect themselves. An insert whose key exists is refused too, so that an
--   INSERT OR REPLACE (whose implicit delete fires no trigger) cannot replace a row.
-- * References: besides the declared foreign keys (enforced on the store's connections),
--   triggers refuse an insert naming a receipt, descriptor, cell or manifest that is not
--   stored, so a raw connection without foreign keys cannot dangle one either.
-- * Order: versions and name_bindings refuse an insert not later than the latest of its
--   cell or name (a trigger), as on PostgreSQL. Concurrent writers need no lock of their
--   own: SQLite has one writer at a time per database, and a write transaction always
--   reads the latest committed state, so the check and the insert are atomic.
-- * receipts_fn_hash is not created: receipts_fn_hash_asof (fn_hash, asof) serves it.

CREATE TABLE descriptors (
    fn_hash TEXT PRIMARY KEY CHECK (length(fn_hash) = 64 AND fn_hash NOT GLOB '*[^0-9a-f]*'),
    -- Canonical (JCS) descriptor JSON.
    bytes BLOB NOT NULL
) STRICT;

CREATE TABLE receipts (
    hash TEXT PRIMARY KEY CHECK (length(hash) = 64 AND hash NOT GLOB '*[^0-9a-f]*'),
    -- Canonical (JCS) receipt JSON, sig included.
    bytes BLOB NOT NULL,
    cell_id TEXT NOT NULL CHECK (length(cell_id) = 64 AND cell_id NOT GLOB '*[^0-9a-f]*'),
    tx TEXT NOT NULL CHECK (tx GLOB '[0-9][0-9][0-9][0-9]-[0-9][0-9]-[0-9][0-9]T[0-9][0-9]:[0-9][0-9]:[0-9][0-9].[0-9][0-9][0-9][0-9][0-9][0-9]Z'),
    -- No foreign key: a receipt may arrive before (or without) its function's descriptor.
    fn_hash TEXT NOT NULL CHECK (length(fn_hash) = 64 AND fn_hash NOT GLOB '*[^0-9a-f]*'),
    publisher TEXT NOT NULL,
    -- The receipt's asof and typed value (its type and lexical form, booleans "true" and
    -- "false"), as in its bytes.
    asof TEXT NOT NULL CHECK (asof GLOB '[0-9][0-9][0-9][0-9]-[0-9][0-9]-[0-9][0-9]T[0-9][0-9]:[0-9][0-9]:[0-9][0-9].[0-9][0-9][0-9][0-9][0-9][0-9]Z'),
    value_type TEXT NOT NULL,
    value TEXT NOT NULL,
    CHECK (asof = json_extract(CAST(bytes AS TEXT), '$.asof')),
    CHECK (value_type = json_extract(CAST(bytes AS TEXT), '$.type')),
    CHECK (value = CASE json_type(CAST(bytes AS TEXT), '$.value')
                       WHEN 'true' THEN 'true' WHEN 'false' THEN 'false'
                       ELSE json_extract(CAST(bytes AS TEXT), '$.value') END)
) STRICT;
CREATE INDEX receipts_cell_tx ON receipts (cell_id, tx);
CREATE INDEX receipts_fn_hash_asof ON receipts (fn_hash, asof);
CREATE INDEX receipts_cell_asof ON receipts (cell_id, asof);

CREATE TABLE blobs (
    sha256 TEXT PRIMARY KEY CHECK (length(sha256) = 64 AND sha256 NOT GLOB '*[^0-9a-f]*'),
    bytes BLOB NOT NULL,
    length INTEGER NOT NULL CHECK (length = length(bytes)),
    -- Unknown when stored through put_blob, which takes only the content.
    media_type TEXT
) STRICT;

CREATE TABLE versions (
    cell_id TEXT NOT NULL CHECK (length(cell_id) = 64 AND cell_id NOT GLOB '*[^0-9a-f]*'),
    tx TEXT NOT NULL CHECK (tx GLOB '[0-9][0-9][0-9][0-9]-[0-9][0-9]-[0-9][0-9]T[0-9][0-9]:[0-9][0-9]:[0-9][0-9].[0-9][0-9][0-9][0-9][0-9][0-9]Z'),
    receipt_hash TEXT NOT NULL REFERENCES receipts (hash),
    PRIMARY KEY (cell_id, tx)
) STRICT;
CREATE INDEX versions_receipt_hash ON versions (receipt_hash);

CREATE TABLE edges (
    receipt_hash TEXT NOT NULL REFERENCES receipts (hash),
    -- No foreign key: an input may be a receipt this store does not hold.
    input_hash TEXT NOT NULL CHECK (length(input_hash) = 64 AND input_hash NOT GLOB '*[^0-9a-f]*'),
    kind TEXT NOT NULL CHECK (kind IN ('receipt', 'slice')),
    PRIMARY KEY (receipt_hash, input_hash)
) STRICT;
CREATE INDEX edges_input_hash ON edges (input_hash, receipt_hash);

CREATE TABLE namespaces (
    host TEXT PRIMARY KEY,
    -- Canonical (JCS) key document, checked as SPEC 9.2 requires for this host.
    keydoc BLOB NOT NULL,
    validated_at TEXT NOT NULL CHECK (validated_at GLOB '[0-9][0-9][0-9][0-9]-[0-9][0-9]-[0-9][0-9]T[0-9][0-9]:[0-9][0-9]:[0-9][0-9].[0-9][0-9][0-9][0-9][0-9][0-9]Z'),
    suspended_at TEXT CHECK (suspended_at IS NULL OR suspended_at GLOB '[0-9][0-9][0-9][0-9]-[0-9][0-9]-[0-9][0-9]T[0-9][0-9]:[0-9][0-9]:[0-9][0-9].[0-9][0-9][0-9][0-9][0-9][0-9]Z')
) STRICT;

CREATE TABLE name_bindings (
    fn_name TEXT NOT NULL,
    tx TEXT NOT NULL CHECK (tx GLOB '[0-9][0-9][0-9][0-9]-[0-9][0-9]-[0-9][0-9]T[0-9][0-9]:[0-9][0-9]:[0-9][0-9].[0-9][0-9][0-9][0-9][0-9][0-9]Z'),
    fn_hash TEXT NOT NULL REFERENCES descriptors (fn_hash),
    PRIMARY KEY (fn_name, tx)
) STRICT;

-- Computed versions: insert-only, in any order (no tx-order trigger), keyed on (cell, tx,
-- asof).
CREATE TABLE computed_versions (
    cell_id TEXT NOT NULL CHECK (length(cell_id) = 64 AND cell_id NOT GLOB '*[^0-9a-f]*'),
    tx TEXT NOT NULL CHECK (tx GLOB '[0-9][0-9][0-9][0-9]-[0-9][0-9]-[0-9][0-9]T[0-9][0-9]:[0-9][0-9]:[0-9][0-9].[0-9][0-9][0-9][0-9][0-9][0-9]Z'),
    asof TEXT NOT NULL CHECK (asof GLOB '[0-9][0-9][0-9][0-9]-[0-9][0-9]-[0-9][0-9]T[0-9][0-9]:[0-9][0-9]:[0-9][0-9].[0-9][0-9][0-9][0-9][0-9][0-9]Z'),
    receipt_hash TEXT NOT NULL REFERENCES receipts (hash),
    PRIMARY KEY (cell_id, tx, asof)
) STRICT;
-- computed_asof: the greatest asof <= A of a cell, ties the greatest tx (a backward scan).
CREATE INDEX computed_versions_asof ON computed_versions (cell_id, asof, tx);

-- The typed-argument index: one row per cell any stored receipt is of, one per argument.
CREATE TABLE cells (
    cell_id TEXT PRIMARY KEY CHECK (length(cell_id) = 64 AND cell_id NOT GLOB '*[^0-9a-f]*'),
    fn_hash TEXT NOT NULL CHECK (length(fn_hash) = 64 AND fn_hash NOT GLOB '*[^0-9a-f]*'),
    ns TEXT NOT NULL,
    fn TEXT NOT NULL,
    nargs INTEGER NOT NULL CHECK (nargs BETWEEN 0 AND 32)
) STRICT;
-- An unbounded slice: every cell of a function.
CREATE INDEX cells_fn_hash ON cells (fn_hash, cell_id);

CREATE TABLE cell_args (
    cell_id TEXT NOT NULL REFERENCES cells (cell_id),
    name TEXT NOT NULL,
    type TEXT NOT NULL CHECK (type IN ('number', 'string', 'boolean', 'date', 'instant', 'blob')),
    -- The lexical form (SPEC 3); booleans "true" and "false".
    value TEXT NOT NULL,
    -- The key ranges and order compare: a number's sortable encoding, else the value.
    sort_key TEXT NOT NULL CHECK (type = 'number' OR sort_key = value),
    -- The cell's function hash, for the index below.
    fn_hash TEXT NOT NULL,
    PRIMARY KEY (cell_id, name)
) STRICT;
-- A bound or ranged argument of a slice, in its type's order.
CREATE INDEX cell_args_key ON cell_args (fn_hash, name, type, sort_key, cell_id);

-- Manifests by entry, and which manifests each member receipt hash belongs to.
CREATE TABLE manifests (
    entry TEXT PRIMARY KEY CHECK (entry = 'slice:' || slice_id || ':' || members_hash),
    slice_id TEXT NOT NULL CHECK (length(slice_id) = 64 AND slice_id NOT GLOB '*[^0-9a-f]*'),
    members_hash TEXT NOT NULL CHECK (length(members_hash) = 64 AND members_hash NOT GLOB '*[^0-9a-f]*'),
    -- The slice's function hash (of the version slice's cell for a version slice).
    fn_hash TEXT NOT NULL CHECK (length(fn_hash) = 64 AND fn_hash NOT GLOB '*[^0-9a-f]*'),
    members INTEGER NOT NULL CHECK (members BETWEEN 0 AND 1048576),
    -- Canonical (JCS) manifest JSON.
    bytes BLOB NOT NULL
) STRICT;

CREATE TABLE manifest_members (
    entry TEXT NOT NULL REFERENCES manifests (entry),
    -- No foreign key: a member may be a receipt this store does not hold.
    member_hash TEXT NOT NULL CHECK (length(member_hash) = 64 AND member_hash NOT GLOB '*[^0-9a-f]*'),
    PRIMARY KEY (entry, member_hash)
) STRICT;
CREATE INDEX manifest_members_member ON manifest_members (member_hash, entry);

-- The slice entries among each stored receipt's inputs.
CREATE TABLE slice_edges (
    receipt_hash TEXT NOT NULL REFERENCES receipts (hash),
    -- No foreign key: the manifest may not be stored (yet).
    entry TEXT NOT NULL CHECK (length(entry) = 135 AND entry GLOB 'slice:*:*'
        AND substr(entry, 7, 64) NOT GLOB '*[^0-9a-f]*' AND substr(entry, 71, 1) = ':'
        AND substr(entry, 72) NOT GLOB '*[^0-9a-f]*'),
    PRIMARY KEY (receipt_hash, entry)
) STRICT;
CREATE INDEX slice_edges_entry ON slice_edges (entry, receipt_hash);

-- Append-only: no UPDATE, no DELETE.

CREATE TRIGGER descriptors_no_update BEFORE UPDATE ON descriptors
    BEGIN SELECT RAISE(ABORT, 'datumesh: UPDATE on descriptors refused: the table is append-only'); END;
CREATE TRIGGER descriptors_no_delete BEFORE DELETE ON descriptors
    BEGIN SELECT RAISE(ABORT, 'datumesh: DELETE on descriptors refused: the table is append-only'); END;
CREATE TRIGGER receipts_no_update BEFORE UPDATE ON receipts
    BEGIN SELECT RAISE(ABORT, 'datumesh: UPDATE on receipts refused: the table is append-only'); END;
CREATE TRIGGER receipts_no_delete BEFORE DELETE ON receipts
    BEGIN SELECT RAISE(ABORT, 'datumesh: DELETE on receipts refused: the table is append-only'); END;
CREATE TRIGGER edges_no_update BEFORE UPDATE ON edges
    BEGIN SELECT RAISE(ABORT, 'datumesh: UPDATE on edges refused: the table is append-only'); END;
CREATE TRIGGER edges_no_delete BEFORE DELETE ON edges
    BEGIN SELECT RAISE(ABORT, 'datumesh: DELETE on edges refused: the table is append-only'); END;
CREATE TRIGGER versions_no_update BEFORE UPDATE ON versions
    BEGIN SELECT RAISE(ABORT, 'datumesh: UPDATE on versions refused: the table is append-only'); END;
CREATE TRIGGER versions_no_delete BEFORE DELETE ON versions
    BEGIN SELECT RAISE(ABORT, 'datumesh: DELETE on versions refused: the table is append-only'); END;
CREATE TRIGGER name_bindings_no_update BEFORE UPDATE ON name_bindings
    BEGIN SELECT RAISE(ABORT, 'datumesh: UPDATE on name_bindings refused: the table is append-only'); END;
CREATE TRIGGER name_bindings_no_delete BEFORE DELETE ON name_bindings
    BEGIN SELECT RAISE(ABORT, 'datumesh: DELETE on name_bindings refused: the table is append-only'); END;
CREATE TRIGGER computed_versions_no_update BEFORE UPDATE ON computed_versions
    BEGIN SELECT RAISE(ABORT, 'datumesh: UPDATE on computed_versions refused: the table is append-only'); END;
CREATE TRIGGER computed_versions_no_delete BEFORE DELETE ON computed_versions
    BEGIN SELECT RAISE(ABORT, 'datumesh: DELETE on computed_versions refused: the table is append-only'); END;
CREATE TRIGGER cells_no_update BEFORE UPDATE ON cells
    BEGIN SELECT RAISE(ABORT, 'datumesh: UPDATE on cells refused: the table is append-only'); END;
CREATE TRIGGER cells_no_delete BEFORE DELETE ON cells
    BEGIN SELECT RAISE(ABORT, 'datumesh: DELETE on cells refused: the table is append-only'); END;
CREATE TRIGGER cell_args_no_update BEFORE UPDATE ON cell_args
    BEGIN SELECT RAISE(ABORT, 'datumesh: UPDATE on cell_args refused: the table is append-only'); END;
CREATE TRIGGER cell_args_no_delete BEFORE DELETE ON cell_args
    BEGIN SELECT RAISE(ABORT, 'datumesh: DELETE on cell_args refused: the table is append-only'); END;
CREATE TRIGGER manifests_no_update BEFORE UPDATE ON manifests
    BEGIN SELECT RAISE(ABORT, 'datumesh: UPDATE on manifests refused: the table is append-only'); END;
CREATE TRIGGER manifests_no_delete BEFORE DELETE ON manifests
    BEGIN SELECT RAISE(ABORT, 'datumesh: DELETE on manifests refused: the table is append-only'); END;
CREATE TRIGGER manifest_members_no_update BEFORE UPDATE ON manifest_members
    BEGIN SELECT RAISE(ABORT, 'datumesh: UPDATE on manifest_members refused: the table is append-only'); END;
CREATE TRIGGER manifest_members_no_delete BEFORE DELETE ON manifest_members
    BEGIN SELECT RAISE(ABORT, 'datumesh: DELETE on manifest_members refused: the table is append-only'); END;
CREATE TRIGGER slice_edges_no_update BEFORE UPDATE ON slice_edges
    BEGIN SELECT RAISE(ABORT, 'datumesh: UPDATE on slice_edges refused: the table is append-only'); END;
CREATE TRIGGER slice_edges_no_delete BEFORE DELETE ON slice_edges
    BEGIN SELECT RAISE(ABORT, 'datumesh: DELETE on slice_edges refused: the table is append-only'); END;

-- No insert over an existing key (an INSERT OR REPLACE would delete the row first).

CREATE TRIGGER descriptors_no_replace BEFORE INSERT ON descriptors
    WHEN EXISTS (SELECT 1 FROM descriptors k WHERE k.fn_hash = NEW.fn_hash)
    BEGIN SELECT RAISE(ABORT, 'datumesh: INSERT on descriptors refused: the key exists and the table is append-only'); END;
CREATE TRIGGER receipts_no_replace BEFORE INSERT ON receipts
    WHEN EXISTS (SELECT 1 FROM receipts k WHERE k.hash = NEW.hash)
    BEGIN SELECT RAISE(ABORT, 'datumesh: INSERT on receipts refused: the key exists and the table is append-only'); END;
CREATE TRIGGER edges_no_replace BEFORE INSERT ON edges
    WHEN EXISTS (SELECT 1 FROM edges k WHERE k.receipt_hash = NEW.receipt_hash AND k.input_hash = NEW.input_hash)
    BEGIN SELECT RAISE(ABORT, 'datumesh: INSERT on edges refused: the key exists and the table is append-only'); END;
CREATE TRIGGER computed_versions_no_replace BEFORE INSERT ON computed_versions
    WHEN EXISTS (SELECT 1 FROM computed_versions k WHERE k.cell_id = NEW.cell_id AND k.tx = NEW.tx AND k.asof = NEW.asof)
    BEGIN SELECT RAISE(ABORT, 'datumesh: INSERT on computed_versions refused: the key exists and the table is append-only'); END;
CREATE TRIGGER cells_no_replace BEFORE INSERT ON cells
    WHEN EXISTS (SELECT 1 FROM cells k WHERE k.cell_id = NEW.cell_id)
    BEGIN SELECT RAISE(ABORT, 'datumesh: INSERT on cells refused: the key exists and the table is append-only'); END;
CREATE TRIGGER cell_args_no_replace BEFORE INSERT ON cell_args
    WHEN EXISTS (SELECT 1 FROM cell_args k WHERE k.cell_id = NEW.cell_id AND k.name = NEW.name)
    BEGIN SELECT RAISE(ABORT, 'datumesh: INSERT on cell_args refused: the key exists and the table is append-only'); END;
CREATE TRIGGER manifests_no_replace BEFORE INSERT ON manifests
    WHEN EXISTS (SELECT 1 FROM manifests k WHERE k.entry = NEW.entry)
    BEGIN SELECT RAISE(ABORT, 'datumesh: INSERT on manifests refused: the key exists and the table is append-only'); END;
CREATE TRIGGER manifest_members_no_replace BEFORE INSERT ON manifest_members
    WHEN EXISTS (SELECT 1 FROM manifest_members k WHERE k.entry = NEW.entry AND k.member_hash = NEW.member_hash)
    BEGIN SELECT RAISE(ABORT, 'datumesh: INSERT on manifest_members refused: the key exists and the table is append-only'); END;
CREATE TRIGGER slice_edges_no_replace BEFORE INSERT ON slice_edges
    WHEN EXISTS (SELECT 1 FROM slice_edges k WHERE k.receipt_hash = NEW.receipt_hash AND k.entry = NEW.entry)
    BEGIN SELECT RAISE(ABORT, 'datumesh: INSERT on slice_edges refused: the key exists and the table is append-only'); END;

-- In order: a version or binding later than every one of its cell or name.
CREATE TRIGGER versions_in_order BEFORE INSERT ON versions
    WHEN EXISTS (SELECT 1 FROM versions v WHERE v.cell_id = NEW.cell_id AND v.tx >= NEW.tx)
    BEGIN SELECT RAISE(ABORT, 'datumesh: version tx is not later than the latest of its cell'); END;
CREATE TRIGGER name_bindings_in_order BEFORE INSERT ON name_bindings
    WHEN EXISTS (SELECT 1 FROM name_bindings b WHERE b.fn_name = NEW.fn_name AND b.tx >= NEW.tx)
    BEGIN SELECT RAISE(ABORT, 'datumesh: binding tx is not later than the latest of its name'); END;

-- References, whether or not the connection enforces foreign keys.

CREATE TRIGGER versions_receipt_hash_stored BEFORE INSERT ON versions
    WHEN NOT EXISTS (SELECT 1 FROM receipts r WHERE r.hash = NEW.receipt_hash)
    BEGIN SELECT RAISE(ABORT, 'datumesh: INSERT on versions refused: no such receipt is stored'); END;
CREATE TRIGGER computed_versions_receipt_hash_stored BEFORE INSERT ON computed_versions
    WHEN NOT EXISTS (SELECT 1 FROM receipts r WHERE r.hash = NEW.receipt_hash)
    BEGIN SELECT RAISE(ABORT, 'datumesh: INSERT on computed_versions refused: no such receipt is stored'); END;
CREATE TRIGGER edges_receipt_hash_stored BEFORE INSERT ON edges
    WHEN NOT EXISTS (SELECT 1 FROM receipts r WHERE r.hash = NEW.receipt_hash)
    BEGIN SELECT RAISE(ABORT, 'datumesh: INSERT on edges refused: no such receipt is stored'); END;
CREATE TRIGGER slice_edges_receipt_hash_stored BEFORE INSERT ON slice_edges
    WHEN NOT EXISTS (SELECT 1 FROM receipts r WHERE r.hash = NEW.receipt_hash)
    BEGIN SELECT RAISE(ABORT, 'datumesh: INSERT on slice_edges refused: no such receipt is stored'); END;
CREATE TRIGGER name_bindings_fn_hash_stored BEFORE INSERT ON name_bindings
    WHEN NOT EXISTS (SELECT 1 FROM descriptors r WHERE r.fn_hash = NEW.fn_hash)
    BEGIN SELECT RAISE(ABORT, 'datumesh: INSERT on name_bindings refused: no such descriptor is stored'); END;
CREATE TRIGGER cell_args_cell_id_stored BEFORE INSERT ON cell_args
    WHEN NOT EXISTS (SELECT 1 FROM cells r WHERE r.cell_id = NEW.cell_id)
    BEGIN SELECT RAISE(ABORT, 'datumesh: INSERT on cell_args refused: no such cell is stored'); END;
CREATE TRIGGER manifest_members_entry_stored BEFORE INSERT ON manifest_members
    WHEN NOT EXISTS (SELECT 1 FROM manifests r WHERE r.entry = NEW.entry)
    BEGIN SELECT RAISE(ABORT, 'datumesh: INSERT on manifest_members refused: no such manifest is stored'); END;
