//! Migration 0007 (`0007_binding_audit.sql`, CONTRACT-1.8b S3): `written_at` on
//! `name_bindings` and `class_bindings`, filled by the database at insert (PostgreSQL
//! `DEFAULT now()`, SQLite the insert statement's `strftime`), `NULL` for rows that existed
//! before the migration, in no hash and read by no API. Both backends.

#[macro_use]
mod common;

use std::time::{SystemTime, UNIX_EPOCH};

use common::*;
use datumesh_store::{migrate, SqliteOptions};

const NAME: &str = "rates.example.org/fx_rate";
const CLASS: &str = "rates.example.org/fx_pair";
const T1: &str = "2024-03-15T16:00:00.000000Z";
const T2: &str = "2024-03-18T10:00:00.000000Z";
const END: &str = "9999-12-31T23:59:59.999999Z";

fn now_secs() -> i64 {
    SystemTime::now().duration_since(UNIX_EPOCH).unwrap().as_secs() as i64
}

/// `written_at` of the table's rows as Unix seconds (`None` for NULL), ordered by `tx`.
fn written(db: &TestDb, table: &str) -> Vec<Option<i64>> {
    let col = db.pick("extract(epoch FROM written_at)::bigint::text", "CAST(strftime('%s', written_at) AS TEXT)");
    // coalesce: a NULL is read as the text "null".
    let sql = format!("SELECT coalesce({col}, 'null') FROM {table} ORDER BY tx");
    db.texts(&sql).into_iter().map(|r| r[0].parse::<i64>().ok()).collect()
}

fn assert_recent(w: Option<i64>) {
    let w = w.expect("written_at is not NULL");
    assert!((now_secs() - w).abs() <= 60, "written_at {w} is not within 60 s of {}", now_secs());
}

fn the_database_fills_written_at_at_insert(b: Backend) {
    let db = TestDb::migrated(b);
    let s = db.store();
    let f = s.put_descriptor(&fx_rate_descriptor()).unwrap();
    s.bind_name(NAME, &f, T1).unwrap();
    let rows = written(&db, "name_bindings");
    assert_eq!(rows.len(), 1);
    assert_recent(rows[0]);
    // Class bindings too (and the method names the binding makes, which are name bindings).
    let c = test_class();
    let hash = put_test_class(&s, &c);
    s.bind_class(CLASS, &hash, T1).unwrap();
    let rows = written(&db, "class_bindings");
    assert_eq!(rows.len(), 1);
    assert_recent(rows[0]);
    for w in written(&db, "name_bindings") {
        assert_recent(w);
    }
}

fn rows_from_before_the_migration_have_null_and_nothing_else_changes(b: Backend) {
    let db = TestDb::empty(b);
    let (old, all): (Vec<&migrate::Migration>, usize) = match b {
        Backend::Pg => (migrate::MIGRATIONS[..6].iter().collect(), migrate::MIGRATIONS.len()),
        Backend::Sqlite => (migrate::sqlite::MIGRATIONS[..3].iter().collect(), migrate::sqlite::MIGRATIONS.len()),
    };
    assert_eq!(all, old.len() + 1, "exactly one migration for the audit column, on each backend");
    if b == Backend::Pg {
        assert_eq!(migrate::MIGRATIONS[6].version, 7);
        assert_eq!(migrate::MIGRATIONS[6].name, "0007_binding_audit.sql");
    }
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
    // A function binding and a class binding, written as 0.4.1 wrote them.
    let f = fx_rate_descriptor();
    let fh = fn_hash_of(&f);
    let hex = |b: &[u8]| -> String { b.iter().map(|x| format!("{x:02x}")).collect() };
    db.exec(db.pick(
        &format!("INSERT INTO descriptors (fn_hash, bytes) VALUES ('{fh}', decode('{}', 'hex'))", hex(&f)),
        &format!("INSERT INTO descriptors (fn_hash, bytes) VALUES ('{fh}', X'{}')", hex(&f)),
    ))
    .unwrap();
    db.exec(db.pick(
        &format!("INSERT INTO name_bindings (fn_name, tx, tx_at, fn_hash) VALUES ('{NAME}', '{T1}', '{T1}'::timestamptz, '{fh}')"),
        &format!("INSERT INTO name_bindings (fn_name, tx, fn_hash) VALUES ('{NAME}', '{T1}', '{fh}')"),
    ))
    .unwrap();
    let class = b"{}";
    let ch = datumesh_core::sha256_hex(class);
    db.exec(db.pick(
        &format!("INSERT INTO classes (class_hash, bytes) VALUES ('{ch}', decode('{}', 'hex'))", hex(class)),
        &format!("INSERT INTO classes (class_hash, bytes) VALUES ('{ch}', X'{}')", hex(class)),
    ))
    .unwrap();
    db.exec(db.pick(
        &format!("INSERT INTO class_bindings (class_name, tx, tx_at, class_hash) VALUES ('{CLASS}', '{T1}', '{T1}'::timestamptz, '{ch}')"),
        &format!("INSERT INTO class_bindings (class_name, tx, class_hash) VALUES ('{CLASS}', '{T1}', '{ch}')"),
    ))
    .unwrap();

    let applied = match b {
        Backend::Pg => migrate::apply(&db.pg().bare_store()).unwrap(),
        Backend::Sqlite => migrate::sqlite::apply(&db.lite().store_with(SqliteOptions::new())).unwrap(),
    };
    assert_eq!(applied, [all as u32], "only the audit migration is applied");

    // The old rows have NULL, not the time of the migration.
    assert_eq!(written(&db, "name_bindings"), [None]);
    assert_eq!(written(&db, "class_bindings"), [None]);
    // Their hashes and what resolves are what they were.
    let s = db.bare_store();
    let got = s.resolve_binding(NAME, END).unwrap().unwrap();
    assert_eq!((got.fn_hash.as_str(), got.tx.as_str()), (fh.as_str(), T1));
    assert_eq!(s.resolve_name(NAME, END).unwrap(), Some(fh.clone()));
    assert_eq!(db.texts("SELECT fn_hash FROM name_bindings")[0][0], fh);
    assert_eq!(s.resolve_class(CLASS, END).unwrap(), Some(ch.clone()));
    assert_eq!(s.resolve_class(CLASS, T1).unwrap(), Some(ch.clone()));

    // A binding made now has written_at; the old row still has NULL.
    s.bind_name(NAME, &fh, T2).unwrap();
    assert_eq!(written(&db, "name_bindings")[0], None);
    let rows = written(&db, "name_bindings");
    assert_eq!(rows.len(), 2);
    assert_recent(rows[1]);
    let got = s.resolve_binding(NAME, END).unwrap().unwrap();
    assert_eq!((got.fn_hash.as_str(), got.tx.as_str()), (fh.as_str(), T2));
    let got = s.resolve_binding(NAME, T1).unwrap().unwrap();
    assert_eq!((got.fn_hash.as_str(), got.tx.as_str()), (fh.as_str(), T1));

    // Re-running is a no-op.
    let again = match b {
        Backend::Pg => migrate::apply(&db.pg().bare_store()).unwrap(),
        Backend::Sqlite => migrate::sqlite::apply(&db.lite().store_with(SqliteOptions::new())).unwrap(),
    };
    assert!(again.is_empty());
}

fn the_audit_column_changes_no_hash_and_no_resolve_result(b: Backend) {
    let db = TestDb::migrated(b);
    let s = db.store();
    let f = s.put_descriptor(&fx_rate_descriptor()).unwrap();
    s.bind_name(NAME, &f, T1).unwrap();
    s.bind_name(NAME, &f, T2).unwrap();
    let at = |a: &str| s.resolve_binding(NAME, a).unwrap().map(|b| (b.fn_hash, b.tx));
    assert_eq!(at(T1), Some((f.clone(), T1.to_string())));
    assert_eq!(at(END), Some((f.clone(), T2.to_string())));
    assert_eq!(s.resolve_name(NAME, END).unwrap(), Some(f.clone()));
    assert_eq!(db.texts("SELECT fn_hash FROM name_bindings ORDER BY tx"), [[f.clone()], [f]]);
}

backends!(
    the_database_fills_written_at_at_insert,
    rows_from_before_the_migration_have_null_and_nothing_else_changes,
    the_audit_column_changes_no_hash_and_no_resolve_result,
);
