//! Migration 11 (`0011_function_test_names.sql`) and SQLite migration 7 (0.5.2): the
//! `function_tests.fn_name` column (NULL), its index `(fn_hash, fn_name, case)` and the
//! `open_keys` table. A store of 0.5.1 keeps its function tests, unnamed, unchanged. Both
//! backends.

#[macro_use]
mod common;

use common::*;
use datumesh_store::{migrate, FunctionTest, SqliteOptions};

fn table_exists(db: &TestDb, table: &str) -> bool {
    let pg = format!("SELECT count(*) FROM information_schema.tables WHERE table_schema = current_schema() AND table_name = '{table}'");
    let lite = format!("SELECT count(*) FROM sqlite_master WHERE type = 'table' AND name = '{table}'");
    db.int(db.pick(&pg, &lite)) == 1
}

fn columns(db: &TestDb, table: &str) -> Vec<String> {
    let pg = format!("SELECT column_name::text FROM information_schema.columns WHERE table_schema = current_schema() AND table_name = '{table}' ORDER BY column_name");
    let lite = format!("SELECT name FROM pragma_table_info('{table}') ORDER BY name");
    db.texts(db.pick(&pg, &lite)).into_iter().map(|r| r[0].clone()).collect()
}

fn bytes_hex(b: &[u8]) -> String {
    b.iter().map(|x| format!("{x:02x}")).collect()
}

fn a_store_of_0_5_1_keeps_its_function_tests_unnamed(b: Backend) {
    let db = TestDb::empty(b);
    let (old, all): (Vec<&migrate::Migration>, usize) = match b {
        Backend::Pg => (migrate::MIGRATIONS[..10].iter().collect(), migrate::MIGRATIONS.len()),
        Backend::Sqlite => (migrate::sqlite::MIGRATIONS[..6].iter().collect(), migrate::sqlite::MIGRATIONS.len()),
    };
    assert!(
        all >= old.len() + 7,
        "at least seven migrations more than 0.5.1 on each backend (0.5.2, 0.5.3, facets, withdrawals, sort keys, member sets, recorded; later releases add more)"
    );
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
    assert!(!columns(&db, "function_tests").contains(&"fn_name".to_string()));
    assert!(!table_exists(&db, "open_keys"));

    // Two rows as 0.5.1 wrote them (one has run).
    let (h1, h2) = ("ab".repeat(32), "cd".repeat(32));
    let line = jcs(br#"{"args":{},"case":"00000000000000a1","expect":"1"}"#);
    for (h, case) in [(&h1, "00000000000000a1"), (&h2, "00000000000000b2")] {
        db.exec(db.pick(
            &format!("INSERT INTO function_tests (fn_hash, \"case\", test_json, signed_by) VALUES ('{h}', '{case}', decode('{}', 'hex'), 'rates.example.org')", bytes_hex(&line)),
            &format!("INSERT INTO function_tests (fn_hash, \"case\", test_json, signed_by) VALUES ('{h}', '{case}', X'{}', 'rates.example.org')", bytes_hex(&line)),
        ))
        .unwrap();
    }
    db.exec(&format!(
        "UPDATE function_tests SET last_run_at = '2024-03-15T16:00:00.000000Z', last_result = 'pass', last_detail = 'ok', branches_total = 5, branches_covered = 2 WHERE fn_hash = '{h1}'"
    ))
    .unwrap();

    let applied = match b {
        Backend::Pg => migrate::apply(&db.pg().bare_store()).unwrap(),
        Backend::Sqlite => migrate::sqlite::apply(&db.lite().store_with(SqliteOptions::new())).unwrap(),
    };
    assert_eq!(
        applied,
        ((old.len() as u32 + 1)..=all as u32).collect::<Vec<u32>>(),
        "every migration after the old ones: 0.5.2, 0.5.3, facets, withdrawals, sort-key, member-set and recorded and any later"
    );

    // The column, the table; every old row has a NULL name and reads back unchanged.
    assert!(columns(&db, "function_tests").contains(&"fn_name".to_string()));
    assert!(table_exists(&db, "open_keys"));
    assert_eq!(db.count("function_tests"), 2);
    assert_eq!(db.count("open_keys"), 0);
    assert_eq!(db.int("SELECT count(*) FROM function_tests WHERE fn_name IS NULL"), 2);
    let s = db.bare_store();
    let mut want = FunctionTest::new(&h1, "00000000000000a1", &line, "rates.example.org");
    want.last_run_at = Some("2024-03-15T16:00:00.000000Z".to_string());
    want.last_result = Some("pass".to_string());
    want.last_detail = Some("ok".to_string());
    want.branches_total = 5;
    want.branches_covered = 2;
    assert_eq!(s.function_tests(&h1).unwrap(), vec![want]);
    assert_eq!(
        s.function_tests(&h2).unwrap(),
        vec![FunctionTest::new(&h2, "00000000000000b2", &line, "rates.example.org")]
    );
    // They are the unnamed ones, in (hash, case) order, and can be named.
    let unnamed = s.unnamed_function_tests(10).unwrap();
    assert_eq!(unnamed.iter().map(|t| t.fn_hash.clone()).collect::<Vec<_>>(), [h1.clone(), h2.clone()]);
    assert_eq!(
        s.name_function_tests(&h1, &[("00000000000000a1".to_string(), "rates.example.org/fx_rate".to_string())])
            .unwrap(),
        1
    );
    assert_eq!(s.unnamed_function_tests(10).unwrap().len(), 1);
    let sum = s.function_test_summary(&h1, Some("rates.example.org/fx_rate")).unwrap();
    assert_eq!((sum.count, sum.pass, sum.branches_total, sum.branches_covered), (1, 1, 5, 2));

    // Re-running is a no-op.
    let again = match b {
        Backend::Pg => migrate::apply(&db.pg().bare_store()).unwrap(),
        Backend::Sqlite => migrate::sqlite::apply(&db.lite().store_with(SqliteOptions::new())).unwrap(),
    };
    assert!(again.is_empty());
}

fn a_fresh_store_has_the_column_the_index_and_the_table(b: Backend) {
    let db = TestDb::migrated(b);
    assert!(columns(&db, "function_tests").contains(&"fn_name".to_string()));
    assert_eq!(columns(&db, "open_keys"), ["key", "marked_at", "owner"]);
    let idx = db.int(db.pick(
        "SELECT count(*) FROM pg_indexes WHERE tablename = 'function_tests' AND indexdef LIKE '%fn_hash%fn_name%'",
        "SELECT count(*) FROM pragma_index_list('function_tests') l JOIN pragma_index_info(l.name) i WHERE i.name = 'fn_name'",
    ));
    assert!(idx >= 1, "an index of function_tests over (fn_hash, fn_name, case)");
}

backends!(a_store_of_0_5_1_keeps_its_function_tests_unnamed, a_fresh_store_has_the_column_the_index_and_the_table);
