//! Every SQL statement `SqliteStore` runs. `SqliteStore::check_statements` prepares all of
//! them against the live schema (rusqlite has no compile-time query checking either).
//!
//! The slice statements take named parameters: `:fn_hash`, `:ns`, `:fn`, `:nargs`, `:asof`,
//! `:cons` (the slice's constraints, a JSON array of `[name, kind, type, lo, hi]`, a
//! number's `lo` and `hi` as sort keys, a bound value's lexical form as its `lo`), and for
//! a driven query the range of the typed-argument index the candidates come from,
//! `:d_name`, `:d_type`, `:d_lo`, `:d_hi` (sort keys). The others take `?1`, `?2`, ...
//!
//! Joins are written `CROSS JOIN`, which SQLite never reorders, and the candidates' index
//! is named (`INDEXED BY`, which also makes a statement fail to prepare when the index is
//! missing): the candidates come first, from an index range of one argument or from the
//! function's cells, and each candidate's version and receipt are looked up by key,
//! whatever the planner's statistics say.

macro_rules! statements {
    ($($name:ident = $sql:expr;)*) => {
        /// A statement.
        #[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
        pub(crate) enum L {
            $($name,)*
        }

        impl L {
            pub(crate) const ALL: &'static [L] = &[$(L::$name,)*];

            pub(crate) fn sql(self) -> &'static str {
                match self {
                    $(L::$name => $sql,)*
                }
            }
        }
    };
}

/// Whether cell `c` meets every constraint of the slice (`:cons`).
macro_rules! slice_match {
    () => {
        "NOT EXISTS (
            SELECT 1 FROM json_each(:cons) k
             WHERE NOT EXISTS (
                 SELECT 1 FROM cell_args a
                  WHERE a.cell_id = c.cell_id AND a.name = k.value ->> 0
                    AND CASE k.value ->> 1 WHEN 'open' THEN 1
                             WHEN 'value' THEN a.type = k.value ->> 2 AND a.value = k.value ->> 3
                             ELSE a.type = k.value ->> 2 AND a.sort_key BETWEEN k.value ->> 3 AND k.value ->> 4 END))"
    };
}

/// The candidate cells: every cell of the function.
macro_rules! from_all {
    () => {
        "cells c INDEXED BY cells_fn_hash"
    };
}

/// The candidate cells: those with the driving argument in its range.
macro_rules! from_drive {
    () => {
        "cell_args d INDEXED BY cell_args_key CROSS JOIN cells c"
    };
}

macro_rules! where_all {
    () => {
        "c.fn_hash = :fn_hash"
    };
}

macro_rules! where_drive {
    () => {
        "d.fn_hash = :fn_hash AND d.name = :d_name AND d.type = :d_type AND d.sort_key BETWEEN :d_lo AND :d_hi
         AND c.cell_id = d.cell_id"
    };
}

/// The members of a slice of cells as of `:asof`: the columns `$cols` of each candidate
/// (`c`) meeting the slice, with the stored version answering (`v`) and its receipt (`r`).
macro_rules! slice_members {
    ($cols:expr, $from:expr, $where:expr) => {
        concat!(
            "SELECT ",
            $cols,
            " FROM ",
            $from,
            " CROSS JOIN versions v CROSS JOIN receipts r
             WHERE ",
            $where,
            " AND c.fn_hash = :fn_hash AND c.ns = :ns AND c.fn = :fn AND c.nargs = :nargs
               AND v.rowid = (SELECT v2.rowid FROM versions v2 CROSS JOIN receipts r2
                               WHERE v2.cell_id = c.cell_id AND v2.tx <= :asof
                                 AND r2.hash = v2.receipt_hash AND r2.asof <= :asof
                               ORDER BY v2.tx DESC LIMIT 1)
               AND r.hash = v.receipt_hash AND ",
            slice_match!()
        )
    };
}

/// A member's columns: (tx, receipt_hash, asof, bytes, cell_id, arguments).
macro_rules! member_cols {
    () => {
        "v.tx, v.receipt_hash, r.asof, r.bytes, c.cell_id, (SELECT json_group_array(json_array(a.name, a.type, a.value))
                                                                FROM cell_args a WHERE a.cell_id = c.cell_id)"
    };
}

/// A member's value columns: (tx, receipt_hash, asof, value_type, value, cell_id,
/// arguments, ns, fn, fn_hash).
macro_rules! value_cols {
    () => {
        "v.tx, v.receipt_hash, r.asof, r.value_type, r.value, c.cell_id, (SELECT json_group_array(json_array(a.name, a.type, a.value))
                                                                             FROM cell_args a WHERE a.cell_id = c.cell_id),
         c.ns, c.fn, c.fn_hash"
    };
}

/// Whether a candidate of a slice has a stored version whose receipt's asof is after A.
macro_rules! changed_cand {
    ($from:expr, $where:expr) => {
        concat!(
            "SELECT 1 FROM ",
            $from,
            " WHERE ",
            $where,
            " AND c.fn_hash = :fn_hash AND c.ns = :ns AND c.fn = :fn AND c.nargs = :nargs
               AND EXISTS (SELECT 1 FROM receipts r WHERE r.cell_id = c.cell_id AND r.asof > :asof
                              AND EXISTS (SELECT 1 FROM versions v WHERE v.receipt_hash = r.hash))
               AND ",
            slice_match!(),
            " LIMIT 1"
        )
    };
}

statements! {
    DescriptorGet = "SELECT bytes FROM descriptors WHERE fn_hash = ?1";
    DescriptorPut = "INSERT INTO descriptors (fn_hash, bytes) SELECT ?1, ?2
                      WHERE NOT EXISTS (SELECT 1 FROM descriptors WHERE fn_hash = ?1)";
    DescriptorExists = "SELECT EXISTS (SELECT 1 FROM descriptors WHERE fn_hash = ?1)";

    ReceiptGet = "SELECT bytes FROM receipts WHERE hash = ?1";
    ReceiptMeta = "SELECT cell_id, tx, asof FROM receipts WHERE hash = ?1";
    ReceiptPut = "INSERT INTO receipts (hash, bytes, cell_id, tx, fn_hash, publisher, asof, value_type, value)
                  VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9)";
    EdgePut = "INSERT INTO edges (receipt_hash, input_hash, kind) SELECT ?1, ?2, 'receipt'
                WHERE NOT EXISTS (SELECT 1 FROM edges WHERE receipt_hash = ?1 AND input_hash = ?2)";
    SliceEdgePut = "INSERT INTO slice_edges (receipt_hash, entry) SELECT ?1, ?2
                     WHERE NOT EXISTS (SELECT 1 FROM slice_edges WHERE receipt_hash = ?1 AND entry = ?2)";
    // The typed-argument index of a receipt's cell, once per cell.
    CellExists = "SELECT EXISTS (SELECT 1 FROM cells WHERE cell_id = ?1)";
    CellPut = "INSERT INTO cells (cell_id, fn_hash, ns, fn, nargs) VALUES (?1, ?2, ?3, ?4, ?5)";
    CellArgPut = "INSERT INTO cell_args (cell_id, name, type, value, sort_key, fn_hash) VALUES (?1, ?2, ?3, ?4, ?5, ?6)";

    BlobGet = "SELECT bytes FROM blobs WHERE sha256 = ?1";
    BlobExists = "SELECT EXISTS (SELECT 1 FROM blobs WHERE sha256 = ?1)";
    BlobPut = "INSERT INTO blobs (sha256, bytes, length) SELECT ?1, ?2, ?3
                WHERE NOT EXISTS (SELECT 1 FROM blobs WHERE sha256 = ?1)";

    // Versions come with their receipt's asof (Version::asof).
    VersionLatestTx = "SELECT tx FROM versions WHERE cell_id = ?1 ORDER BY tx DESC LIMIT 1";
    VersionLatest = "SELECT v.tx, v.receipt_hash, (SELECT r.asof FROM receipts r WHERE r.hash = v.receipt_hash)
                     FROM versions v WHERE v.cell_id = ?1 ORDER BY v.tx DESC LIMIT 1";
    VersionAsof = "SELECT v.tx, v.receipt_hash, (SELECT r.asof FROM receipts r WHERE r.hash = v.receipt_hash)
                   FROM versions v WHERE v.cell_id = ?1 AND v.tx <= ?2 ORDER BY v.tx DESC LIMIT 1";
    VersionRange = "SELECT v.tx, v.receipt_hash, (SELECT r.asof FROM receipts r WHERE r.hash = v.receipt_hash)
                    FROM versions v WHERE v.cell_id = ?1 AND v.tx >= ?2 AND v.tx <= ?3 ORDER BY v.tx";
    // A stored version answers as of A when its tx <= A and its receipt's asof <= A.
    StoredAsof = "SELECT v.tx, v.receipt_hash, r.asof FROM versions v CROSS JOIN receipts r
                  WHERE v.cell_id = ?1 AND v.tx <= ?2 AND r.hash = v.receipt_hash AND r.asof <= ?2
                  ORDER BY v.tx DESC LIMIT 1";
    StoredAsofReceipt = "SELECT v.tx, v.receipt_hash, r.asof, r.bytes FROM versions v CROSS JOIN receipts r
                         WHERE v.cell_id = ?1 AND v.tx <= ?2 AND r.hash = v.receipt_hash AND r.asof <= ?2
                         ORDER BY v.tx DESC LIMIT 1";
    VersionPut = "INSERT INTO versions (cell_id, tx, receipt_hash) VALUES (?1, ?2, ?3)";
    // The Resolver's evaluate of a stored function: the receipt of stored_asof's version.
    ValueAsof = "SELECT r.bytes FROM versions v CROSS JOIN receipts r
                 WHERE v.cell_id = ?1 AND v.tx <= ?2 AND r.hash = v.receipt_hash AND r.asof <= ?2
                 ORDER BY v.tx DESC LIMIT 1";

    ComputedPut = "INSERT INTO computed_versions (cell_id, tx, asof, receipt_hash) VALUES (?1, ?2, ?3, ?4)";
    ComputedGet = "SELECT receipt_hash FROM computed_versions WHERE cell_id = ?1 AND tx = ?2 AND asof = ?3";
    // The greatest asof <= A, ties the greatest tx.
    ComputedAsof = "SELECT tx, receipt_hash, asof FROM computed_versions
                    WHERE cell_id = ?1 AND asof <= ?2 ORDER BY asof DESC, tx DESC LIMIT 1";
    ComputedAsofReceipt = "SELECT c.tx, c.receipt_hash, c.asof, r.bytes FROM
                               (SELECT tx, receipt_hash, asof FROM computed_versions
                                 WHERE cell_id = ?1 AND asof <= ?2 ORDER BY asof DESC, tx DESC LIMIT 1) c
                           CROSS JOIN receipts r WHERE r.hash = c.receipt_hash";

    // The cells of stored receipts that list any stored receipt of the cell among their
    // inputs, or a slice entry whose stored manifest lists one.
    CellDependents = "SELECT d.cell_id FROM receipts i
                      JOIN edges e ON e.input_hash = i.hash
                      JOIN receipts d ON d.hash = e.receipt_hash
                      WHERE i.cell_id = ?1
                      UNION
                      SELECT d.cell_id FROM receipts i
                      JOIN manifest_members mm ON mm.member_hash = i.hash
                      JOIN slice_edges se ON se.entry = mm.entry
                      JOIN receipts d ON d.hash = se.receipt_hash
                      WHERE i.cell_id = ?1
                      ORDER BY 1";

    Dependents = "SELECT receipt_hash FROM edges WHERE input_hash = ?1 ORDER BY receipt_hash";
    Inputs = "SELECT input_hash FROM edges WHERE receipt_hash = ?1 ORDER BY input_hash";

    NamespaceGet = "SELECT keydoc, validated_at, suspended_at FROM namespaces WHERE host = ?1";
    NamespacePut = "INSERT INTO namespaces (host, keydoc, validated_at) VALUES (?1, ?2, ?3)
                    ON CONFLICT (host) DO UPDATE SET keydoc = excluded.keydoc, validated_at = excluded.validated_at";

    NameLatest = "SELECT tx FROM name_bindings WHERE fn_name = ?1 ORDER BY tx DESC LIMIT 1";
    NamePut = "INSERT INTO name_bindings (fn_name, tx, fn_hash) VALUES (?1, ?2, ?3)";
    NameAsof = "SELECT fn_hash FROM name_bindings WHERE fn_name = ?1 AND tx <= ?2 ORDER BY tx DESC LIMIT 1";
    BindingAsof = "SELECT fn_hash, tx FROM name_bindings WHERE fn_name = ?1 AND tx <= ?2 ORDER BY tx DESC LIMIT 1";

    ManifestExists = "SELECT EXISTS (SELECT 1 FROM manifests WHERE entry = ?1)";
    ManifestPut = "INSERT INTO manifests (entry, slice_id, members_hash, fn_hash, members, bytes)
                   VALUES (?1, ?2, ?3, ?4, ?5, ?6)";
    ManifestMemberPut = "INSERT INTO manifest_members (entry, member_hash) VALUES (?1, ?2)";
    ManifestGet = "SELECT bytes FROM manifests WHERE entry = ?1";
    MemberManifests = "SELECT entry FROM manifest_members WHERE member_hash = ?1 ORDER BY entry";
    SliceEntries = "SELECT entry FROM slice_edges WHERE receipt_hash = ?1 ORDER BY entry";
    SliceDependents = "SELECT receipt_hash FROM slice_edges WHERE entry = ?1 ORDER BY receipt_hash";

    // A version slice: the stored versions with from <= tx <= to answering as of A.
    VersionsBetween = "SELECT v.tx, v.receipt_hash, r.asof, r.bytes FROM versions v CROSS JOIN receipts r
                       WHERE v.cell_id = ?1 AND v.tx >= ?2 AND v.tx <= ?3 AND v.tx <= ?4
                         AND r.hash = v.receipt_hash AND r.asof <= ?4 ORDER BY v.tx";
    // Its values: (tx, receipt_hash, asof, value_type, value, cell_id, arguments, ns, fn, fn_hash).
    VersionsBetweenValues = "SELECT v.tx, v.receipt_hash, r.asof, r.value_type, r.value, c.cell_id,
                                    (SELECT json_group_array(json_array(a.name, a.type, a.value))
                                       FROM cell_args a WHERE a.cell_id = c.cell_id),
                                    c.ns, c.fn, c.fn_hash
                               FROM versions v CROSS JOIN receipts r CROSS JOIN cells c
                              WHERE v.cell_id = ?1 AND v.tx >= ?2 AND v.tx <= ?3 AND v.tx <= ?4
                                AND r.hash = v.receipt_hash AND r.asof <= ?4 AND c.cell_id = v.cell_id ORDER BY v.tx";
    VersionsChangedAfter = "SELECT EXISTS (SELECT 1 FROM versions v CROSS JOIN receipts r
                             WHERE v.cell_id = ?1 AND v.tx >= ?2 AND v.tx <= ?3 AND r.hash = v.receipt_hash AND r.asof > ?4)";

    // The members of a slice of cells, and their values: from every cell of the function,
    // or from a range of one argument of the typed-argument index.
    SliceMembersAll = slice_members!(member_cols!(), from_all!(), where_all!());
    SliceMembersDrive = slice_members!(member_cols!(), from_drive!(), where_drive!());
    SliceValuesAll = slice_members!(value_cols!(), from_all!(), where_all!());
    SliceValuesDrive = slice_members!(value_cols!(), from_drive!(), where_drive!());
    // How many cells (up to 10,001) an argument's range holds, to choose the range scanned.
    SliceProbe = "SELECT count(*) FROM (SELECT 1 FROM cell_args
                   WHERE fn_hash = ?1 AND name = ?2 AND type = ?3 AND sort_key BETWEEN ?4 AND ?5 LIMIT 10001)";
    // Whether a matching cell has a stored version whose receipt's asof is after A: from
    // the function's receipts with an asof after A, then each one's cell against the slice,
    ChangedRecent = concat!(
        "SELECT 1 FROM receipts r INDEXED BY receipts_fn_hash_asof CROSS JOIN cells c
          WHERE r.fn_hash = :fn_hash AND r.asof > :asof AND c.cell_id = r.cell_id
            AND c.ns = :ns AND c.fn = :fn AND c.nargs = :nargs
            AND EXISTS (SELECT 1 FROM versions v WHERE v.receipt_hash = r.hash) AND ",
        slice_match!(),
        " LIMIT 1"
    );
    // or from the slice's candidate cells, then each one's receipts by cell and asof.
    ChangedCandAll = changed_cand!(from_all!(), where_all!());
    ChangedCandDrive = changed_cand!(from_drive!(), where_drive!());
    // How many receipts of the function have an asof after A, and how many cells it has
    // (up to 10,001), to choose between the two.
    ProbeRecent = "SELECT count(*) FROM (SELECT 1 FROM receipts WHERE fn_hash = ?1 AND asof > ?2 LIMIT 10001)";
    ProbeCells = "SELECT count(*) FROM (SELECT 1 FROM cells WHERE fn_hash = ?1 LIMIT 10001)";
}
