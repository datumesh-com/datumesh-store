//! Every SQL statement the store runs, with its parameter types. `PgStore::check_statements`
//! prepares all of them against the live schema; the sync driver has no compile-time query
//! checking, so that test is what catches a statement and a schema that disagree.

use postgres::types::Type;

macro_rules! statements {
    ($($name:ident = $sql:literal [$($ty:ident),*];)*) => {
        /// A statement.
        #[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
        pub(crate) enum Q {
            $($name,)*
        }

        impl Q {
            pub(crate) const ALL: &'static [Q] = &[$(Q::$name,)*];

            pub(crate) fn sql(self) -> &'static str {
                match self {
                    $(Q::$name => $sql,)*
                }
            }

            pub(crate) fn types(self) -> &'static [Type] {
                match self {
                    $(Q::$name => &[$(Type::$ty),*],)*
                }
            }
        }
    };
}

statements! {
    Lock = "SELECT datumesh_lock($1, $2)" [TEXT, TEXT];

    DescriptorGet = "SELECT bytes FROM descriptors WHERE fn_hash = $1" [TEXT];
    DescriptorPut = "INSERT INTO descriptors (fn_hash, bytes) VALUES ($1, $2) ON CONFLICT (fn_hash) DO NOTHING"
        [TEXT, BYTEA];
    DescriptorExists = "SELECT EXISTS (SELECT 1 FROM descriptors WHERE fn_hash = $1)" [TEXT];

    ReceiptGet = "SELECT bytes FROM receipts WHERE hash = $1" [TEXT];
    ReceiptMeta = "SELECT cell_id, tx, asof FROM receipts WHERE hash = $1" [TEXT];
    ReceiptPut = "INSERT INTO receipts (hash, bytes, cell_id, tx, tx_at, asof, asof_at, fn_hash, publisher)
                  VALUES ($1, $2, $3, $4, datumesh_instant($4), $5, datumesh_instant($5), $6, $7)
                  ON CONFLICT (hash) DO NOTHING"
        [TEXT, BYTEA, TEXT, TEXT, TEXT, TEXT, TEXT];
    EdgesPut = "INSERT INTO edges (receipt_hash, input_hash, kind)
                SELECT $1, i, 'receipt' FROM unnest($2::text[]) AS i ON CONFLICT DO NOTHING"
        [TEXT, TEXT_ARRAY];

    BlobGet = "SELECT bytes FROM blobs WHERE sha256 = $1" [TEXT];
    BlobPut = "INSERT INTO blobs (sha256, bytes, length) VALUES ($1, $2, $3) ON CONFLICT (sha256) DO NOTHING"
        [TEXT, BYTEA, INT8];

    // Versions come with their receipt's asof (Version::asof). A scalar subquery rather than
    // a join, so that the versions table is looked at first (a missing column of it is still
    // reported as such).
    VersionLatestTx = "SELECT tx FROM versions WHERE cell_id = $1 ORDER BY tx DESC LIMIT 1" [TEXT];
    VersionLatest = "SELECT v.tx, v.receipt_hash, (SELECT r.asof FROM receipts r WHERE r.hash = v.receipt_hash)
                     FROM versions v WHERE v.cell_id = $1 ORDER BY v.tx DESC LIMIT 1"
        [TEXT];
    VersionAsof = "SELECT v.tx, v.receipt_hash, (SELECT r.asof FROM receipts r WHERE r.hash = v.receipt_hash)
                   FROM versions v WHERE v.cell_id = $1 AND v.tx <= $2 ORDER BY v.tx DESC LIMIT 1"
        [TEXT, TEXT];
    VersionRange = "SELECT v.tx, v.receipt_hash, (SELECT r.asof FROM receipts r WHERE r.hash = v.receipt_hash)
                    FROM versions v WHERE v.cell_id = $1 AND v.tx >= $2 AND v.tx <= $3 ORDER BY v.tx"
        [TEXT, TEXT, TEXT];
    // A stored version answers as of A when its tx <= A and its receipt's asof <= A.
    StoredAsof = "SELECT v.tx, v.receipt_hash, r.asof FROM versions v JOIN receipts r ON r.hash = v.receipt_hash
                  WHERE v.cell_id = $1 AND v.tx <= $2 AND r.asof <= $2 ORDER BY v.tx DESC LIMIT 1"
        [TEXT, TEXT];
    StoredAsofReceipt = "SELECT v.tx, v.receipt_hash, r.asof, r.bytes FROM versions v JOIN receipts r ON r.hash = v.receipt_hash
                         WHERE v.cell_id = $1 AND v.tx <= $2 AND r.asof <= $2 ORDER BY v.tx DESC LIMIT 1"
        [TEXT, TEXT];
    VersionPut = "INSERT INTO versions (cell_id, tx, tx_at, receipt_hash) VALUES ($1, $2, datumesh_instant($2), $3)"
        [TEXT, TEXT, TEXT];
    // The Resolver's evaluate of a stored function: the receipt of stored_asof's version.
    ValueAsof = "SELECT r.bytes FROM versions v JOIN receipts r ON r.hash = v.receipt_hash
                 WHERE v.cell_id = $1 AND v.tx <= $2 AND r.asof <= $2 ORDER BY v.tx DESC LIMIT 1"
        [TEXT, TEXT];

    ComputedPut = "INSERT INTO computed_versions (cell_id, tx, tx_at, asof, asof_at, receipt_hash)
                   VALUES ($1, $2, datumesh_instant($2), $3, datumesh_instant($3), $4)
                   ON CONFLICT (cell_id, tx, asof) DO NOTHING"
        [TEXT, TEXT, TEXT, TEXT];
    ComputedGet = "SELECT receipt_hash FROM computed_versions WHERE cell_id = $1 AND tx = $2 AND asof = $3"
        [TEXT, TEXT, TEXT];
    // The greatest asof <= A, ties the greatest tx.
    ComputedAsof = "SELECT tx, receipt_hash, asof FROM computed_versions
                    WHERE cell_id = $1 AND asof <= $2 ORDER BY asof DESC, tx DESC LIMIT 1"
        [TEXT, TEXT];
    ComputedAsofReceipt = "SELECT c.tx, c.receipt_hash, c.asof, r.bytes FROM
                               (SELECT tx, receipt_hash, asof FROM computed_versions
                                 WHERE cell_id = $1 AND asof <= $2 ORDER BY asof DESC, tx DESC LIMIT 1) c
                           JOIN receipts r ON r.hash = c.receipt_hash"
        [TEXT, TEXT];

    // The cells of stored receipts that list any stored receipt of the cell among their inputs.
    CellDependents = "SELECT DISTINCT d.cell_id FROM receipts i
                      JOIN edges e ON e.input_hash = i.hash
                      JOIN receipts d ON d.hash = e.receipt_hash
                      WHERE i.cell_id = $1 ORDER BY d.cell_id"
        [TEXT];

    Dependents = "SELECT receipt_hash FROM edges WHERE input_hash = $1 ORDER BY receipt_hash" [TEXT];
    Inputs = "SELECT input_hash FROM edges WHERE receipt_hash = $1 ORDER BY input_hash" [TEXT];

    NamespaceGet = "SELECT keydoc, validated_at, suspended_at FROM namespaces WHERE host = $1" [TEXT];
    NamespacePut = "INSERT INTO namespaces (host, keydoc, validated_at, validated_at_at)
                    VALUES ($1, $2, $3, datumesh_instant($3))
                    ON CONFLICT (host) DO UPDATE SET keydoc = EXCLUDED.keydoc,
                        validated_at = EXCLUDED.validated_at, validated_at_at = EXCLUDED.validated_at_at"
        [TEXT, BYTEA, TEXT];

    NameLatest = "SELECT tx FROM name_bindings WHERE fn_name = $1 ORDER BY tx DESC LIMIT 1" [TEXT];
    NamePut = "INSERT INTO name_bindings (fn_name, tx, tx_at, fn_hash) VALUES ($1, $2, datumesh_instant($2), $3)"
        [TEXT, TEXT, TEXT];
    NameAsof = "SELECT fn_hash FROM name_bindings WHERE fn_name = $1 AND tx <= $2 ORDER BY tx DESC LIMIT 1"
        [TEXT, TEXT];
    BindingAsof = "SELECT fn_hash, tx FROM name_bindings WHERE fn_name = $1 AND tx <= $2 ORDER BY tx DESC LIMIT 1"
        [TEXT, TEXT];
}
