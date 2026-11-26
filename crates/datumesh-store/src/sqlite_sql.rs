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
                               WHERE v2.cell_id = c.cell_id AND coalesce(v2.recorded, v2.tx) <= :asof
                                 AND r2.hash = v2.receipt_hash AND r2.asof <= :asof
                               ORDER BY coalesce(v2.recorded, v2.tx) DESC LIMIT 1)
               AND r.hash = v.receipt_hash AND r.value_type <> '' AND ",
            slice_match!()
        )
    };
}

/// The latest withdrawal among the members of a slice as of `:asof` (0.6.0; SPEC 8.2): the
/// candidates of `$from` and `$where` whose answering version is a withdrawal; (tx, asof) of the
/// greatest, one row.
macro_rules! slice_withdrawn {
    ($from:expr, $where:expr) => {
        concat!(
            "SELECT v.tx, r.asof FROM ",
            $from,
            " CROSS JOIN versions v CROSS JOIN receipts r
             WHERE ",
            $where,
            " AND c.fn_hash = :fn_hash AND c.ns = :ns AND c.fn = :fn AND c.nargs = :nargs
               AND v.rowid = (SELECT v2.rowid FROM versions v2 CROSS JOIN receipts r2
                               WHERE v2.cell_id = c.cell_id AND coalesce(v2.recorded, v2.tx) <= :asof
                                 AND r2.hash = v2.receipt_hash AND r2.asof <= :asof
                               ORDER BY coalesce(v2.recorded, v2.tx) DESC LIMIT 1)
               AND r.hash = v.receipt_hash AND r.value_type = '' AND ",
            slice_match!(),
            " ORDER BY v.tx DESC, r.asof DESC LIMIT 1"
        )
    };
}

/// A member's columns: (tx, receipt_hash, asof, bytes, cell_id, arguments).
macro_rules! member_cols {
    () => {
        "v.tx, v.receipt_hash, r.asof, coalesce(v.recorded, v.tx), r.bytes, c.cell_id, (SELECT json_group_array(json_array(a.name, a.type, a.value))
                                                                FROM cell_args a WHERE a.cell_id = c.cell_id)"
    };
}

/// A member's value columns: (tx, receipt_hash, asof, value_type, value, cell_id,
/// arguments, ns, fn, fn_hash, sort_key).
macro_rules! value_cols {
    () => {
        "v.tx, v.receipt_hash, r.asof, coalesce(v.recorded, v.tx), r.value_type, r.value, c.cell_id, (SELECT json_group_array(json_array(a.name, a.type, a.value))
                                                                             FROM cell_args a WHERE a.cell_id = c.cell_id),
         c.ns, c.fn, c.fn_hash, c.sort_key"
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
               AND EXISTS (SELECT 1 FROM versions v CROSS JOIN receipts r ON r.hash = v.receipt_hash
                            WHERE v.cell_id = c.cell_id AND (coalesce(v.recorded, v.tx) > :asof OR r.asof > :asof))
               AND ",
            slice_match!(),
            " LIMIT 1"
        )
    };
}

/// The cells of a slice with a computed version answering as of `:asof` (tx and the receipt's
/// asof both at or before it), ascending by cell id, at most `:limit`. Columns: (ns, fn,
/// fn_hash, arguments).
macro_rules! computed_cells {
    ($from:expr, $where:expr) => {
        concat!(
            "SELECT c.ns, c.fn, c.fn_hash, (SELECT json_group_array(json_array(a.name, a.type, a.value))
                                              FROM cell_args a WHERE a.cell_id = c.cell_id)
              FROM ",
            $from,
            " WHERE ",
            $where,
            " AND c.fn_hash = :fn_hash AND c.ns = :ns AND c.fn = :fn AND c.nargs = :nargs
               AND EXISTS (SELECT 1 FROM computed_versions cv
                            WHERE cv.cell_id = c.cell_id AND cv.tx <= :asof AND cv.asof <= :asof)
               AND ",
            slice_match!(),
            " ORDER BY c.cell_id LIMIT :limit"
        )
    };
}

statements! {
    DescriptorGet = "SELECT bytes FROM descriptors WHERE fn_hash = ?1";
    DescriptorPut = "INSERT INTO descriptors (fn_hash, bytes) SELECT ?1, ?2
                      WHERE NOT EXISTS (SELECT 1 FROM descriptors WHERE fn_hash = ?1)";
    DescriptorExists = "SELECT EXISTS (SELECT 1 FROM descriptors WHERE fn_hash = ?1)";
    // The inputs of a descriptor, indexed by name (0.5.1; migration 6).
    DescriptorInputPut = "INSERT INTO descriptor_inputs (fn_hash, input_name) VALUES (?1, ?2) ON CONFLICT DO NOTHING";
    FunctionReaders = "SELECT fn_hash FROM descriptor_inputs WHERE input_name = ?1 ORDER BY fn_hash";
    BoundNames = "SELECT DISTINCT fn_name FROM name_bindings WHERE fn_hash = ?1 ORDER BY fn_name";

    // Function tests (0.5.1; migration 6): one row per (fn_hash, case).
    FunctionTestPut = r#"INSERT INTO function_tests (fn_hash, "case", test_json, signed_by, last_run_at, last_result,
                           last_detail, branches_total, branches_covered)
                         VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9) ON CONFLICT (fn_hash, "case") DO NOTHING"#;
    FunctionTestsOf = r#"SELECT t.fn_hash, t."case", t.test_json, t.signed_by, t.last_run_at, t.last_result, t.last_detail,
                           COALESCE((SELECT k.branches_total FROM function_test_counts k WHERE k.fn_hash = t.fn_hash LIMIT 1), 0),
                           COALESCE((SELECT k.branches_covered FROM function_test_counts k WHERE k.fn_hash = t.fn_hash LIMIT 1), 0)
                         FROM function_tests t WHERE t.fn_hash = ?1 ORDER BY t."case""#;
    FunctionTestRecord = r#"UPDATE function_tests SET last_run_at = ?1, last_result = ?2, last_detail = ?3
                            WHERE fn_hash = ?4 AND "case" = ?5"#;
    // The branch counts are per hash, on the hash's counter rows (0.5.3): none for a hash with no test.
    FunctionBranches = "UPDATE function_test_counts SET branches_total = ?2, branches_covered = ?3 WHERE fn_hash = ?1";
    // Named tests (0.5.2; migration 7): a name is set once, on the rows still unnamed. Returns
    // the rows named: the store reads the old result of the row first, for the counters.
    FunctionTestOld = r#"SELECT fn_name, last_result FROM function_tests WHERE fn_hash = ?1 AND "case" = ?2"#;
    FunctionTestName = r#"UPDATE function_tests SET fn_name = ?3 WHERE fn_hash = ?1 AND "case" = ?2 AND fn_name IS NULL"#;
    FunctionTestsUnnamed = r#"SELECT t.fn_hash, t."case", t.test_json, t.signed_by, t.last_run_at, t.last_result, t.last_detail,
                           COALESCE((SELECT k.branches_total FROM function_test_counts k WHERE k.fn_hash = t.fn_hash LIMIT 1), 0),
                           COALESCE((SELECT k.branches_covered FROM function_test_counts k WHERE k.fn_hash = t.fn_hash LIMIT 1), 0)
                         FROM function_tests t WHERE t.fn_name IS NULL ORDER BY t.fn_hash, t."case" LIMIT ?1"#;
    // The counters (0.5.3; migration 8): a row is made with zero counts and the hash's branch
    // counts (or the given ones), then moved by deltas.
    CounterEnsure = "INSERT INTO function_test_counts (fn_hash, fn_name, branches_total, branches_covered)
                     VALUES (?1, ?2,
                             COALESCE((SELECT k.branches_total FROM function_test_counts k WHERE k.fn_hash = ?1 LIMIT 1), ?3),
                             COALESCE((SELECT k.branches_covered FROM function_test_counts k WHERE k.fn_hash = ?1 LIMIT 1), ?4))
                     ON CONFLICT (fn_hash, fn_name) DO NOTHING";
    CounterAdd = "UPDATE function_test_counts SET n = n + ?3, pass = pass + ?4, fail = fail + ?5, error = error + ?6,
                    not_run = not_run + ?7,
                    last_run_at = CASE WHEN ?8 IS NULL THEN last_run_at
                                       WHEN last_run_at IS NULL OR ?8 > last_run_at THEN ?8 ELSE last_run_at END
                  WHERE fn_hash = ?1 AND fn_name = ?2";
    CounterRunRecompute = "UPDATE function_test_counts SET last_run_at = (SELECT max(t.last_run_at) FROM function_tests t
                             WHERE t.fn_hash = function_test_counts.fn_hash
                               AND COALESCE(t.fn_name, '') = function_test_counts.fn_name)
                           WHERE fn_hash = ?1 AND fn_name = ?2";
    // The summary of a hash's tests, all of them or those of one name: one aggregate of the counters.
    FunctionTestSummary = r#"SELECT COALESCE(SUM(n), 0), COALESCE(SUM(pass), 0), COALESCE(SUM(fail), 0),
                           COALESCE(SUM(error), 0), COALESCE(SUM(not_run), 0), max(last_run_at),
                           COALESCE(max(branches_total), 0), COALESCE(max(branches_covered), 0)
                         FROM function_test_counts WHERE fn_hash = ?1 AND (?2 IS NULL OR fn_name = ?2)"#;
    // A page of a hash's tests by case, after a case (exclusive), at most ?3 (or ?4) rows.
    FunctionTestsPage = r#"SELECT t.fn_hash, t."case", t.test_json, t.signed_by, t.last_run_at, t.last_result, t.last_detail,
                           COALESCE((SELECT k.branches_total FROM function_test_counts k WHERE k.fn_hash = t.fn_hash LIMIT 1), 0),
                           COALESCE((SELECT k.branches_covered FROM function_test_counts k WHERE k.fn_hash = t.fn_hash LIMIT 1), 0)
                         FROM function_tests t
                         WHERE t.fn_hash = ?1 AND (?2 IS NULL OR t."case" > ?2) ORDER BY t."case" LIMIT ?3"#;
    FunctionTestsPageNamed = r#"SELECT t.fn_hash, t."case", t.test_json, t.signed_by, t.last_run_at, t.last_result, t.last_detail,
                           COALESCE((SELECT k.branches_total FROM function_test_counts k WHERE k.fn_hash = t.fn_hash LIMIT 1), 0),
                           COALESCE((SELECT k.branches_covered FROM function_test_counts k WHERE k.fn_hash = t.fn_hash LIMIT 1), 0)
                         FROM function_tests t
                         WHERE t.fn_hash = ?1 AND t.fn_name = ?2 AND (?3 IS NULL OR t."case" > ?3)
                         ORDER BY t."case" LIMIT ?4"#;
    FunctionTestOne = r#"SELECT t.fn_hash, t."case", t.test_json, t.signed_by, t.last_run_at, t.last_result, t.last_detail,
                           COALESCE((SELECT k.branches_total FROM function_test_counts k WHERE k.fn_hash = t.fn_hash LIMIT 1), 0),
                           COALESCE((SELECT k.branches_covered FROM function_test_counts k WHERE k.fn_hash = t.fn_hash LIMIT 1), 0)
                         FROM function_tests t WHERE t.fn_hash = ?1 AND t."case" = ?2"#;
    FunctionTestsFailing = r#"SELECT t.fn_hash, t."case", t.test_json, t.signed_by, t.last_run_at, t.last_result, t.last_detail,
                           COALESCE((SELECT k.branches_total FROM function_test_counts k WHERE k.fn_hash = t.fn_hash LIMIT 1), 0),
                           COALESCE((SELECT k.branches_covered FROM function_test_counts k WHERE k.fn_hash = t.fn_hash LIMIT 1), 0)
                         FROM function_tests t
                         WHERE t.fn_hash = ?1 AND (?2 IS NULL OR t.fn_name = ?2) AND t.last_result IN ('fail', 'error')
                         ORDER BY t."case" LIMIT ?3"#;
    // The stored test cells whose function_hash argument is ?1 and whose latest version is false.
    FalseTestCells = "SELECT c.cell_id, r.bytes FROM cell_args a JOIN cells c ON c.cell_id = a.cell_id
                        JOIN (SELECT v.receipt_hash, v.cell_id FROM versions v
                               WHERE coalesce(v.recorded, v.tx) = (SELECT max(coalesce(w.recorded, w.tx)) FROM versions w WHERE w.cell_id = v.cell_id)) m
                          ON m.cell_id = c.cell_id
                        JOIN receipts r ON r.hash = m.receipt_hash
                       WHERE a.name = 'function_hash' AND a.type = 'string' AND a.value = ?1 AND c.fn = 'test_case'
                         AND r.value_type = 'boolean' AND r.value = 'false'
                       ORDER BY c.cell_id LIMIT ?2";
    // The names bound at ?2 to a hash that reads ?1: the latest binding of each name that was
    // bound to a reader (one query), at most ?3.
    DependantsOf = "WITH readers AS (SELECT fn_hash FROM descriptor_inputs WHERE input_name = ?1),
                      cand AS (SELECT DISTINCT fn_name FROM name_bindings
                                WHERE fn_hash IN (SELECT fn_hash FROM readers)),
                      latest AS (SELECT b.fn_name, b.fn_hash FROM name_bindings b
                                  WHERE b.fn_name IN (SELECT fn_name FROM cand) AND b.tx <= ?2
                                    AND b.tx = (SELECT max(w.tx) FROM name_bindings w
                                                 WHERE w.fn_name = b.fn_name AND w.tx <= ?2))
                    SELECT fn_name, fn_hash FROM latest WHERE fn_hash IN (SELECT fn_hash FROM readers)
                    ORDER BY fn_name LIMIT ?3";
    // The receipts of the latest stored version of the first cells (by cell id) of a function
    // name and hash that have one: at most ?4 cells read, one indexed query.
    StoredReceiptsOf = "SELECT r.bytes FROM (SELECT c.cell_id FROM cells c INDEXED BY cells_fn_hash
                          WHERE c.fn_hash = ?1 AND c.ns = ?2 AND c.fn = ?3
                            AND EXISTS (SELECT 1 FROM versions v WHERE v.cell_id = c.cell_id)
                          ORDER BY c.cell_id LIMIT ?4) k
                        JOIN receipts r ON r.hash = (SELECT v.receipt_hash FROM versions v WHERE v.cell_id = k.cell_id
                                                      ORDER BY coalesce(v.recorded, v.tx) DESC LIMIT 1)
                          AND r.value_type <> ''
                        ORDER BY k.cell_id";
    // The cells of a function name, from the counter (0.5.3; the primary key).
    StoredCellCount = "SELECT COALESCE((SELECT n FROM cell_counts WHERE fn_hash = ?1 AND ns = ?2 AND fn = ?3), 0)";
    // A version slice's versions, at most ?5 (SQL LIMIT, in tx order).
    VersionRangeLimit = "SELECT v.tx, v.receipt_hash, (SELECT r.asof FROM receipts r WHERE r.hash = v.receipt_hash), (SELECT r.value_type = '' FROM receipts r WHERE r.hash = v.receipt_hash), coalesce(v.recorded, v.tx)
                         FROM versions v WHERE v.cell_id = ?1 AND v.tx >= ?2 AND v.tx <= ?3 ORDER BY v.tx LIMIT ?4";
    // Dictionaries (0.5.2): exact lookups on the unique (owner, key, value).
    DictionaryHas = "SELECT EXISTS (SELECT 1 FROM key_values WHERE owner = ?1 AND key = ?2 AND value = ?3)";
    DictionaryCount = "SELECT count(*) FROM key_values WHERE owner = ?1 AND key = ?2";
    DictionaryValuesOrdered = "SELECT value FROM key_values WHERE owner = ?1 AND key = ?2 ORDER BY id LIMIT ?3";
    CatalogEntriesAll = "SELECT kind, name, label, description FROM catalog_entries ORDER BY id LIMIT ?1";
    KeyOpenPut = "INSERT INTO open_keys (owner, key, marked_at)
                  VALUES (?1, ?2, strftime('%Y-%m-%dT%H:%M:%f', 'now') || '000Z') ON CONFLICT (owner, key) DO NOTHING";
    KeyIsOpen = "SELECT EXISTS (SELECT 1 FROM open_keys WHERE owner = ?1 AND key = ?2)";
    OpenKeyCount = "SELECT count(*) FROM open_keys";
    // A catalogue entry by name alone: a name may be both a function's and a class's; the
    // function's first, then a method's, then a class's.
    CatalogEntryNamed = "SELECT kind, name, hash, label, description, args, examples, source, signed_by
                         FROM catalog_entries WHERE name = ?1
                         ORDER BY CASE kind WHEN 'function' THEN 0 WHEN 'method' THEN 1 ELSE 2 END LIMIT 1";

    ReceiptGet = "SELECT bytes FROM receipts WHERE hash = ?1";
    ReceiptMeta = "SELECT cell_id, tx, asof, value_type = '' FROM receipts WHERE hash = ?1";
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
    VersionLatestTx = "SELECT recorded FROM versions WHERE cell_id = ?1 ORDER BY recorded DESC LIMIT 1";
    VersionLatest = "SELECT v.tx, v.receipt_hash, (SELECT r.asof FROM receipts r WHERE r.hash = v.receipt_hash), (SELECT r.value_type = '' FROM receipts r WHERE r.hash = v.receipt_hash), coalesce(v.recorded, v.tx)
                     FROM versions v WHERE v.cell_id = ?1 ORDER BY coalesce(v.recorded, v.tx) DESC LIMIT 1";
    VersionAsof = "SELECT v.tx, v.receipt_hash, (SELECT r.asof FROM receipts r WHERE r.hash = v.receipt_hash), (SELECT r.value_type = '' FROM receipts r WHERE r.hash = v.receipt_hash), coalesce(v.recorded, v.tx)
                   FROM versions v WHERE v.cell_id = ?1 AND coalesce(v.recorded, v.tx) <= ?2 ORDER BY coalesce(v.recorded, v.tx) DESC LIMIT 1";
    VersionRange = "SELECT v.tx, v.receipt_hash, (SELECT r.asof FROM receipts r WHERE r.hash = v.receipt_hash), (SELECT r.value_type = '' FROM receipts r WHERE r.hash = v.receipt_hash), coalesce(v.recorded, v.tx)
                    FROM versions v WHERE v.cell_id = ?1 AND v.tx >= ?2 AND v.tx <= ?3 ORDER BY v.tx";
    // A stored version answers as of A when its tx <= A and its receipt's asof <= A.
    StoredAsof = "SELECT v.tx, v.receipt_hash, r.asof, r.value_type = '', coalesce(v.recorded, v.tx) FROM versions v CROSS JOIN receipts r
                  WHERE v.cell_id = ?1 AND coalesce(v.recorded, v.tx) <= ?2 AND r.hash = v.receipt_hash AND r.asof <= ?2
                  ORDER BY coalesce(v.recorded, v.tx) DESC LIMIT 1";
    StoredAsofReceipt = "SELECT v.tx, v.receipt_hash, r.asof, r.value_type = '', coalesce(v.recorded, v.tx), r.bytes FROM versions v CROSS JOIN receipts r
                         WHERE v.cell_id = ?1 AND coalesce(v.recorded, v.tx) <= ?2 AND r.hash = v.receipt_hash AND r.asof <= ?2
                         ORDER BY coalesce(v.recorded, v.tx) DESC LIMIT 1";
    VersionPut = "INSERT INTO versions (cell_id, tx, receipt_hash, recorded) VALUES (?1, ?2, ?3, ?4)";
    // The member-set rows (0.6.5, G2) and the slice definitions (G10): inserted or nothing (the
    // insert selects no row when the key is held, so the append-only trigger never fires).
    MemberSetPut = "INSERT INTO member_sets (entry, slice_id, members_hash, fn_hash, slice, members, tx, asof, licences)
                    SELECT ?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9 WHERE NOT EXISTS (SELECT 1 FROM member_sets WHERE entry = ?1)";
    MemberSetGet = "SELECT slice_id, members_hash, fn_hash, slice, members, tx, asof, licences FROM member_sets WHERE entry = ?1";
    SliceDefinitionPut = "INSERT INTO slice_definitions (slice_id, fn_hash, slice) SELECT ?1, ?2, ?3
                          WHERE NOT EXISTS (SELECT 1 FROM slice_definitions WHERE slice_id = ?1)";
    SliceDefinitionGet = "SELECT slice FROM slice_definitions WHERE slice_id = ?1";
    // The Resolver's evaluate of a stored function: the receipt of stored_asof's version.
    ValueAsof = "SELECT r.bytes FROM versions v CROSS JOIN receipts r
                 WHERE v.cell_id = ?1 AND coalesce(v.recorded, v.tx) <= ?2 AND r.hash = v.receipt_hash AND r.asof <= ?2
                 ORDER BY coalesce(v.recorded, v.tx) DESC LIMIT 1";

    ComputedPut = "INSERT INTO computed_versions (cell_id, tx, asof, receipt_hash) VALUES (?1, ?2, ?3, ?4)";
    ComputedGet = "SELECT receipt_hash FROM computed_versions WHERE cell_id = ?1 AND tx = ?2 AND asof = ?3";
    // The greatest asof <= A, ties the greatest tx.
    ComputedAsof = "SELECT tx, receipt_hash, asof, 0, tx FROM computed_versions
                    WHERE cell_id = ?1 AND asof <= ?2 ORDER BY asof DESC, tx DESC LIMIT 1";
    ComputedAsofReceipt = "SELECT c.tx, c.receipt_hash, c.asof, 0, c.tx, r.bytes FROM
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

    NamespaceGet = "SELECT keydoc, validated_at, suspended_at, consecutive_failures, last_checked_at
                    FROM namespaces WHERE host = ?1";
    NamespaceList = "SELECT host, keydoc, validated_at, suspended_at, consecutive_failures, last_checked_at
                     FROM namespaces ORDER BY host";
    NamespaceCheck = "UPDATE namespaces SET consecutive_failures = ?2, last_checked_at = ?3, suspended_at = ?4
                      WHERE host = ?1";
    NamespacePut = "INSERT INTO namespaces (host, keydoc, validated_at) VALUES (?1, ?2, ?3)
                    ON CONFLICT (host) DO UPDATE SET keydoc = excluded.keydoc, validated_at = excluded.validated_at";

    NameLatest = "SELECT tx FROM name_bindings WHERE fn_name = ?1 ORDER BY tx DESC LIMIT 1";
    // The latest binding with its hash (NULL when the name is closed, 0.6.0).
    NameLatestRow = "SELECT tx, fn_hash FROM name_bindings WHERE fn_name = ?1 ORDER BY tx DESC LIMIT 1";
    NameClose = "INSERT INTO name_bindings (fn_name, tx, fn_hash, written_at)
                 VALUES (?1, ?2, NULL, strftime('%Y-%m-%dT%H:%M:%fZ', 'now'))";
    NamePut = "INSERT INTO name_bindings (fn_name, tx, fn_hash, written_at)
               VALUES (?1, ?2, ?3, strftime('%Y-%m-%dT%H:%M:%fZ', 'now'))";
    NameAsof = "SELECT fn_hash FROM name_bindings WHERE fn_name = ?1 AND tx <= ?2 ORDER BY tx DESC LIMIT 1";
    BindingAsof = "SELECT fn_hash, tx FROM name_bindings WHERE fn_name = ?1 AND tx <= ?2 ORDER BY tx DESC LIMIT 1";
    // Whether a name is bound as a function, or as a class (any binding): one namespace of names.
    NameBound = "SELECT EXISTS (SELECT 1 FROM name_bindings WHERE fn_name = ?1)";
    ClassNameBound = "SELECT EXISTS (SELECT 1 FROM class_bindings WHERE class_name = ?1)";

    // Classes: flattened class descriptors by class hash, and class-name bindings.
    ClassPut = "INSERT INTO classes (class_hash, bytes) SELECT ?1, ?2
                 WHERE NOT EXISTS (SELECT 1 FROM classes WHERE class_hash = ?1)";
    ClassGet = "SELECT bytes FROM classes WHERE class_hash = ?1";
    ClassBindingLatest = "SELECT tx FROM class_bindings WHERE class_name = ?1 ORDER BY tx DESC LIMIT 1";
    ClassBindingLatestRow = "SELECT tx, class_hash FROM class_bindings WHERE class_name = ?1 ORDER BY tx DESC LIMIT 1";
    ClassBindingClose = "INSERT INTO class_bindings (class_name, tx, class_hash, written_at)
                         VALUES (?1, ?2, NULL, strftime('%Y-%m-%dT%H:%M:%fZ', 'now'))";
    ClassBindingPut = "INSERT INTO class_bindings (class_name, tx, class_hash, written_at)
                       VALUES (?1, ?2, ?3, strftime('%Y-%m-%dT%H:%M:%fZ', 'now'))";
    ClassBindingAsof = "SELECT class_hash, tx FROM class_bindings WHERE class_name = ?1 AND tx <= ?2 ORDER BY tx DESC LIMIT 1";
    // The class names bound to a hash as of an instant (0.5.3): each name's latest binding with
    // tx <= ?2 must be to the hash.
    ClassNamesBound = "SELECT DISTINCT b.class_name FROM class_bindings b
                       WHERE b.class_hash = ?1 AND b.tx <= ?2
                         AND b.tx = (SELECT max(c.tx) FROM class_bindings c WHERE c.class_name = b.class_name AND c.tx <= ?2)
                       ORDER BY b.class_name";

    ManifestExists = "SELECT EXISTS (SELECT 1 FROM manifests WHERE entry = ?1)";
    ManifestPut = "INSERT INTO manifests (entry, slice_id, members_hash, fn_hash, members, bytes)
                   VALUES (?1, ?2, ?3, ?4, ?5, ?6)";
    ManifestMemberPut = "INSERT INTO manifest_members (entry, member_hash) VALUES (?1, ?2)";
    ManifestGet = "SELECT bytes FROM manifests WHERE entry = ?1";
    MemberManifests = "SELECT entry FROM manifest_members WHERE member_hash = ?1 ORDER BY entry";
    SliceEntries = "SELECT entry FROM slice_edges WHERE receipt_hash = ?1 ORDER BY entry";
    SliceDependents = "SELECT receipt_hash FROM slice_edges WHERE entry = ?1 ORDER BY receipt_hash";

    // A version slice: the stored versions with from <= tx <= to answering as of A.
    VersionsBetween = "SELECT v.tx, v.receipt_hash, r.asof, r.value_type = '', coalesce(v.recorded, v.tx), r.bytes FROM versions v CROSS JOIN receipts r
                       WHERE v.cell_id = ?1 AND v.tx >= ?2 AND v.tx <= ?3 AND coalesce(v.recorded, v.tx) <= ?4
                         AND r.hash = v.receipt_hash AND r.asof <= ?4 ORDER BY v.tx";
    // Its values: (tx, receipt_hash, asof, recorded, value_type, value, cell_id, arguments, ns, fn,
    // fn_hash, sort_key).
    VersionsBetweenValues = "SELECT v.tx, v.receipt_hash, r.asof, coalesce(v.recorded, v.tx), r.value_type, r.value, c.cell_id,
                                    (SELECT json_group_array(json_array(a.name, a.type, a.value))
                                       FROM cell_args a WHERE a.cell_id = c.cell_id),
                                    c.ns, c.fn, c.fn_hash, c.sort_key
                               FROM versions v CROSS JOIN receipts r CROSS JOIN cells c
                              WHERE v.cell_id = ?1 AND v.tx >= ?2 AND v.tx <= ?3 AND coalesce(v.recorded, v.tx) <= ?4
                                AND r.hash = v.receipt_hash AND r.asof <= ?4
                                AND r.value_type <> '' AND c.cell_id = v.cell_id ORDER BY v.tx";
    VersionsChangedAfter = "SELECT EXISTS (SELECT 1 FROM versions v CROSS JOIN receipts r
                             WHERE v.cell_id = ?1 AND v.tx >= ?2 AND v.tx <= ?3 AND r.hash = v.receipt_hash AND (coalesce(v.recorded, v.tx) > ?4 OR r.asof > ?4))";

    // The members of a slice of cells, and their values: from every cell of the function,
    // or from a range of one argument of the typed-argument index.
    // The latest withdrawal among a slice's members as of :asof (0.6.0; SPEC 8.2): (tx, asof).
    SliceWithdrawnAll = slice_withdrawn!(from_all!(), where_all!());
    SliceWithdrawnDrive = slice_withdrawn!(from_drive!(), where_drive!());
    SliceMembersAll = slice_members!(member_cols!(), from_all!(), where_all!());
    SliceMembersDrive = slice_members!(member_cols!(), from_drive!(), where_drive!());
    SliceValuesAll = slice_members!(value_cols!(), from_all!(), where_all!());
    SliceValuesDrive = slice_members!(value_cols!(), from_drive!(), where_drive!());
    // A page of a slice in key order (CONTRACT-0.5.1 S2): the candidates of the function's
    // index on (fn_hash, ns, fn, sort_key); the caller appends the key range, the cursor and the
    // ORDER BY ... LIMIT.
    SliceKeyBase = slice_members!(value_cols!(), "cells c INDEXED BY cells_sort_key", where_all!());
    // How many cells (up to 10,001) an argument's range holds, to choose the range scanned.
    SliceProbe = "SELECT count(*) FROM (SELECT 1 FROM cell_args
                   WHERE fn_hash = ?1 AND name = ?2 AND type = ?3 AND sort_key BETWEEN ?4 AND ?5 LIMIT 10001)";
    // Whether a matching cell has a stored version whose receipt's asof is after A: from
    // the function's receipts with an asof after A, then each one's cell against the slice,
    // Two branches, each stopping at its first match: a version recorded after A of a matching cell
    // (versions_recorded_all), or a receipt of the function with an asof after A and its version
    // (receipts_fn_hash_asof), of a matching cell.
    ChangedRecent = concat!(
        "SELECT x FROM (SELECT 1 AS x FROM versions v INDEXED BY versions_recorded_all JOIN cells c ON c.cell_id = v.cell_id
           WHERE coalesce(v.recorded, v.tx) > :asof AND c.fn_hash = :fn_hash AND c.ns = :ns AND c.fn = :fn AND c.nargs = :nargs AND ",
        slice_match!(),
        " LIMIT 1)
          UNION ALL
         SELECT x FROM (SELECT 1 AS x FROM receipts r INDEXED BY receipts_fn_hash_asof JOIN versions v ON v.receipt_hash = r.hash
                      JOIN cells c ON c.cell_id = v.cell_id
                     WHERE r.fn_hash = :fn_hash AND r.asof > :asof AND c.fn_hash = :fn_hash AND c.ns = :ns AND c.fn = :fn AND c.nargs = :nargs AND ",
        slice_match!(),
        " LIMIT 1)"
    );
    // The 0.6.4 statements, for a store whose schema is older than migration 17 (decision B).
    HasRecorded = "SELECT count(*) FROM pragma_table_info('versions') WHERE name = 'recorded'";
    VersionLatestTxLegacy = "SELECT tx FROM versions WHERE cell_id = ?1 ORDER BY tx DESC LIMIT 1";
    VersionPutLegacy = "INSERT INTO versions (cell_id, tx, receipt_hash) VALUES (?1, ?2, ?3)";
    // or from the slice's candidate cells, then each one's receipts by cell and asof.
    ChangedCandAll = changed_cand!(from_all!(), where_all!());
    ChangedCandDrive = changed_cand!(from_drive!(), where_drive!());
    // computed_cells (0.4.2): from every cell of the function, or from a range of one argument.
    ComputedCellsAll = computed_cells!(from_all!(), where_all!());
    ComputedCellsDrive = computed_cells!(from_drive!(), where_drive!());
    // has_bindings_under (0.4.2): a name binding or class binding under ?1 (a namespace and "/").
    HasBindingsUnder = "SELECT EXISTS (SELECT 1 FROM name_bindings WHERE substr(fn_name, 1, length(?1)) = ?1)
                           OR EXISTS (SELECT 1 FROM class_bindings WHERE substr(class_name, 1, length(?1)) = ?1)";

    // How many receipts of the function have an asof after A, and how many cells it has
    // (up to 10,001), to choose between the two.
    ProbeRecent = "SELECT count(*) FROM (SELECT 1 FROM receipts WHERE fn_hash = ?1 AND asof > ?2 LIMIT 10001)";
    ProbeCells = "SELECT count(*) FROM (SELECT 1 FROM cells WHERE fn_hash = ?1 LIMIT 10001)";
}
