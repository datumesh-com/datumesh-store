//! Every SQL statement the store runs, with its parameter types. `PgStore::check_statements`
//! prepares all of them against the live schema; the sync driver has no compile-time query
//! checking, so that test is what catches a statement and a schema that disagree.

use postgres::types::Type;

macro_rules! statements {
    ($($name:ident = $sql:expr => [$($ty:ident),*];)*) => {
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

/// The statement of the members of a slice of cells, with the candidates narrowed by
/// `$drive`.
macro_rules! slice_members {
    ($drive:literal) => {
        concat!(
            "SELECT m.tx, m.receipt_hash, m.asof, m.bytes, c.cell_id,
                    ARRAY(SELECT a.type || ':' || a.value FROM cell_args a
                           WHERE a.cell_id = c.cell_id AND a.name = ANY($11::text[]) ORDER BY a.name)
               FROM cells c
              CROSS JOIN LATERAL (SELECT v.tx, v.receipt_hash, r.asof, r.bytes
                                    FROM versions v JOIN receipts r ON r.hash = v.receipt_hash
                                   WHERE v.cell_id = c.cell_id AND v.tx <= $5 AND r.asof <= $5
                                   ORDER BY v.tx DESC LIMIT 1) m
              WHERE c.fn_hash = $1 AND c.ns = $2 AND c.fn = $3 AND c.nargs = $4
                AND NOT EXISTS (
                    SELECT 1 FROM unnest($6::text[], $7::text[], $8::text[], $9::text[], $10::text[]) AS k(name, kind, type, lo, hi)
                     WHERE NOT EXISTS (
                         SELECT 1 FROM cell_args a
                          WHERE a.cell_id = c.cell_id AND a.name = k.name
                            AND CASE WHEN k.kind = 'open' THEN true
                                     WHEN k.kind = 'value' THEN a.type = k.type AND a.value = k.lo
                                     WHEN k.type = 'number' THEN a.type = 'number' AND a.num BETWEEN k.lo::numeric AND k.hi::numeric
                                     ELSE a.type = k.type AND a.value BETWEEN k.lo AND k.hi END))
                ",
            $drive
        )
    };
}

statements! {
    Lock = "SELECT datumesh_lock($1, $2)" => [TEXT, TEXT];

    DescriptorGet = "SELECT bytes FROM descriptors WHERE fn_hash = $1" => [TEXT];
    DescriptorPut = "INSERT INTO descriptors (fn_hash, bytes) VALUES ($1, $2) ON CONFLICT (fn_hash) DO NOTHING"
        => [TEXT, BYTEA];
    DescriptorExists = "SELECT EXISTS (SELECT 1 FROM descriptors WHERE fn_hash = $1)" => [TEXT];

    ReceiptGet = "SELECT bytes FROM receipts WHERE hash = $1" => [TEXT];
    ReceiptMeta = "SELECT cell_id, tx, asof FROM receipts WHERE hash = $1" => [TEXT];
    ReceiptPut = "INSERT INTO receipts (hash, bytes, cell_id, tx, tx_at, asof, asof_at, fn_hash, publisher)
                  VALUES ($1, $2, $3, $4, datumesh_instant($4), $5, datumesh_instant($5), $6, $7)
                  ON CONFLICT (hash) DO NOTHING"
        => [TEXT, BYTEA, TEXT, TEXT, TEXT, TEXT, TEXT];
    SliceEdgesPut = "INSERT INTO slice_edges (receipt_hash, entry)
                     SELECT $1, e FROM unnest($2::text[]) AS e ON CONFLICT DO NOTHING"
        => [TEXT, TEXT_ARRAY];
    EdgesPut = "INSERT INTO edges (receipt_hash, input_hash, kind)
                SELECT $1, i, 'receipt' FROM unnest($2::text[]) AS i ON CONFLICT DO NOTHING"
        => [TEXT, TEXT_ARRAY];

    BlobGet = "SELECT bytes FROM blobs WHERE sha256 = $1" => [TEXT];
    BlobPut = "INSERT INTO blobs (sha256, bytes, length) VALUES ($1, $2, $3) ON CONFLICT (sha256) DO NOTHING"
        => [TEXT, BYTEA, INT8];

    // Versions come with their receipt's asof (Version::asof). A scalar subquery rather than
    // a join, so that the versions table is looked at first (a missing column of it is still
    // reported as such).
    VersionLatestTx = "SELECT tx FROM versions WHERE cell_id = $1 ORDER BY tx DESC LIMIT 1" => [TEXT];
    VersionLatest = "SELECT v.tx, v.receipt_hash, (SELECT r.asof FROM receipts r WHERE r.hash = v.receipt_hash)
                     FROM versions v WHERE v.cell_id = $1 ORDER BY v.tx DESC LIMIT 1"
        => [TEXT];
    VersionAsof = "SELECT v.tx, v.receipt_hash, (SELECT r.asof FROM receipts r WHERE r.hash = v.receipt_hash)
                   FROM versions v WHERE v.cell_id = $1 AND v.tx <= $2 ORDER BY v.tx DESC LIMIT 1"
        => [TEXT, TEXT];
    VersionRange = "SELECT v.tx, v.receipt_hash, (SELECT r.asof FROM receipts r WHERE r.hash = v.receipt_hash)
                    FROM versions v WHERE v.cell_id = $1 AND v.tx >= $2 AND v.tx <= $3 ORDER BY v.tx"
        => [TEXT, TEXT, TEXT];
    // A stored version answers as of A when its tx <= A and its receipt's asof <= A.
    StoredAsof = "SELECT v.tx, v.receipt_hash, r.asof FROM versions v JOIN receipts r ON r.hash = v.receipt_hash
                  WHERE v.cell_id = $1 AND v.tx <= $2 AND r.asof <= $2 ORDER BY v.tx DESC LIMIT 1"
        => [TEXT, TEXT];
    StoredAsofReceipt = "SELECT v.tx, v.receipt_hash, r.asof, r.bytes FROM versions v JOIN receipts r ON r.hash = v.receipt_hash
                         WHERE v.cell_id = $1 AND v.tx <= $2 AND r.asof <= $2 ORDER BY v.tx DESC LIMIT 1"
        => [TEXT, TEXT];
    VersionPut = "INSERT INTO versions (cell_id, tx, tx_at, receipt_hash) VALUES ($1, $2, datumesh_instant($2), $3)"
        => [TEXT, TEXT, TEXT];
    // The Resolver's evaluate of a stored function: the receipt of stored_asof's version.
    ValueAsof = "SELECT r.bytes FROM versions v JOIN receipts r ON r.hash = v.receipt_hash
                 WHERE v.cell_id = $1 AND v.tx <= $2 AND r.asof <= $2 ORDER BY v.tx DESC LIMIT 1"
        => [TEXT, TEXT];

    ComputedPut = "INSERT INTO computed_versions (cell_id, tx, tx_at, asof, asof_at, receipt_hash)
                   VALUES ($1, $2, datumesh_instant($2), $3, datumesh_instant($3), $4)
                   ON CONFLICT (cell_id, tx, asof) DO NOTHING"
        => [TEXT, TEXT, TEXT, TEXT];
    ComputedGet = "SELECT receipt_hash FROM computed_versions WHERE cell_id = $1 AND tx = $2 AND asof = $3"
        => [TEXT, TEXT, TEXT];
    // The greatest asof <= A, ties the greatest tx.
    ComputedAsof = "SELECT tx, receipt_hash, asof FROM computed_versions
                    WHERE cell_id = $1 AND asof <= $2 ORDER BY asof DESC, tx DESC LIMIT 1"
        => [TEXT, TEXT];
    ComputedAsofReceipt = "SELECT c.tx, c.receipt_hash, c.asof, r.bytes FROM
                               (SELECT tx, receipt_hash, asof FROM computed_versions
                                 WHERE cell_id = $1 AND asof <= $2 ORDER BY asof DESC, tx DESC LIMIT 1) c
                           JOIN receipts r ON r.hash = c.receipt_hash"
        => [TEXT, TEXT];

    // The cells of stored receipts that list any stored receipt of the cell among their inputs,
    // or a slice entry whose stored manifest lists one (0.3.1).
    CellDependents = "SELECT d.cell_id FROM receipts i
                      JOIN edges e ON e.input_hash = i.hash
                      JOIN receipts d ON d.hash = e.receipt_hash
                      WHERE i.cell_id = $1
                      UNION
                      SELECT d.cell_id FROM receipts i
                      JOIN manifest_members mm ON mm.member_hash = i.hash
                      JOIN slice_edges se ON se.entry = mm.entry
                      JOIN receipts d ON d.hash = se.receipt_hash
                      WHERE i.cell_id = $1
                      ORDER BY 1"
        => [TEXT];

    Dependents = "SELECT receipt_hash FROM edges WHERE input_hash = $1 ORDER BY receipt_hash" => [TEXT];
    Inputs = "SELECT input_hash FROM edges WHERE receipt_hash = $1 ORDER BY input_hash" => [TEXT];

    NamespaceGet = "SELECT keydoc, validated_at, suspended_at FROM namespaces WHERE host = $1" => [TEXT];
    NamespacePut = "INSERT INTO namespaces (host, keydoc, validated_at, validated_at_at)
                    VALUES ($1, $2, $3, datumesh_instant($3))
                    ON CONFLICT (host) DO UPDATE SET keydoc = EXCLUDED.keydoc,
                        validated_at = EXCLUDED.validated_at, validated_at_at = EXCLUDED.validated_at_at"
        => [TEXT, BYTEA, TEXT];

    NameLatest = "SELECT tx FROM name_bindings WHERE fn_name = $1 ORDER BY tx DESC LIMIT 1" => [TEXT];
    NamePut = "INSERT INTO name_bindings (fn_name, tx, tx_at, fn_hash) VALUES ($1, $2, datumesh_instant($2), $3)"
        => [TEXT, TEXT, TEXT];
    NameAsof = "SELECT fn_hash FROM name_bindings WHERE fn_name = $1 AND tx <= $2 ORDER BY tx DESC LIMIT 1"
        => [TEXT, TEXT];
    BindingAsof = "SELECT fn_hash, tx FROM name_bindings WHERE fn_name = $1 AND tx <= $2 ORDER BY tx DESC LIMIT 1"
        => [TEXT, TEXT];

    // Manifests (0.3.1).
    ManifestPut = "INSERT INTO manifests (entry, slice_id, members_hash, fn_hash, members, bytes)
                   VALUES ($1, $2, $3, $4, $5, $6) ON CONFLICT (entry) DO NOTHING"
        => [TEXT, TEXT, TEXT, TEXT, INT4, BYTEA];
    ManifestMembersPut = "INSERT INTO manifest_members (entry, member_hash) SELECT $1, m FROM unnest($2::text[]) AS m"
        => [TEXT, TEXT_ARRAY];
    ManifestGet = "SELECT bytes FROM manifests WHERE entry = $1" => [TEXT];
    MemberManifests = "SELECT entry FROM manifest_members WHERE member_hash = $1 ORDER BY entry" => [TEXT];
    SliceEntries = "SELECT entry FROM slice_edges WHERE receipt_hash = $1 ORDER BY entry" => [TEXT];
    SliceDependents = "SELECT receipt_hash FROM slice_edges WHERE entry = $1 ORDER BY receipt_hash" => [TEXT];

    // A version slice: the stored versions with from <= tx <= to answering as of A.
    VersionsBetween = "SELECT v.tx, v.receipt_hash, r.asof, r.bytes FROM versions v JOIN receipts r ON r.hash = v.receipt_hash
                       WHERE v.cell_id = $1 AND v.tx >= $2 AND v.tx <= $3 AND v.tx <= $4 AND r.asof <= $4 ORDER BY v.tx"
        => [TEXT, TEXT, TEXT, TEXT];

    // The members of a slice of cells, as of A ($5): cells of the function ($1-$3) with its
    // number of arguments ($4) and every constraint ($6-$10: name, kind open / value / range,
    // type, lo, hi; a value's lexical form is its lo) met, each with the stored version
    // answering as of A and the "type:value" of its key arguments ($11, in name order).
    // Candidates come from all the function's cells, or from a range scan of one argument
    // ($12-$15: name, type, lo, hi) of the typed-argument index.
    SliceMembersAll = slice_members!("") => [TEXT, TEXT, TEXT, INT4, TEXT, TEXT_ARRAY, TEXT_ARRAY, TEXT_ARRAY, TEXT_ARRAY, TEXT_ARRAY, TEXT_ARRAY, TEXT, TEXT, TEXT, TEXT];
    SliceMembersText = slice_members!(
        "AND c.cell_id IN (SELECT a.cell_id FROM cell_args a
                            WHERE a.fn_hash = $1 AND a.name = $12 AND a.type = $13 AND a.value BETWEEN $14 AND $15)"
    ) => [TEXT, TEXT, TEXT, INT4, TEXT, TEXT_ARRAY, TEXT_ARRAY, TEXT_ARRAY, TEXT_ARRAY, TEXT_ARRAY, TEXT_ARRAY, TEXT, TEXT, TEXT, TEXT];
    SliceMembersNum = slice_members!(
        "AND $13 = 'number' AND c.cell_id IN (SELECT a.cell_id FROM cell_args a
                            WHERE a.fn_hash = $1 AND a.name = $12 AND a.type = 'number'
                              AND a.num BETWEEN $14::numeric AND $15::numeric)"
    ) => [TEXT, TEXT, TEXT, INT4, TEXT, TEXT_ARRAY, TEXT_ARRAY, TEXT_ARRAY, TEXT_ARRAY, TEXT_ARRAY, TEXT_ARRAY, TEXT, TEXT, TEXT, TEXT];
    // How many cells (up to 10,001) an argument's range holds, to choose the range scanned.
    SliceProbeText = "SELECT count(*) FROM (SELECT 1 FROM cell_args a
                       WHERE a.fn_hash = $1 AND a.name = $2 AND a.type = $3 AND a.value BETWEEN $4 AND $5 LIMIT 10001) x"
        => [TEXT, TEXT, TEXT, TEXT, TEXT];
    SliceProbeNum = "SELECT count(*) FROM (SELECT 1 FROM cell_args a
                      WHERE a.fn_hash = $1 AND a.name = $2 AND a.type = 'number' AND $3 = 'number'
                        AND a.num BETWEEN $4::numeric AND $5::numeric LIMIT 10001) x"
        => [TEXT, TEXT, TEXT, TEXT, TEXT];
}
