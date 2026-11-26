//! Store 0.5.3, SP1 and SP2 (CONTRACT-round-0.7-P): the cell counters (`cell_counts`) and the
//! function test counters (`function_test_counts`), migration 12 on PostgreSQL (8 on SQLite).
//! Both backends.
//!
//! Signatures assumed:
//!
//! ```text
//! fn cell_count(&self, ns: &str, fn_name: &str, fn_hash: &str) -> Result<u64>;   // as stored_cell_count
//! ```
//!
//! `function_test_summary` keeps its signature. The counter tables are read with raw SQL
//! through the columns the contract names. The "bundle" path is `put_cells` (the batch the
//! engine's bundle import and `POST /bundles` use); the CLI import is tested in the CLI
//! crate (`store_053_cli_counts.rs`). The migration test applies migrations 1..=11, writes
//! rows with the 0.5.2 schema, then applies 12: both backends' harness can stop part way.
//!
//! The migration's number and the SQL file's name are not given by the contract: only the
//! version is asserted (12 on PostgreSQL; 8 on SQLite, the next after its 7).

#[macro_use]
mod common;

use common::*;
use datumesh_core::Type;
use datumesh_store::{migrate, CatalogEntry, CatalogKind, CatalogSource, CellWrite, FunctionTest, SqliteOptions};
use std::collections::BTreeMap;

const T1: &str = "2024-03-15T16:00:00.000000Z";
const T2: &str = "2024-03-18T10:00:00.000000Z";
const T3: &str = "2024-03-20T08:30:00.000000Z";
const NAME: &str = "rates.example.org/fx_rate";
const NAME2: &str = "rates.example.org/fx_rate_two";

fn day(d: u32) -> String {
    format!("2024-01-{d:02}T00:00:00.000000Z")
}

fn kcell(name: &str, h: &str, k: u32) -> datumesh_core::Cell {
    slice_cell(name, h, &[("k", sc(Type::Number, &k.to_string()))])
}

/// The receipt of `cell` (value `v`) at day `d`.
fn receipt(cell: &datumesh_core::Cell, v: &str, d: u32) -> Vec<u8> {
    slice_receipt(cell, v, &day(d), &day(d))
}

fn put_cell(s: &TStore, cell: &datumesh_core::Cell, v: &str, d: u32) {
    let h = s.put_receipt(&receipt(cell, v, d)).unwrap();
    s.put_version(&cell.id(), &day(d), &h).unwrap();
}

/// The counter table's total and its row count, and the same by recounting `cells`.
fn counter_vs_cells(db: &TestDb) -> ((i64, i64), (i64, i64)) {
    let counter = (
        db.int("SELECT CAST(COALESCE(sum(n), 0) AS BIGINT) FROM cell_counts"),
        db.int("SELECT count(*) FROM cell_counts"),
    );
    let cells = (
        db.int("SELECT count(*) FROM cells"),
        db.int("SELECT count(*) FROM (SELECT DISTINCT fn_hash, ns, fn FROM cells) d"),
    );
    (counter, cells)
}

// ---- SP1 ----------------------------------------------------------------------------------

fn cell_count_follows_put_receipt_and_a_second_version_is_not_a_new_cell(b: Backend) {
    let db = TestDb::migrated(b);
    let s = slice_store(&db);
    let h = s.put_descriptor(&stored_descriptor(&[("k", Type::Number)], "f")).unwrap();
    let h2 = s.put_descriptor(&stored_descriptor(&[("k", Type::Number)], "f2")).unwrap();
    assert_eq!(s.cell_count(SLICE_HOST, "f", &h).unwrap(), 0);
    for k in 1..=3 {
        put_cell(&s, &kcell("f", &h, k), "1", 1);
    }
    assert_eq!(s.cell_count(SLICE_HOST, "f", &h).unwrap(), 3);
    // A correction: a second and third version of cell 1 is not a new cell.
    put_cell(&s, &kcell("f", &h, 1), "2", 2);
    put_cell(&s, &kcell("f", &h, 1), "3", 3);
    assert_eq!(s.cell_count(SLICE_HOST, "f", &h).unwrap(), 3);
    // The same receipt put again: no new cell either.
    s.put_receipt(&receipt(&kcell("f", &h, 2), "1", 1)).unwrap();
    assert_eq!(s.cell_count(SLICE_HOST, "f", &h).unwrap(), 3);
    // Another name with the same hash, another hash with the same name: counted apart.
    for k in 1..=2 {
        put_cell(&s, &kcell("g", &h, k), "1", 1);
    }
    put_cell(&s, &kcell("f", &h2, 1), "1", 1);
    assert_eq!(s.cell_count(SLICE_HOST, "f", &h).unwrap(), 3);
    assert_eq!(s.cell_count(SLICE_HOST, "g", &h).unwrap(), 2);
    assert_eq!(s.cell_count(SLICE_HOST, "f", &h2).unwrap(), 1);
    assert_eq!(s.cell_count(SLICE_HOST, "g", &h2).unwrap(), 0);
    assert_eq!(s.cell_count("other.example.org", "f", &h).unwrap(), 0);
    assert_eq!(s.cell_count(SLICE_HOST, "f", &"ee".repeat(32)).unwrap(), 0);
    // It agrees with the stored count and with a recount of the cells.
    assert_eq!(s.cell_count(SLICE_HOST, "f", &h).unwrap(), s.stored_cell_count(SLICE_HOST, "f", &h).unwrap());
    let (counter, cells) = counter_vs_cells(&db);
    assert_eq!(counter, cells);
    assert_eq!(counter, (6, 3));
    // The counter is read by primary key: its row is (fn_hash, ns, fn) -> n.
    assert_eq!(
        db.int(&format!("SELECT n FROM cell_counts WHERE fn_hash = '{h}' AND ns = '{SLICE_HOST}' AND fn = 'f'")),
        3
    );
}

/// An item: a receipt and the (cell id, tx) of its version, if any.
type Item = (Vec<u8>, Option<(String, String)>);

fn cell_count_follows_put_cells_the_bundle_path(b: Backend) {
    let db = TestDb::migrated(b);
    let s = slice_store(&db);
    let h = s.put_descriptor(&stored_descriptor(&[("k", Type::Number)], "f")).unwrap();
    // One batch: cells 1..=4, with cell 1 twice in it (two versions) and cell 5 with no version.
    let mut items: Vec<Item> = Vec::new();
    for k in 1..=4 {
        let c = kcell("f", &h, k);
        items.push((receipt(&c, "1", 1), Some((c.id(), day(1)))));
    }
    let c1 = kcell("f", &h, 1);
    items.push((receipt(&c1, "2", 2), Some((c1.id(), day(2)))));
    items.push((receipt(&kcell("f", &h, 5), "1", 1), None));
    let writes: Vec<CellWrite<'_>> =
        items.iter().map(|(r, v)| CellWrite::new(r, v.as_ref().map(|(c, t)| (c.as_str(), t.as_str())))).collect();
    s.put_cells(&writes).unwrap();
    assert_eq!(s.cell_count(SLICE_HOST, "f", &h).unwrap(), 5, "a statement that inserts several cells adds them all");
    // A second batch: one new cell and four known ones.
    let mut items2 = Vec::new();
    for k in 3..=7 {
        let c = kcell("f", &h, k);
        items2.push((receipt(&c, "9", 3), Some((c.id(), day(3)))));
    }
    let writes2: Vec<CellWrite<'_>> =
        items2.iter().map(|(r, v)| CellWrite::new(r, v.as_ref().map(|(c, t)| (c.as_str(), t.as_str())))).collect();
    s.put_cells(&writes2).unwrap();
    assert_eq!(s.cell_count(SLICE_HOST, "f", &h).unwrap(), 7);
    let (counter, cells) = counter_vs_cells(&db);
    assert_eq!(counter, cells);
}

fn catalog_summary_n_cells_equals_the_counter(b: Backend) {
    let db = TestDb::migrated(b);
    let s = slice_store(&db);
    let h = s.put_descriptor(&stored_descriptor(&[("k", Type::Number)], "f")).unwrap();
    for k in 1..=4 {
        put_cell(&s, &kcell("f", &h, k), "1", 1);
    }
    put_cell(&s, &kcell("f", &h, 1), "2", 2);
    let e = CatalogEntry {
        kind: CatalogKind::Function,
        name: format!("{SLICE_HOST}/f"),
        hash: Some(h.clone()),
        label: "F".into(),
        description: "A function.".into(),
        args: BTreeMap::new(),
        examples: "[]".into(),
        source: CatalogSource::Operator,
        signed_by: None,
    };
    s.upsert_catalog(&[e]).unwrap();
    let sum = s.catalog_summary().unwrap();
    assert_eq!(sum.entries.len(), 1);
    assert_eq!(sum.entries[0].n_cells, 4);
    assert_eq!(sum.entries[0].n_cells as u64, s.cell_count(SLICE_HOST, "f", &h).unwrap());
    // It reads the counter: a counter set by hand shows (the summary never recounts `cells`).
    db.exec(&format!("UPDATE cell_counts SET n = 41 WHERE fn_hash = '{h}'")).unwrap();
    assert_eq!(s.catalog_summary().unwrap().entries[0].n_cells, 41);
    assert_eq!(s.cell_count(SLICE_HOST, "f", &h).unwrap(), 41);
}

// ---- Migration 12 on a database that already has rows -------------------------------------

fn migrate_to(db: &TestDb, b: Backend, up_to: usize) {
    let old: Vec<&migrate::Migration> = match b {
        Backend::Pg => migrate::MIGRATIONS[..up_to].iter().collect(),
        Backend::Sqlite => migrate::sqlite::MIGRATIONS[..up_to].iter().collect(),
    };
    db.exec(db.pick(
        "CREATE TABLE IF NOT EXISTS datumesh_schema_migrations (version integer PRIMARY KEY, name text NOT NULL, checksum text NOT NULL, applied_at timestamptz NOT NULL DEFAULT now())",
        "CREATE TABLE IF NOT EXISTS datumesh_schema_migrations (version INTEGER PRIMARY KEY, name TEXT NOT NULL, checksum TEXT NOT NULL, applied_at TEXT NOT NULL DEFAULT (strftime('%Y-%m-%dT%H:%M:%fZ', 'now'))) STRICT",
    ))
    .unwrap();
    for m in &old {
        db.exec(m.sql).unwrap();
        db.exec(&format!(
            "INSERT INTO datumesh_schema_migrations (version, name, checksum) VALUES ({}, '{}', '{}')",
            m.version,
            m.name,
            m.checksum()
        ))
        .unwrap();
    }
}

fn hex_of(b: &[u8]) -> String {
    b.iter().map(|x| format!("{x:02x}")).collect()
}

fn text_of(db: &TestDb, sql: &str) -> Vec<Vec<String>> {
    db.texts(sql)
}

fn the_migration_fills_both_counters_from_rows_that_exist(b: Backend) {
    let db = TestDb::empty(b);
    let n_all = match b {
        Backend::Pg => migrate::MIGRATIONS.len(),
        Backend::Sqlite => migrate::sqlite::MIGRATIONS.len(),
    };
    let last = match b {
        Backend::Pg => migrate::MIGRATIONS.last().unwrap().version,
        Backend::Sqlite => migrate::sqlite::MIGRATIONS.last().unwrap().version,
    };
    assert_eq!(last as usize, n_all);
    assert_eq!(n_all, if b == Backend::Pg { 16 } else { 12 }, "migrations 1..=16 (SQLite 1..=12)");
    // The counters are migration 12 (SQLite 8); the facets migration 13 (9), the withdrawals 14 (10), the sort keys 15 (11) and the member sets 16 (12) follow them.
    let n_all = n_all - 3;
    migrate_to(&db, b, n_all - 1);
    assert!(text_of(&db, db.pick(
        "SELECT table_name::text FROM information_schema.tables WHERE table_schema = current_schema() AND table_name IN ('cell_counts', 'function_test_counts')",
        "SELECT name FROM sqlite_master WHERE type = 'table' AND name IN ('cell_counts', 'function_test_counts')",
    ))
    .is_empty());

    // Cells as 0.5.2 stored them: through the store (the cell index trigger fills `cells`).
    let s = {
        let s = db.bare_store();
        s.put_namespace(SLICE_HOST, &slice_keydoc(), VALIDATED_AT).unwrap();
        s
    };
    let h = s.put_descriptor(&stored_descriptor(&[("k", Type::Number)], "f")).unwrap();
    for k in 1..=5 {
        put_cell(&s, &kcell("f", &h, k), "1", 1);
    }
    put_cell(&s, &kcell("f", &h, 2), "2", 2); // a second version
    for k in 1..=2 {
        put_cell(&s, &kcell("g", &h, k), "1", 1);
    }
    assert_eq!(db.count("cells"), 7);

    // Function tests as 0.5.2 wrote them: h1 has four (one named, two have run), h2 one.
    let (h1, h2) = ("ab".repeat(32), "cd".repeat(32));
    let line = jcs(br#"{"args":{},"case":"00000000000000a1","expect":"1"}"#);
    for (hh, case) in [
        (&h1, "00000000000000a1"),
        (&h1, "00000000000000a2"),
        (&h1, "00000000000000a3"),
        (&h1, "00000000000000a4"),
        (&h2, "00000000000000b1"),
    ] {
        db.exec(db.pick(
            &format!("INSERT INTO function_tests (fn_hash, \"case\", test_json, signed_by) VALUES ('{hh}', '{case}', decode('{}', 'hex'), 'rates.example.org')", hex_of(&line)),
            &format!("INSERT INTO function_tests (fn_hash, \"case\", test_json, signed_by) VALUES ('{hh}', '{case}', X'{}', 'rates.example.org')", hex_of(&line)),
        ))
        .unwrap();
    }
    db.exec(&format!("UPDATE function_tests SET last_run_at = '{T1}', last_result = 'pass', last_detail = 'ok' WHERE fn_hash = '{h1}' AND \"case\" = '00000000000000a1'")).unwrap();
    db.exec(&format!("UPDATE function_tests SET last_run_at = '{T2}', last_result = 'fail', last_detail = 'no' WHERE fn_hash = '{h1}' AND \"case\" = '00000000000000a2'")).unwrap();
    db.exec(&format!("UPDATE function_tests SET fn_name = '{NAME}' WHERE fn_hash = '{h1}' AND \"case\" IN ('00000000000000a2', '00000000000000a3')")).unwrap();
    db.exec(&format!("UPDATE function_tests SET branches_total = 6, branches_covered = 4 WHERE fn_hash = '{h1}'"))
        .unwrap();

    let applied = match b {
        Backend::Pg => migrate::apply(&db.pg().bare_store()).unwrap(),
        Backend::Sqlite => migrate::sqlite::apply(&db.lite().store_with(SqliteOptions::new())).unwrap(),
    };
    assert_eq!(
        applied,
        [n_all as u32, n_all as u32 + 1, n_all as u32 + 2, n_all as u32 + 3],
        "only migrations 12, 13 (the facets), 14 and 15 are applied"
    );

    // cell_counts is filled from `cells`, once.
    let (counter, cells) = counter_vs_cells(&db);
    assert_eq!(counter, cells);
    assert_eq!(counter, (7, 2));
    let s = db.bare_store();
    assert_eq!(s.cell_count(SLICE_HOST, "f", &h).unwrap(), 5);
    assert_eq!(s.cell_count(SLICE_HOST, "g", &h).unwrap(), 2);
    // And keeps counting from there.
    put_cell(&s, &kcell("f", &h, 6), "1", 1);
    assert_eq!(s.cell_count(SLICE_HOST, "f", &h).unwrap(), 6);

    // function_test_counts is filled from function_tests, per (hash, name), '' for unnamed.
    let rows = text_of(&db, "SELECT fn_hash, fn_name, CAST(n AS TEXT), CAST(pass AS TEXT), CAST(fail AS TEXT), CAST(error AS TEXT), CAST(not_run AS TEXT), CAST(branches_total AS TEXT), CAST(branches_covered AS TEXT) FROM function_test_counts ORDER BY fn_hash, fn_name");
    let row = |hh: &str, name: &str, v: [&str; 7]| {
        let mut r = vec![hh.to_string(), name.to_string()];
        r.extend(v.iter().map(|x| x.to_string()));
        r
    };
    assert_eq!(
        rows,
        vec![
            row(&h1, "", ["2", "1", "0", "0", "1", "6", "4"]),
            row(&h1, NAME, ["2", "0", "1", "0", "1", "6", "4"]),
            row(&h2, "", ["1", "0", "0", "0", "1", "0", "0"]),
        ]
    );
    let sum = s.function_test_summary(&h1, None).unwrap();
    assert_eq!((sum.count, sum.pass, sum.fail, sum.error, sum.not_run), (4, 1, 1, 0, 2));
    assert_eq!((sum.branches_total, sum.branches_covered), (6, 4));
    assert_eq!(sum.last_run_at.as_deref(), Some(T2));
    let named = s.function_test_summary(&h1, Some(NAME)).unwrap();
    assert_eq!((named.count, named.fail, named.not_run), (2, 1, 1));

    // Re-running the migrations does not fill again.
    let again = match b {
        Backend::Pg => migrate::apply(&db.pg().bare_store()).unwrap(),
        Backend::Sqlite => migrate::sqlite::apply(&db.lite().store_with(SqliteOptions::new())).unwrap(),
    };
    assert!(again.is_empty());
    assert_eq!(s.cell_count(SLICE_HOST, "f", &h).unwrap(), 6);
}

fn a_fresh_store_has_the_counter_tables_empty(b: Backend) {
    let db = TestDb::migrated(b);
    assert_eq!(db.count("cell_counts"), 0);
    assert_eq!(db.count("function_test_counts"), 0);
    let last = match b {
        Backend::Pg => migrate::MIGRATIONS.last().unwrap(),
        Backend::Sqlite => migrate::sqlite::MIGRATIONS.last().unwrap(),
    };
    assert_eq!(last.version, if b == Backend::Pg { 16 } else { 12 });
    assert_eq!(db.count("datumesh_schema_migrations"), last.version as i64);
}

// ---- SP2 ----------------------------------------------------------------------------------

fn test_of(h: &str, n: u32) -> FunctionTest {
    let case = format!("{n:016x}");
    let line = datumesh_core::json::parse(format!(r#"{{"args":{{}},"case":"{case}","expect":"{n}"}}"#).as_bytes())
        .unwrap()
        .to_jcs();
    FunctionTest::new(h, &case, &line, "rates.example.org")
}

fn case_of(n: u32) -> String {
    format!("{n:016x}")
}

/// The counter rows of `h` as (name, n, pass, fail, error, not_run), by name.
fn counter_rows(db: &TestDb, h: &str) -> Vec<Vec<String>> {
    db.texts(&format!("SELECT fn_name, CAST(n AS TEXT), CAST(pass AS TEXT), CAST(fail AS TEXT), CAST(error AS TEXT), CAST(not_run AS TEXT) FROM function_test_counts WHERE fn_hash = '{h}' AND n > 0 ORDER BY fn_name"))
}

/// The same, recounted from `function_tests`.
fn recount(db: &TestDb, h: &str) -> Vec<Vec<String>> {
    db.texts(&format!("SELECT COALESCE(fn_name, ''), CAST(count(*) AS TEXT), CAST(sum(CASE WHEN last_result = 'pass' THEN 1 ELSE 0 END) AS TEXT), CAST(sum(CASE WHEN last_result = 'fail' THEN 1 ELSE 0 END) AS TEXT), CAST(sum(CASE WHEN last_result = 'error' THEN 1 ELSE 0 END) AS TEXT), CAST(sum(CASE WHEN last_result IS NULL THEN 1 ELSE 0 END) AS TEXT) FROM function_tests WHERE fn_hash = '{h}' GROUP BY COALESCE(fn_name, '') ORDER BY 1"))
}

fn rows(want: &[[&str; 6]]) -> Vec<Vec<String>> {
    want.iter().map(|r| r.iter().map(|x| x.to_string()).collect()).collect()
}

fn function_test_counts_follow_put_record_name_and_branches(b: Backend) {
    let db = TestDb::migrated(b);
    let s = db.bare_store();
    let h = "ab".repeat(32);
    let other = "cd".repeat(32);

    // Four tests, unnamed, none run.
    for n in 1..=4 {
        assert!(s.put_function_test(&test_of(&h, n)).unwrap());
    }
    assert_eq!(counter_rows(&db, &h), rows(&[["", "4", "0", "0", "0", "4"]]));
    // A test put again changes nothing (the first writer's row stays).
    assert!(!s.put_function_test(&test_of(&h, 1)).unwrap());
    assert_eq!(counter_rows(&db, &h), rows(&[["", "4", "0", "0", "0", "4"]]));
    // A test that carries a result when put counts under it.
    let mut ran = test_of(&h, 5);
    ran.last_run_at = Some(T1.to_string());
    ran.last_result = Some("pass".to_string());
    ran.last_detail = Some("ok".to_string());
    assert!(s.put_function_test(&ran).unwrap());
    assert_eq!(counter_rows(&db, &h), rows(&[["", "5", "1", "0", "0", "4"]]));

    // Runs: 1 pass, 2 fail, 3 error; then rows move between results.
    assert!(s.record_function_test(&h, &case_of(1), T1, "pass", "ok").unwrap());
    assert!(s.record_function_test(&h, &case_of(2), T1, "fail", "no").unwrap());
    assert!(s.record_function_test(&h, &case_of(3), T2, "error", "boom").unwrap());
    assert_eq!(counter_rows(&db, &h), rows(&[["", "5", "2", "1", "1", "1"]]));
    assert!(s.record_function_test(&h, &case_of(1), T2, "fail", "now no").unwrap()); // pass -> fail
    assert_eq!(counter_rows(&db, &h), rows(&[["", "5", "1", "2", "1", "1"]]));
    assert!(s.record_function_test(&h, &case_of(1), T3, "fail", "still no").unwrap()); // fail -> fail
    assert!(s.record_function_test(&h, &case_of(3), T3, "pass", "fixed").unwrap()); // error -> pass
    assert!(s.record_function_test(&h, &case_of(4), T3, "error", "x").unwrap()); // not run -> error
    assert_eq!(counter_rows(&db, &h), rows(&[["", "5", "2", "2", "1", "0"]]));
    // A test that does not exist and a result that is not one change nothing.
    assert!(!s.record_function_test(&h, &case_of(9), T3, "pass", "-").unwrap());
    assert!(s.record_function_test(&h, &case_of(2), T3, "bogus", "-").is_err());
    assert_eq!(counter_rows(&db, &h), rows(&[["", "5", "2", "2", "1", "0"]]));
    assert_eq!(counter_rows(&db, &h), recount(&db, &h));
    let sum = s.function_test_summary(&h, None).unwrap();
    assert_eq!((sum.count, sum.pass, sum.fail, sum.error, sum.not_run), (5, 2, 2, 1, 0));
    assert_eq!(sum.last_run_at.as_deref(), Some(T3));

    // Naming: cases 1, 2 and 4 get NAME; a named one keeps its name; a missing case changes nothing.
    let names: Vec<(String, String)> = [1, 2, 4].iter().map(|n| (case_of(*n), NAME.to_string())).collect();
    assert_eq!(s.name_function_tests(&h, &names).unwrap(), 3);
    assert_eq!(
        counter_rows(&db, &h),
        rows(&[["", "2", "2", "0", "0", "0"], [NAME, "3", "0", "2", "1", "0"]]),
        "rows moved from unnamed to the name, with their results"
    );
    let again: Vec<(String, String)> =
        vec![(case_of(1), NAME2.to_string()), (case_of(3), NAME2.to_string()), (case_of(9), NAME2.to_string())];
    assert_eq!(s.name_function_tests(&h, &again).unwrap(), 1, "only case 3 was unnamed");
    assert_eq!(
        counter_rows(&db, &h),
        rows(&[["", "1", "1", "0", "0", "0"], [NAME, "3", "0", "2", "1", "0"], [NAME2, "1", "1", "0", "0", "0"]]),
        "case 3 had passed: it moves with its result"
    );
    assert_eq!(counter_rows(&db, &h), recount(&db, &h));
    // A run of a named test moves within its named row.
    assert!(s.record_function_test(&h, &case_of(2), T3, "pass", "fixed").unwrap());
    assert_eq!(counter_rows(&db, &h), recount(&db, &h));

    // summary: with a name its row; with None the sum over the hash's rows.
    let all = s.function_test_summary(&h, None).unwrap();
    assert_eq!((all.count, all.pass, all.fail, all.error, all.not_run), (5, 3, 1, 1, 0));
    let n1 = s.function_test_summary(&h, Some(NAME)).unwrap();
    assert_eq!((n1.count, n1.pass, n1.fail, n1.error), (3, 1, 1, 1));
    let n2 = s.function_test_summary(&h, Some(NAME2)).unwrap();
    assert_eq!((n2.count, n2.pass), (1, 1));
    assert_eq!(s.function_test_summary(&h, Some("rates.example.org/none")).unwrap().count, 0);
    assert_eq!(s.function_test_summary(&other, None).unwrap().count, 0);

    // Branches are per hash: every row of the hash's counters shows them; the summary of the
    // hash and of each name; a hash with no tests is a no-op; covered above total is Invalid.
    s.set_function_branches(&h, 8, 5).unwrap();
    for name in [None, Some(NAME), Some(NAME2)] {
        let t = s.function_test_summary(&h, name).unwrap();
        assert_eq!((t.branches_total, t.branches_covered), (8, 5), "{name:?}");
    }
    assert_eq!(db.int(&format!("SELECT count(*) FROM function_test_counts WHERE fn_hash = '{h}' AND n > 0 AND (branches_total <> 8 OR branches_covered <> 5)")), 0);
    s.set_function_branches(&h, 9, 9).unwrap();
    assert_eq!(s.function_test_summary(&h, None).unwrap().branches_total, 9);
    assert!(s.set_function_branches(&h, 3, 4).is_err());
    assert_eq!(s.function_test_summary(&h, None).unwrap().branches_covered, 9, "a refused call changes nothing");
    s.set_function_branches(&other, 4, 2).unwrap();
    let z = s.function_test_summary(&other, None).unwrap();
    assert_eq!((z.count, z.branches_total, z.branches_covered), (0, 0, 0));
    assert_eq!(counter_rows(&db, &other), Vec::<Vec<String>>::new());
    // A test put later on a hash that has branches does not lose them.
    assert!(s.put_function_test(&test_of(&h, 6)).unwrap());
    let t = s.function_test_summary(&h, None).unwrap();
    assert_eq!((t.count, t.branches_total, t.branches_covered), (6, 9, 9));
    assert_eq!(counter_rows(&db, &h), recount(&db, &h));
}

fn the_summary_with_none_sums_the_hashs_counter_rows(b: Backend) {
    let db = TestDb::migrated(b);
    let s = db.bare_store();
    let h = "ab".repeat(32);
    for n in 1..=6 {
        assert!(s.put_function_test(&test_of(&h, n)).unwrap());
    }
    let res = [(1, "pass", T1), (2, "pass", T3), (3, "fail", T2), (4, "error", T1)];
    for (n, r, t) in res {
        assert!(s.record_function_test(&h, &case_of(n), t, r, "d").unwrap());
    }
    let to = |ns: &[u32], name: &str| ns.iter().map(|n| (case_of(*n), name.to_string())).collect::<Vec<_>>();
    s.name_function_tests(&h, &to(&[1, 3], "rates.example.org/a")).unwrap();
    s.name_function_tests(&h, &to(&[2], "rates.example.org/b")).unwrap();
    let all = s.function_test_summary(&h, None).unwrap();
    assert_eq!((all.count, all.pass, all.fail, all.error, all.not_run), (6, 2, 1, 1, 2));
    assert_eq!(all.last_run_at.as_deref(), Some(T3), "the latest over the rows");
    let a = s.function_test_summary(&h, Some("rates.example.org/a")).unwrap();
    assert_eq!((a.count, a.pass, a.fail, a.last_run_at.as_deref()), (2, 1, 1, Some(T2)));
    let un = db.int(&format!("SELECT n FROM function_test_counts WHERE fn_hash = '{h}' AND fn_name = ''"));
    assert_eq!(un, 3);
}

backends!(
    cell_count_follows_put_receipt_and_a_second_version_is_not_a_new_cell,
    cell_count_follows_put_cells_the_bundle_path,
    catalog_summary_n_cells_equals_the_counter,
    the_migration_fills_both_counters_from_rows_that_exist,
    a_fresh_store_has_the_counter_tables_empty,
    function_test_counts_follow_put_record_name_and_branches,
    the_summary_with_none_sums_the_hashs_counter_rows,
);
