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

/// The value-bound constraints ($6-$10: name, kind open / value / range, type, lo, hi; a
/// value's lexical form is its lo) of the cell `$cell` (a column reference): each one an
/// index-only probe of `cell_args_value` (fn_hash, name, type, value, cell_id) with the
/// function hash $1.
macro_rules! value_probes {
    ($cell:literal) => {
        concat!(
            "NOT EXISTS (
            SELECT 1 FROM unnest($6::text[], $7::text[], $8::text[], $9::text[], $10::text[]) AS k(name, kind, type, lo, hi)
             WHERE k.kind = 'value' AND NOT EXISTS (
                 SELECT 1 FROM cell_args e
                  WHERE e.fn_hash = $1 AND e.name = k.name AND e.type = k.type AND e.value = k.lo
                    AND e.cell_id = ",
            $cell,
            "))"
        )
    };
}

/// Whether cell `c` meets every constraint of a slice: the value-bound ones by `value_probes!`,
/// the open and range ones by a scan of the cell's arguments.
macro_rules! slice_match {
    () => {
        concat!(
            value_probes!("c.cell_id"),
            " AND NOT EXISTS (
            SELECT 1 FROM unnest($6::text[], $7::text[], $8::text[], $9::text[], $10::text[]) AS k(name, kind, type, lo, hi)
             WHERE k.kind <> 'value' AND NOT EXISTS (
                 SELECT 1 FROM cell_args a
                  WHERE a.cell_id = c.cell_id AND a.name = k.name
                    AND CASE WHEN k.kind = 'open' THEN true
                             WHEN k.type = 'number' THEN a.type = 'number' AND a.num BETWEEN k.lo::numeric AND k.hi::numeric
                             ELSE a.type = k.type AND a.value BETWEEN k.lo AND k.hi END))"
        )
    };
}

/// The statement of the members of a slice of cells: the columns `$outer` of the version
/// answering (`$inner` of the receipt), the cell id, and its key; the candidates from `$with`
/// (a CTE, or nothing) and `$from`.
macro_rules! slice_members {
    ($with:expr, $from:expr, $inner:literal, $outer:literal) => {
        concat!(
            $with,
            "SELECT m.tx, m.receipt_hash, m.asof, m.recorded, ",
            $outer,
            ", c.cell_id,
                    ARRAY(SELECT a.type || ':' || a.value FROM cell_args a
                           WHERE a.cell_id = c.cell_id AND a.name = ANY($11::text[]) ORDER BY a.name)
               ",
            $from,
            "
              CROSS JOIN LATERAL (SELECT v.tx, v.receipt_hash, r.asof, r.value_type = '' AS withdrawn, v.recorded, ",
            $inner,
            "
                                    FROM versions v JOIN receipts r ON r.hash = v.receipt_hash
                                   WHERE v.cell_id = c.cell_id AND v.recorded <= $5 AND r.asof <= $5
                                   ORDER BY v.recorded DESC LIMIT 1) m
              WHERE c.fn_hash = $1 AND c.ns = $2 AND c.fn = $3 AND c.nargs = $4 AND ",
            slice_match!(),
            " AND NOT m.withdrawn"
        )
    };
}

/// The latest withdrawal among the members of a slice of cells as of $5 (SPEC 8.2, 0.6.0): the
/// candidates from `$with` and `$from`, whose version answering as of $5 (the answering rule of
/// `stored_asof`) is a withdrawal; the (tx, asof) of the greatest such version, one row.
macro_rules! slice_withdrawn {
    ($with:expr, $from:expr) => {
        concat!(
            $with,
            "SELECT m.tx, m.asof ",
            $from,
            "
              CROSS JOIN LATERAL (SELECT v.tx, r.asof, r.value_type = '' AS withdrawn
                                    FROM versions v JOIN receipts r ON r.hash = v.receipt_hash
                                   WHERE v.cell_id = c.cell_id AND v.recorded <= $5 AND r.asof <= $5
                                   ORDER BY v.recorded DESC LIMIT 1) m
              WHERE c.fn_hash = $1 AND c.ns = $2 AND c.fn = $3 AND c.nargs = $4 AND m.withdrawn AND ",
            slice_match!(),
            " ORDER BY m.tx DESC, m.asof DESC LIMIT 1"
        )
    };
}

/// Whether a candidate cell of a slice (from `$with` and `$from`) has a stored version whose
/// receipt's asof is after A.
macro_rules! changed_cand {
    ($with:expr, $from:expr) => {
        concat!(
            $with,
            "SELECT true ",
            $from,
            "
              WHERE c.fn_hash = $1 AND c.ns = $2 AND c.fn = $3 AND c.nargs = $4
                AND (EXISTS (SELECT 1 FROM versions v WHERE v.cell_id = c.cell_id AND v.recorded > $5)
                     OR EXISTS (SELECT 1 FROM versions v JOIN receipts r ON r.hash = v.receipt_hash
                                 WHERE v.cell_id = c.cell_id AND r.asof > $5))
                AND ",
            slice_match!(),
            " LIMIT 1"
        )
    };
}

/// The cells of a slice of cells that have a computed version answering as of A ($5): one
/// with tx <= A and its receipt's asof <= A (the computed versions' own asof). Ascending by
/// cell id, at most $16; the candidates from `$with` and `$from`.
macro_rules! computed_cells {
    ($with:expr, $from:expr) => {
        concat!(
            $with,
            "SELECT c.ns, c.fn, c.fn_hash,
                    ARRAY(SELECT a.name || ':' || a.type || ':' || a.value FROM cell_args a
                           WHERE a.cell_id = c.cell_id ORDER BY a.name)
               ",
            $from,
            "
              WHERE c.fn_hash = $1 AND c.ns = $2 AND c.fn = $3 AND c.nargs = $4
                AND EXISTS (SELECT 1 FROM computed_versions cv
                             WHERE cv.cell_id = c.cell_id AND cv.tx <= $5 AND cv.asof <= $5)
                AND ",
            slice_match!(),
            " ORDER BY c.cell_id LIMIT $16"
        )
    };
}

// The candidates of a slice: all the function's cells (`no_with!` and `from_all!`), or the
// cells of a driving argument's range ($12-$15: name, type, lo, hi) as the materialized CTE
// `cand`, which already applies the value-bound constraints by index probes, and then joined
// to `cells` by primary key (`from_drive!`). A text range is scanned in `cell_args_value`, a
// number's in `cell_args_num`.
macro_rules! no_with {
    () => {
        ""
    };
}
macro_rules! from_all {
    () => {
        "FROM cells c"
    };
}
macro_rules! from_drive {
    () => {
        "FROM cand k JOIN cells c ON c.cell_id = k.cell_id"
    };
}
macro_rules! drive_text {
    () => {
        concat!(
            "WITH cand AS MATERIALIZED (
            SELECT a.cell_id FROM cell_args a
             WHERE a.fn_hash = $1 AND a.name = $12 AND a.type = $13 AND a.value BETWEEN $14 AND $15
               AND ",
            value_probes!("a.cell_id"),
            ") "
        )
    };
}
macro_rules! drive_num {
    () => {
        concat!(
            "WITH cand AS MATERIALIZED (
            SELECT a.cell_id FROM cell_args a
             WHERE $13 = 'number' AND a.fn_hash = $1 AND a.name = $12 AND a.type = 'number'
               AND a.num BETWEEN $14::numeric AND $15::numeric
               AND ",
            value_probes!("a.cell_id"),
            ") "
        )
    };
}

statements! {
    Lock = "SELECT datumesh_lock($1, $2)" => [TEXT, TEXT];

    DescriptorGet = "SELECT bytes FROM descriptors WHERE fn_hash = $1" => [TEXT];
    DescriptorPut = "INSERT INTO descriptors (fn_hash, bytes) VALUES ($1, $2) ON CONFLICT (fn_hash) DO NOTHING"
        => [TEXT, BYTEA];
    DescriptorExists = "SELECT EXISTS (SELECT 1 FROM descriptors WHERE fn_hash = $1)" => [TEXT];
    // The inputs of a descriptor, indexed by name (0.5.1; migration 10).
    DescriptorInputsPut = "INSERT INTO descriptor_inputs (fn_hash, input_name)
                             SELECT $1, i FROM unnest($2::text[]) AS i ON CONFLICT DO NOTHING" => [TEXT, TEXT_ARRAY];
    FunctionReaders = "SELECT fn_hash FROM descriptor_inputs WHERE input_name = $1 ORDER BY fn_hash" => [TEXT];
    BoundNames = "SELECT DISTINCT fn_name FROM name_bindings WHERE fn_hash = $1 ORDER BY fn_name" => [TEXT];

    // Function tests (0.5.1; migration 10): one row per (fn_hash, case).
    FunctionTestPut = r#"INSERT INTO function_tests (fn_hash, "case", test_json, signed_by, last_run_at, last_result,
                           last_detail, branches_total, branches_covered)
                         VALUES ($1, $2, $3, $4, $5, $6, $7, $8, $9) ON CONFLICT (fn_hash, "case") DO NOTHING"#
        => [TEXT, TEXT, BYTEA, TEXT, TEXT, TEXT, TEXT, INT4, INT4];
    // The branch counts of a test are its hash's, read from the hash's counter (0.5.3).
    FunctionTestsOf = r#"SELECT t.fn_hash, t."case", t.test_json, t.signed_by, t.last_run_at, t.last_result, t.last_detail,
                           COALESCE(k.branches_total, 0), COALESCE(k.branches_covered, 0) FROM function_tests t
                           LEFT JOIN LATERAL (SELECT branches_total, branches_covered FROM function_test_counts c
                                               WHERE c.fn_hash = t.fn_hash LIMIT 1) k ON true
                         WHERE t.fn_hash = $1 ORDER BY t."case""#
        => [TEXT];
    FunctionTestOne = r#"SELECT t.fn_hash, t."case", t.test_json, t.signed_by, t.last_run_at, t.last_result, t.last_detail,
                           COALESCE(k.branches_total, 0), COALESCE(k.branches_covered, 0) FROM function_tests t
                           LEFT JOIN LATERAL (SELECT branches_total, branches_covered FROM function_test_counts c
                                               WHERE c.fn_hash = t.fn_hash LIMIT 1) k ON true
                         WHERE t.fn_hash = $1 AND t."case" = $2"#
        => [TEXT, TEXT];
    // The row a test is in before a run or a naming (0.5.3: the counters move with it).
    FunctionTestOld = r#"SELECT fn_name, last_result FROM function_tests WHERE fn_hash = $1 AND "case" = $2 FOR UPDATE"#
        => [TEXT, TEXT];
    // A counter row moves by deltas. The row is created first, with zero counts and the hash's
    // branch counts (or the given ones): a CHECK of a proposed row is tested before a conflict,
    // so a negative delta cannot go through an INSERT ... ON CONFLICT.
    CounterEnsure = "INSERT INTO function_test_counts (fn_hash, fn_name, branches_total, branches_covered)
                     VALUES ($1, $2,
                             COALESCE((SELECT k.branches_total FROM function_test_counts k WHERE k.fn_hash = $1 LIMIT 1), $3),
                             COALESCE((SELECT k.branches_covered FROM function_test_counts k WHERE k.fn_hash = $1 LIMIT 1), $4))
                     ON CONFLICT (fn_hash, fn_name) DO NOTHING"
        => [TEXT, TEXT, INT4, INT4];
    CounterAdd = "UPDATE function_test_counts SET n = n + $3, pass = pass + $4, fail = fail + $5, error = error + $6,
                    not_run = not_run + $7, last_run_at = GREATEST(last_run_at, $8)
                  WHERE fn_hash = $1 AND fn_name = $2"
        => [TEXT, TEXT, INT8, INT8, INT8, INT8, INT8, TEXT];
    // The latest run of a counter row, from its tests (after a run moved one, or a naming).
    CounterRunRecompute = "UPDATE function_test_counts c SET last_run_at = (SELECT max(t.last_run_at) FROM function_tests t
                             WHERE t.fn_hash = c.fn_hash AND COALESCE(t.fn_name, '') = c.fn_name)
                           WHERE c.fn_hash = $1 AND c.fn_name = $2"
        => [TEXT, TEXT];
    FunctionTestRecord = r#"UPDATE function_tests SET last_run_at = $1, last_result = $2, last_detail = $3
                            WHERE fn_hash = $4 AND "case" = $5"# => [TEXT, TEXT, TEXT, TEXT, TEXT];
    // The branch counts are per hash: on the hash's counter rows (0.5.3), none for a hash with no test.
    FunctionBranches = "UPDATE function_test_counts SET branches_total = $2, branches_covered = $3 WHERE fn_hash = $1"
        => [TEXT, INT4, INT4];
    // Named tests (0.5.2; migration 11): a name is set once, on the rows still unnamed.
    // Returns the rows it named (their name and last result), for the counters.
    FunctionTestName = r#"UPDATE function_tests f SET fn_name = n.name FROM unnest($2::text[], $3::text[]) AS n(c, name)
                          WHERE f.fn_hash = $1 AND f."case" = n.c AND f.fn_name IS NULL
                          RETURNING f.fn_name, f.last_result"#
        => [TEXT, TEXT_ARRAY, TEXT_ARRAY];
    FunctionTestsUnnamed = r#"SELECT t.fn_hash, t."case", t.test_json, t.signed_by, t.last_run_at, t.last_result, t.last_detail,
                           COALESCE(k.branches_total, 0), COALESCE(k.branches_covered, 0) FROM function_tests t
                           LEFT JOIN LATERAL (SELECT branches_total, branches_covered FROM function_test_counts c
                                               WHERE c.fn_hash = t.fn_hash LIMIT 1) k ON true
                         WHERE t.fn_name IS NULL ORDER BY t.fn_hash, t."case" LIMIT $1"#
        => [INT8];
    // The summary of a hash's tests, all of them or those of one name: one aggregate.
    // Read from the counters (0.5.3): the sum over the hash's rows, or the row of one name.
    FunctionTestSummary = r#"SELECT COALESCE(sum(n), 0)::bigint, COALESCE(sum(pass), 0)::bigint,
                           COALESCE(sum(fail), 0)::bigint, COALESCE(sum(error), 0)::bigint,
                           COALESCE(sum(not_run), 0)::bigint, max(last_run_at),
                           COALESCE(max(branches_total), 0), COALESCE(max(branches_covered), 0)
                         FROM function_test_counts WHERE fn_hash = $1 AND ($2::text IS NULL OR fn_name = $2)"#
        => [TEXT, TEXT];
    FunctionTestsFailing = r#"SELECT t.fn_hash, t."case", t.test_json, t.signed_by, t.last_run_at, t.last_result, t.last_detail,
                           COALESCE(k.branches_total, 0), COALESCE(k.branches_covered, 0) FROM function_tests t
                           LEFT JOIN LATERAL (SELECT branches_total, branches_covered FROM function_test_counts c
                                               WHERE c.fn_hash = t.fn_hash LIMIT 1) k ON true
                         WHERE t.fn_hash = $1 AND ($2::text IS NULL OR t.fn_name = $2)
                           AND t.last_result IN ('fail', 'error') ORDER BY t."case" LIMIT $3"#
        => [TEXT, TEXT, INT8];
    // A page of a hash's tests by case, after a case (exclusive), at most $LIMIT rows.
    FunctionTestsPage = r#"SELECT t.fn_hash, t."case", t.test_json, t.signed_by, t.last_run_at, t.last_result, t.last_detail,
                           COALESCE(k.branches_total, 0), COALESCE(k.branches_covered, 0) FROM function_tests t
                           LEFT JOIN LATERAL (SELECT branches_total, branches_covered FROM function_test_counts c
                                               WHERE c.fn_hash = t.fn_hash LIMIT 1) k ON true
                         WHERE t.fn_hash = $1 AND ($2::text IS NULL OR t."case" > $2) ORDER BY t."case" LIMIT $3"#
        => [TEXT, TEXT, INT8];
    FunctionTestsPageNamed = r#"SELECT t.fn_hash, t."case", t.test_json, t.signed_by, t.last_run_at, t.last_result, t.last_detail,
                           COALESCE(k.branches_total, 0), COALESCE(k.branches_covered, 0) FROM function_tests t
                           LEFT JOIN LATERAL (SELECT branches_total, branches_covered FROM function_test_counts c
                                               WHERE c.fn_hash = t.fn_hash LIMIT 1) k ON true
                         WHERE t.fn_hash = $1 AND t.fn_name = $2 AND ($3::text IS NULL OR t."case" > $3)
                         ORDER BY t."case" LIMIT $4"#
        => [TEXT, TEXT, TEXT, INT8];
    // The stored test cells of any namespace whose function_hash argument is $1 and whose latest
    // version is false (0.5.3): one query, at most $2 (cell id, receipt bytes).
    FalseTestCells = "SELECT c.cell_id, r.bytes FROM cell_args a JOIN cells c ON c.cell_id = a.cell_id
                        CROSS JOIN LATERAL (SELECT v.receipt_hash FROM versions v WHERE v.cell_id = c.cell_id
                                             ORDER BY v.recorded DESC LIMIT 1) m
                        JOIN receipts r ON r.hash = m.receipt_hash
                       WHERE a.name = 'function_hash' AND a.type = 'string' AND a.value = $1 AND c.fn = 'test_case'
                         AND r.value_type = 'boolean' AND r.value = 'false'
                       ORDER BY c.cell_id LIMIT $2"
        => [TEXT, INT8];
    // The names bound at $2 to a hash that reads $1 (0.5.3): the latest binding of each name
    // that was bound to a reader, one query, at most $3.
    DependantsOf = "WITH readers AS (SELECT fn_hash FROM descriptor_inputs WHERE input_name = $1),
                      cand AS (SELECT DISTINCT b.fn_name FROM name_bindings b
                                WHERE b.fn_hash IN (SELECT fn_hash FROM readers)),
                      latest AS (SELECT DISTINCT ON (b.fn_name) b.fn_name, b.fn_hash FROM name_bindings b
                                  JOIN cand USING (fn_name) WHERE b.tx <= $2 ORDER BY b.fn_name, b.tx DESC)
                    SELECT fn_name, fn_hash FROM latest WHERE fn_hash IN (SELECT fn_hash FROM readers)
                    ORDER BY fn_name LIMIT $3"
        => [TEXT, TEXT, INT8];
    // The receipts of the latest stored version of the first cells (by cell id) of a function
    // name and hash that have one: at most $4 cells read, one indexed query.
    StoredReceiptsOf = "SELECT r.bytes FROM (SELECT c.cell_id FROM cells c
                          WHERE c.fn_hash = $1 AND c.ns = $2 AND c.fn = $3
                            AND EXISTS (SELECT 1 FROM versions v WHERE v.cell_id = c.cell_id)
                          ORDER BY c.cell_id LIMIT $4) k
                        CROSS JOIN LATERAL (SELECT v.receipt_hash FROM versions v WHERE v.cell_id = k.cell_id
                                             ORDER BY v.recorded DESC LIMIT 1) m
                        JOIN receipts r ON r.hash = m.receipt_hash AND r.value_type <> ''
                        ORDER BY k.cell_id"
        => [TEXT, TEXT, TEXT, INT8];
    // The cells of a function name, from the counter (0.5.3; the PK, never a count over cells).
    StoredCellCount = "SELECT COALESCE((SELECT n FROM cell_counts WHERE fn_hash = $1 AND ns = $2 AND fn = $3), 0)"
        => [TEXT, TEXT, TEXT];
    // A version slice's versions, at most $4 (SQL LIMIT, in tx order).
    VersionRangeLimit = "SELECT v.tx, v.receipt_hash, (SELECT r.asof FROM receipts r WHERE r.hash = v.receipt_hash), (SELECT r.value_type = '' FROM receipts r WHERE r.hash = v.receipt_hash), v.recorded
                         FROM versions v WHERE v.cell_id = $1 AND v.tx >= $2 AND v.tx <= $3 ORDER BY v.tx LIMIT $4"
        => [TEXT, TEXT, TEXT, INT8];
    // Dictionaries (0.5.2): exact lookups on the primary key (owner, key, value).
    DictionaryHas = "SELECT EXISTS (SELECT 1 FROM key_values WHERE owner = $1 AND key = $2 AND value = $3)"
        => [TEXT, TEXT, TEXT];
    DictionaryCount = "SELECT count(*) FROM key_values WHERE owner = $1 AND key = $2" => [TEXT, TEXT];
    // The values in insertion order (0.5.3): the row's id, which an upsert keeps.
    DictionaryValuesOrdered = "SELECT value FROM key_values WHERE owner = $1 AND key = $2 ORDER BY id LIMIT $3"
        => [TEXT, TEXT, INT8];
    CatalogEntriesAll = "SELECT kind, name, label, description FROM catalog_entries ORDER BY id LIMIT $1" => [INT8];
    KeyOpenPut = r#"INSERT INTO open_keys (owner, key, marked_at)
                    VALUES ($1, $2, to_char(now() AT TIME ZONE 'UTC', 'YYYY-MM-DD"T"HH24:MI:SS.US"Z"'))
                    ON CONFLICT (owner, key) DO NOTHING"#
        => [TEXT, TEXT];
    KeyIsOpen = "SELECT EXISTS (SELECT 1 FROM open_keys WHERE owner = $1 AND key = $2)" => [TEXT, TEXT];
    OpenKeyCount = "SELECT count(*) FROM open_keys" => [];
    // A catalogue entry by name alone: a name may be both a function's and a class's; the
    // function's first, then a method's, then a class's.
    CatalogEntryNamed = "SELECT kind, name, hash, label, description, args, examples, source, signed_by
                         FROM catalog_entries WHERE name = $1
                         ORDER BY CASE kind WHEN 'function' THEN 0 WHEN 'method' THEN 1 ELSE 2 END LIMIT 1"
        => [TEXT];

    // The member-set rows (0.6.5, G2): inserted or nothing; read by entry.
    MemberSetPut = "INSERT INTO member_sets (entry, slice_id, members_hash, fn_hash, slice, members, tx, asof, licences)
                    VALUES ($1, $2, $3, $4, $5, $6, $7, $8, $9) ON CONFLICT (entry) DO NOTHING"
        => [TEXT, TEXT, TEXT, TEXT, BYTEA, INT4, TEXT, TEXT, TEXT];
    MemberSetGet = "SELECT slice_id, members_hash, fn_hash, slice, members, tx, asof, licences FROM member_sets WHERE entry = $1"
        => [TEXT];
    // The slice definitions (0.6.5, G10): inserted or nothing, once per slice id.
    SliceDefinitionPut = "INSERT INTO slice_definitions (slice_id, fn_hash, slice) VALUES ($1, $2, $3)
                          ON CONFLICT (slice_id) DO NOTHING"
        => [TEXT, TEXT, BYTEA];
    SliceDefinitionGet = "SELECT slice FROM slice_definitions WHERE slice_id = $1" => [TEXT];

    ReceiptGet = "SELECT bytes FROM receipts WHERE hash = $1" => [TEXT];
    ReceiptMeta = "SELECT cell_id, tx, asof, value_type = '' FROM receipts WHERE hash = $1" => [TEXT];
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
    VersionLatestTx = "SELECT recorded FROM versions WHERE cell_id = $1 ORDER BY recorded DESC LIMIT 1" => [TEXT];
    VersionLatest = "SELECT v.tx, v.receipt_hash, (SELECT r.asof FROM receipts r WHERE r.hash = v.receipt_hash), (SELECT r.value_type = '' FROM receipts r WHERE r.hash = v.receipt_hash), v.recorded
                     FROM versions v WHERE v.cell_id = $1 ORDER BY v.recorded DESC LIMIT 1"
        => [TEXT];
    VersionAsof = "SELECT v.tx, v.receipt_hash, (SELECT r.asof FROM receipts r WHERE r.hash = v.receipt_hash), (SELECT r.value_type = '' FROM receipts r WHERE r.hash = v.receipt_hash), v.recorded
                   FROM versions v WHERE v.cell_id = $1 AND v.recorded <= $2 ORDER BY v.recorded DESC LIMIT 1"
        => [TEXT, TEXT];
    VersionRange = "SELECT v.tx, v.receipt_hash, (SELECT r.asof FROM receipts r WHERE r.hash = v.receipt_hash), (SELECT r.value_type = '' FROM receipts r WHERE r.hash = v.receipt_hash), v.recorded
                    FROM versions v WHERE v.cell_id = $1 AND v.tx >= $2 AND v.tx <= $3 ORDER BY v.tx"
        => [TEXT, TEXT, TEXT];
    // A stored version answers as of A when its tx <= A and its receipt's asof <= A.
    StoredAsof = "SELECT v.tx, v.receipt_hash, r.asof, r.value_type = '', v.recorded FROM versions v JOIN receipts r ON r.hash = v.receipt_hash
                  WHERE v.cell_id = $1 AND v.recorded <= $2 AND r.asof <= $2 ORDER BY v.recorded DESC LIMIT 1"
        => [TEXT, TEXT];
    StoredAsofReceipt = "SELECT v.tx, v.receipt_hash, r.asof, r.value_type = '', v.recorded, r.bytes FROM versions v JOIN receipts r ON r.hash = v.receipt_hash
                         WHERE v.cell_id = $1 AND v.recorded <= $2 AND r.asof <= $2 ORDER BY v.recorded DESC LIMIT 1"
        => [TEXT, TEXT];
    // The same for a store whose schema is older than migration 17 (no recorded column yet): the
    // 0.6.4 statements, by tx. The store chooses them with HasRecorded (0.6.5, decision B).
    HasRecorded = "SELECT EXISTS (SELECT 1 FROM information_schema.columns WHERE table_schema = current_schema()
                     AND table_name = 'versions' AND column_name = 'recorded')"
        => [];
    VersionLatestTxLegacy = "SELECT tx FROM versions WHERE cell_id = $1 ORDER BY tx DESC LIMIT 1" => [TEXT];
    VersionPutLegacy = "INSERT INTO versions (cell_id, tx, tx_at, receipt_hash) VALUES ($1, $2, datumesh_instant($2), $3)"
        => [TEXT, TEXT, TEXT];
    VersionsLatestTxManyLegacy = "SELECT cell_id, max(tx) FROM versions WHERE cell_id = ANY($1::text[]) GROUP BY cell_id"
        => [TEXT_ARRAY];
    VersionsPutManyLegacy = "INSERT INTO versions (cell_id, tx, tx_at, receipt_hash)
                       SELECT c, t, datumesh_instant(t), r FROM unnest($1::text[], $2::text[], $3::text[]) AS x(c, t, r)"
        => [TEXT_ARRAY, TEXT_ARRAY, TEXT_ARRAY];
    VersionPut = "INSERT INTO versions (cell_id, tx, tx_at, receipt_hash, recorded) VALUES ($1, $2, datumesh_instant($2), $3, $4)"
        => [TEXT, TEXT, TEXT, TEXT];
    // The Resolver's evaluate of a stored function: the receipt of stored_asof's version.
    ValueAsof = "SELECT r.bytes FROM versions v JOIN receipts r ON r.hash = v.receipt_hash
                 WHERE v.cell_id = $1 AND v.recorded <= $2 AND r.asof <= $2 ORDER BY v.recorded DESC LIMIT 1"
        => [TEXT, TEXT];

    ComputedPut = "INSERT INTO computed_versions (cell_id, tx, tx_at, asof, asof_at, receipt_hash)
                   VALUES ($1, $2, datumesh_instant($2), $3, datumesh_instant($3), $4)
                   ON CONFLICT (cell_id, tx, asof) DO NOTHING"
        => [TEXT, TEXT, TEXT, TEXT];
    ComputedGet = "SELECT receipt_hash FROM computed_versions WHERE cell_id = $1 AND tx = $2 AND asof = $3"
        => [TEXT, TEXT, TEXT];
    // The greatest asof <= A, ties the greatest tx.
    ComputedAsof = "SELECT tx, receipt_hash, asof, false, tx FROM computed_versions
                    WHERE cell_id = $1 AND asof <= $2 ORDER BY asof DESC, tx DESC LIMIT 1"
        => [TEXT, TEXT];
    ComputedAsofReceipt = "SELECT c.tx, c.receipt_hash, c.asof, false, c.tx, r.bytes FROM
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

    NamespaceGet = "SELECT keydoc, validated_at, suspended_at, consecutive_failures, last_checked_at
                    FROM namespaces WHERE host = $1" => [TEXT];
    NamespaceList = "SELECT host, keydoc, validated_at, suspended_at, consecutive_failures, last_checked_at
                     FROM namespaces ORDER BY host" => [];
    NamespaceCheck = "UPDATE namespaces SET consecutive_failures = $2, last_checked_at = $3,
                        last_checked_at_at = datumesh_instant($3), suspended_at = $4::text,
                        suspended_at_at = CASE WHEN $4::text IS NULL THEN NULL ELSE datumesh_instant($4::text) END
                      WHERE host = $1" => [TEXT, INT4, TEXT, TEXT];
    NamespacePut = "INSERT INTO namespaces (host, keydoc, validated_at, validated_at_at)
                    VALUES ($1, $2, $3, datumesh_instant($3))
                    ON CONFLICT (host) DO UPDATE SET keydoc = EXCLUDED.keydoc,
                        validated_at = EXCLUDED.validated_at, validated_at_at = EXCLUDED.validated_at_at"
        => [TEXT, BYTEA, TEXT];

    NameLatest = "SELECT tx FROM name_bindings WHERE fn_name = $1 ORDER BY tx DESC LIMIT 1" => [TEXT];
    // The latest binding with its hash (NULL when the name is closed, 0.6.0).
    NameLatestRow = "SELECT tx, fn_hash FROM name_bindings WHERE fn_name = $1 ORDER BY tx DESC LIMIT 1" => [TEXT];
    NameClose = "INSERT INTO name_bindings (fn_name, tx, tx_at, fn_hash) VALUES ($1, $2, datumesh_instant($2), NULL)"
        => [TEXT, TEXT];
    NamePut = "INSERT INTO name_bindings (fn_name, tx, tx_at, fn_hash) VALUES ($1, $2, datumesh_instant($2), $3)"
        => [TEXT, TEXT, TEXT];
    NameAsof = "SELECT fn_hash FROM name_bindings WHERE fn_name = $1 AND tx <= $2 ORDER BY tx DESC LIMIT 1"
        => [TEXT, TEXT];
    BindingAsof = "SELECT fn_hash, tx FROM name_bindings WHERE fn_name = $1 AND tx <= $2 ORDER BY tx DESC LIMIT 1"
        => [TEXT, TEXT];
    // Whether a name is bound as a function (any binding), or as a class (any binding): the
    // two kinds of one namespace of names (0.4.1).
    NameBound = "SELECT EXISTS (SELECT 1 FROM name_bindings WHERE fn_name = $1)" => [TEXT];
    ClassNameBound = "SELECT EXISTS (SELECT 1 FROM class_bindings WHERE class_name = $1)" => [TEXT];

    // Classes (0.4.1): flattened class descriptors by class hash, and class-name bindings.
    ClassPut = "INSERT INTO classes (class_hash, bytes) VALUES ($1, $2) ON CONFLICT (class_hash) DO NOTHING"
        => [TEXT, BYTEA];
    ClassGet = "SELECT bytes FROM classes WHERE class_hash = $1" => [TEXT];
    ClassBindingLatest = "SELECT tx FROM class_bindings WHERE class_name = $1 ORDER BY tx DESC LIMIT 1" => [TEXT];
    ClassBindingLatestRow = "SELECT tx, class_hash FROM class_bindings WHERE class_name = $1 ORDER BY tx DESC LIMIT 1"
        => [TEXT];
    ClassBindingClose = "INSERT INTO class_bindings (class_name, tx, tx_at, class_hash) VALUES ($1, $2, datumesh_instant($2), NULL)"
        => [TEXT, TEXT];
    ClassBindingPut = "INSERT INTO class_bindings (class_name, tx, tx_at, class_hash) VALUES ($1, $2, datumesh_instant($2), $3)"
        => [TEXT, TEXT, TEXT];
    ClassBindingAsof = "SELECT class_hash, tx FROM class_bindings WHERE class_name = $1 AND tx <= $2 ORDER BY tx DESC LIMIT 1"
        => [TEXT, TEXT];
    // The class names bound to a hash as of an instant: each name's latest binding with tx <= $2
    // must be to the hash (0.5.3).
    ClassNamesBound = "SELECT b.class_name FROM class_bindings b
                       WHERE b.class_hash = $1 AND b.tx <= $2
                         AND b.tx = (SELECT max(c.tx) FROM class_bindings c WHERE c.class_name = b.class_name AND c.tx <= $2)
                       GROUP BY b.class_name ORDER BY b.class_name COLLATE \"C\""
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
    VersionsBetween = "SELECT v.tx, v.receipt_hash, r.asof, r.value_type = '', v.recorded, r.bytes FROM versions v JOIN receipts r ON r.hash = v.receipt_hash
                       WHERE v.cell_id = $1 AND v.tx >= $2 AND v.tx <= $3 AND v.recorded <= $4 AND r.asof <= $4 ORDER BY v.tx"
        => [TEXT, TEXT, TEXT, TEXT];

    // The members of a slice of cells, as of A ($5): cells of the function ($1-$3) with its
    // number of arguments ($4) and every constraint ($6-$10: name, kind open / value / range,
    // type, lo, hi; a value's lexical form is its lo) met, each with the stored version
    // answering as of A and the "type:value" of its key arguments ($11, in name order).
    // Candidates come from all the function's cells, or from a range scan of one argument
    // ($12-$15: name, type, lo, hi) of the typed-argument index.
    SliceMembersAll = slice_members!(no_with!(), from_all!(), "r.bytes", "m.bytes") => [TEXT, TEXT, TEXT, INT4, TEXT, TEXT_ARRAY, TEXT_ARRAY, TEXT_ARRAY, TEXT_ARRAY, TEXT_ARRAY, TEXT_ARRAY, TEXT, TEXT, TEXT, TEXT];
    SliceMembersText = slice_members!(drive_text!(), from_drive!(), "r.bytes", "m.bytes") => [TEXT, TEXT, TEXT, INT4, TEXT, TEXT_ARRAY, TEXT_ARRAY, TEXT_ARRAY, TEXT_ARRAY, TEXT_ARRAY, TEXT_ARRAY, TEXT, TEXT, TEXT, TEXT];
    SliceMembersNum = slice_members!(drive_num!(), from_drive!(), "r.bytes", "m.bytes") => [TEXT, TEXT, TEXT, INT4, TEXT, TEXT_ARRAY, TEXT_ARRAY, TEXT_ARRAY, TEXT_ARRAY, TEXT_ARRAY, TEXT_ARRAY, TEXT, TEXT, TEXT, TEXT];
    // The same with the receipt's value and the member's cell from the index (0.3.2): (tx,
    // receipt_hash, asof, value_type, value, ns, fn, fn_hash, "name:type:value" of each
    // argument by name, cell_id, key).
    // The latest withdrawal among a slice's members as of $5 (0.6.0; SPEC 8.2): (tx, asof).
    SliceWithdrawnAll = slice_withdrawn!(no_with!(), from_all!()) => [TEXT, TEXT, TEXT, INT4, TEXT, TEXT_ARRAY, TEXT_ARRAY, TEXT_ARRAY, TEXT_ARRAY, TEXT_ARRAY, TEXT_ARRAY, TEXT, TEXT, TEXT, TEXT];
    SliceWithdrawnText = slice_withdrawn!(drive_text!(), from_drive!()) => [TEXT, TEXT, TEXT, INT4, TEXT, TEXT_ARRAY, TEXT_ARRAY, TEXT_ARRAY, TEXT_ARRAY, TEXT_ARRAY, TEXT_ARRAY, TEXT, TEXT, TEXT, TEXT];
    SliceWithdrawnNum = slice_withdrawn!(drive_num!(), from_drive!()) => [TEXT, TEXT, TEXT, INT4, TEXT, TEXT_ARRAY, TEXT_ARRAY, TEXT_ARRAY, TEXT_ARRAY, TEXT_ARRAY, TEXT_ARRAY, TEXT, TEXT, TEXT, TEXT];
    SliceValuesAll = slice_members!(no_with!(), from_all!(), "r.value_type, r.value", "m.value_type, m.value, c.ns, c.fn, c.fn_hash,
                    ARRAY(SELECT a.name || ':' || a.type || ':' || a.value FROM cell_args a WHERE a.cell_id = c.cell_id ORDER BY a.name), c.sort_key") => [TEXT, TEXT, TEXT, INT4, TEXT, TEXT_ARRAY, TEXT_ARRAY, TEXT_ARRAY, TEXT_ARRAY, TEXT_ARRAY, TEXT_ARRAY, TEXT, TEXT, TEXT, TEXT];
    SliceValuesText = slice_members!(drive_text!(), from_drive!(), "r.value_type, r.value", "m.value_type, m.value, c.ns, c.fn, c.fn_hash,
                    ARRAY(SELECT a.name || ':' || a.type || ':' || a.value FROM cell_args a WHERE a.cell_id = c.cell_id ORDER BY a.name), c.sort_key") => [TEXT, TEXT, TEXT, INT4, TEXT, TEXT_ARRAY, TEXT_ARRAY, TEXT_ARRAY, TEXT_ARRAY, TEXT_ARRAY, TEXT_ARRAY, TEXT, TEXT, TEXT, TEXT];
    SliceValuesNum = slice_members!(drive_num!(), from_drive!(), "r.value_type, r.value", "m.value_type, m.value, c.ns, c.fn, c.fn_hash,
                    ARRAY(SELECT a.name || ':' || a.type || ':' || a.value FROM cell_args a WHERE a.cell_id = c.cell_id ORDER BY a.name), c.sort_key") => [TEXT, TEXT, TEXT, INT4, TEXT, TEXT_ARRAY, TEXT_ARRAY, TEXT_ARRAY, TEXT_ARRAY, TEXT_ARRAY, TEXT_ARRAY, TEXT, TEXT, TEXT, TEXT];
    // How many cells (up to 10,001) an argument's range holds, to choose the range scanned.
    SliceProbeText = "SELECT count(*) FROM (SELECT 1 FROM cell_args a
                       WHERE a.fn_hash = $1 AND a.name = $2 AND a.type = $3 AND a.value BETWEEN $4 AND $5 LIMIT 10001) x"
        => [TEXT, TEXT, TEXT, TEXT, TEXT];
    SliceProbeNum = "SELECT count(*) FROM (SELECT 1 FROM cell_args a
                      WHERE a.fn_hash = $1 AND a.name = $2 AND a.type = 'number' AND $3 = 'number'
                        AND a.num BETWEEN $4::numeric AND $5::numeric LIMIT 10001) x"
        => [TEXT, TEXT, TEXT, TEXT, TEXT];

    // 0.3.2: a version slice's values.
    VersionsBetweenValues = "SELECT v.tx, v.receipt_hash, r.asof, v.recorded, r.value_type, r.value, c.ns, c.fn, c.fn_hash,
                                    ARRAY(SELECT a.name || ':' || a.type || ':' || a.value FROM cell_args a
                                           WHERE a.cell_id = c.cell_id ORDER BY a.name), c.sort_key
                               FROM versions v JOIN receipts r ON r.hash = v.receipt_hash JOIN cells c ON c.cell_id = v.cell_id
                              WHERE v.cell_id = $1 AND v.tx >= $2 AND v.tx <= $3 AND v.recorded <= $4 AND r.asof <= $4
                                AND r.value_type <> '' ORDER BY v.tx"
        => [TEXT, TEXT, TEXT, TEXT];
    // computed_cells (0.4.2): the cells of a slice with a computed version as of A, $16 at most;
    // the columns are the cell's namespace, function name, function hash and "name:type:value"
    // of each argument (the key's arguments are not needed here: $11 is unused).
    ComputedCellsAll = computed_cells!(no_with!(), from_all!()) => [TEXT, TEXT, TEXT, INT4, TEXT, TEXT_ARRAY, TEXT_ARRAY, TEXT_ARRAY, TEXT_ARRAY, TEXT_ARRAY, TEXT_ARRAY, TEXT, TEXT, TEXT, TEXT, INT8];
    ComputedCellsText = computed_cells!(drive_text!(), from_drive!()) => [TEXT, TEXT, TEXT, INT4, TEXT, TEXT_ARRAY, TEXT_ARRAY, TEXT_ARRAY, TEXT_ARRAY, TEXT_ARRAY, TEXT_ARRAY, TEXT, TEXT, TEXT, TEXT, INT8];
    ComputedCellsNum = computed_cells!(drive_num!(), from_drive!()) => [TEXT, TEXT, TEXT, INT4, TEXT, TEXT_ARRAY, TEXT_ARRAY, TEXT_ARRAY, TEXT_ARRAY, TEXT_ARRAY, TEXT_ARRAY, TEXT, TEXT, TEXT, TEXT, INT8];

    // has_bindings_under (0.4.2): whether a name binding or a class binding has the prefix $1
    // (a namespace and "/").
    HasBindingsUnder = "SELECT EXISTS (SELECT 1 FROM name_bindings WHERE starts_with(fn_name, $1))
                          OR EXISTS (SELECT 1 FROM class_bindings WHERE starts_with(class_name, $1))"
        => [TEXT];

    // Whether a matching cell has a stored version whose receipt's asof (never earlier than
    // its tx) is after A ($5), the slice's parameters as for its members ($11-$15 unused
    // here). From the function's receipts by asof, then each one's cell against the slice:
    // Two branches, each stopping at its first match (EXISTS): a version recorded after A of a
    // matching cell (versions_recorded_all), or a receipt of the function with an asof after A and
    // its version (receipts_fn_hash_asof), of a matching cell.
    ChangedRecent = concat!(
        "SELECT true WHERE (EXISTS (SELECT 1 FROM versions v JOIN cells c ON c.cell_id = v.cell_id
                          WHERE v.recorded > $5 AND c.fn_hash = $1 AND c.ns = $2 AND c.fn = $3 AND c.nargs = $4 AND ",
        slice_match!(),
        " LIMIT 1)
          OR (EXISTS (SELECT 1 FROM receipts r JOIN versions v ON v.receipt_hash = r.hash JOIN cells c ON c.cell_id = v.cell_id
                      WHERE r.fn_hash = $1 AND r.asof > $5 AND c.fn_hash = $1 AND c.ns = $2 AND c.fn = $3 AND c.nargs = $4 AND ",
        slice_match!(),
        " LIMIT 1)))"
    ) => [TEXT, TEXT, TEXT, INT4, TEXT, TEXT_ARRAY, TEXT_ARRAY, TEXT_ARRAY, TEXT_ARRAY, TEXT_ARRAY, TEXT_ARRAY, TEXT, TEXT, TEXT, TEXT];
    // or from the slice's candidate cells, then each one's receipts by cell and asof.
    ChangedCandAll = changed_cand!(no_with!(), from_all!()) => [TEXT, TEXT, TEXT, INT4, TEXT, TEXT_ARRAY, TEXT_ARRAY, TEXT_ARRAY, TEXT_ARRAY, TEXT_ARRAY, TEXT_ARRAY, TEXT, TEXT, TEXT, TEXT];
    ChangedCandText = changed_cand!(drive_text!(), from_drive!()) => [TEXT, TEXT, TEXT, INT4, TEXT, TEXT_ARRAY, TEXT_ARRAY, TEXT_ARRAY, TEXT_ARRAY, TEXT_ARRAY, TEXT_ARRAY, TEXT, TEXT, TEXT, TEXT];
    ChangedCandNum = changed_cand!(drive_num!(), from_drive!()) => [TEXT, TEXT, TEXT, INT4, TEXT, TEXT_ARRAY, TEXT_ARRAY, TEXT_ARRAY, TEXT_ARRAY, TEXT_ARRAY, TEXT_ARRAY, TEXT, TEXT, TEXT, TEXT];
    // How many receipts of the function have an asof after A, and how many cells it has (up
    // to 10,001), to choose between the two.
    // (Ordered as the indexes are, so that a probe is an index scan of at most 10,001 entries
    // even when every row matches.)
    ProbeRecent = "SELECT count(*) FROM (SELECT 1 FROM receipts WHERE fn_hash = $1 AND asof > $2
                                          ORDER BY fn_hash, asof LIMIT 10001) x"
        => [TEXT, TEXT];
    ProbeCells = "SELECT count(*) FROM (SELECT 1 FROM cells WHERE fn_hash = $1 ORDER BY fn_hash, cell_id LIMIT 10001) x"
        => [TEXT];
    // The same for a version slice: a version of the cell with tx in [from, to] ($2, $3).
    VersionsChangedAfter = "SELECT EXISTS (SELECT 1 FROM versions v JOIN receipts r ON r.hash = v.receipt_hash
                             WHERE v.cell_id = $1 AND v.tx >= $2 AND v.tx <= $3 AND (v.recorded > $4 OR r.asof > $4))"
        => [TEXT, TEXT, TEXT, TEXT];

    // Batched reads (0.4.3). One row per input, in input order (`ord`): latest_many's version
    // of each cell (its tx, receipt hash and the receipt's asof), receipts_many's bytes.
    VersionLatestMany = "SELECT l.tx, l.receipt_hash, (SELECT r.asof FROM receipts r WHERE r.hash = l.receipt_hash),
                                    (SELECT r.value_type = '' FROM receipts r WHERE r.hash = l.receipt_hash), l.recorded
                         FROM unnest($1::text[]) WITH ORDINALITY AS u(id, ord)
                         LEFT JOIN LATERAL (SELECT v.tx, v.receipt_hash, v.recorded FROM versions v
                                             WHERE v.cell_id = u.id ORDER BY v.recorded DESC LIMIT 1) l ON true
                         ORDER BY u.ord"
        => [TEXT_ARRAY];
    ReceiptsMany = "SELECT r.bytes FROM unnest($1::text[]) WITH ORDINALITY AS u(h, ord)
                    LEFT JOIN receipts r ON r.hash = u.h ORDER BY u.ord"
        => [TEXT_ARRAY];

    // put_cells (0.4.3), one transaction: the version locks of the call's cells in item order
    // (one statement); the stored receipts, namespaces and latest tx of the call's hashes,
    // signer hosts and cells; then the inserts (receipts, edges, slice edges, versions), each
    // one statement with arrays.
    VersionLocksMany = "SELECT datumesh_lock('versions', id) FROM unnest($1::text[]) WITH ORDINALITY AS u(id, ord)
                        ORDER BY ord"
        => [TEXT_ARRAY];
    ReceiptsGetMany = "SELECT hash, bytes FROM receipts WHERE hash = ANY($1::text[])" => [TEXT_ARRAY];
    NamespacesGetMany = "SELECT host, keydoc, suspended_at FROM namespaces WHERE host = ANY($1::text[])" => [TEXT_ARRAY];
    VersionsLatestTxMany = "SELECT cell_id, max(recorded) FROM versions WHERE cell_id = ANY($1::text[]) GROUP BY cell_id"
        => [TEXT_ARRAY];
    ReceiptsPutMany = "INSERT INTO receipts (hash, bytes, cell_id, tx, tx_at, asof, asof_at, fn_hash, publisher)
                       SELECT h, b, c, t, datumesh_instant(t), s, datumesh_instant(s), f, p
                         FROM unnest($1::text[], $2::bytea[], $3::text[], $4::text[], $5::text[], $6::text[], $7::text[])
                              AS x(h, b, c, t, s, f, p)
                       ON CONFLICT (hash) DO NOTHING RETURNING hash"
        => [TEXT_ARRAY, BYTEA_ARRAY, TEXT_ARRAY, TEXT_ARRAY, TEXT_ARRAY, TEXT_ARRAY, TEXT_ARRAY];
    EdgesPutMany = "INSERT INTO edges (receipt_hash, input_hash, kind)
                    SELECT r, i, 'receipt' FROM unnest($1::text[], $2::text[]) AS x(r, i) ON CONFLICT DO NOTHING"
        => [TEXT_ARRAY, TEXT_ARRAY];
    SliceEdgesPutMany = "INSERT INTO slice_edges (receipt_hash, entry)
                         SELECT r, e FROM unnest($1::text[], $2::text[]) AS x(r, e) ON CONFLICT DO NOTHING"
        => [TEXT_ARRAY, TEXT_ARRAY];
    VersionsPutMany = "INSERT INTO versions (cell_id, tx, tx_at, receipt_hash, recorded)
                       SELECT c, t, datumesh_instant(t), r, rec FROM unnest($1::text[], $2::text[], $3::text[], $4::text[]) AS x(c, t, r, rec)"
        => [TEXT_ARRAY, TEXT_ARRAY, TEXT_ARRAY, TEXT_ARRAY];
}
