//! CONTRACT-perf R-T1 (migration 0008, S6): the cell index runs once per statement, and the
//! `cells` and `cell_args` rows are exactly the ones the per-row trigger makes. PostgreSQL only.
//!
//! Two fresh databases, both fully migrated. In B the per-row trigger is restored by test SQL;
//! the same mixed load goes into both and the two tables must be equal row for row.

#[macro_use]
mod common;

use common::*;
use datumesh_core::{Receipt, Type};
use datumesh_store::{migrate, CellWrite, PgStore, Store};

fn day(d: u32) -> String {
    format!("2024-01-01T{:02}:{:02}:00.000000Z", d / 60, d % 60)
}

fn triggers(db: &PgDb) -> Vec<String> {
    let rows = db
        .client()
        .query(
            "SELECT tgname FROM pg_trigger WHERE tgrelid = 'receipts'::regclass AND NOT tgisinternal ORDER BY 1",
            &[],
        )
        .unwrap();
    rows.iter().map(|r| r.get::<_, String>(0)).collect()
}

/// Is trigger `name` on `receipts` a statement trigger (not per row)?
fn is_statement_trigger(db: &PgDb, name: &str) -> bool {
    let rows = db
        .client()
        .query("SELECT (tgtype & 1) = 0 FROM pg_trigger WHERE tgrelid = 'receipts'::regclass AND tgname = $1", &[&name])
        .unwrap();
    rows.len() == 1 && rows[0].get::<_, bool>(0)
}

fn rows(db: &PgDb, sql: &str) -> Vec<String> {
    db.client().query(sql, &[]).unwrap().iter().map(|r| r.get::<_, String>(0)).collect()
}

fn n_of(db: &PgDb, table: &str) -> i64 {
    db.client().query_one(&format!("SELECT count(*) FROM {table}"), &[]).unwrap().get(0)
}

fn cells_rows(db: &PgDb) -> Vec<String> {
    rows(db, "SELECT c::text FROM cells c ORDER BY c.cell_id")
}

fn args_rows(db: &PgDb) -> Vec<String> {
    rows(db, "SELECT a::text FROM cell_args a ORDER BY a.cell_id, a.name")
}

/// A signed receipt of `cell` with a number value.
fn rcpt(cell: &datumesh_core::Cell, value: &str, tx: u32) -> Vec<u8> {
    slice_receipt(cell, value, &day(tx), &day(tx))
}

type Item = (Vec<u8>, Option<(String, String)>);

fn item(cell: &datumesh_core::Cell, value: &str, tx: u32) -> Item {
    (rcpt(cell, value, tx), Some((cell.id(), day(tx))))
}

fn writes(items: &[Item]) -> Vec<CellWrite<'_>> {
    items.iter().map(|(r, v)| CellWrite::new(r, v.as_ref().map(|(c, t)| (c.as_str(), t.as_str())))).collect()
}

struct Fns {
    f0: String,
    f1: String,
    f4: String,
}

fn setup(db: &PgDb) -> (PgStore, Fns) {
    let s = slice_store_pg(db);
    let f0 = s.put_descriptor(&stored_descriptor(&[], "f0")).unwrap();
    let f1 = s.put_descriptor(&stored_descriptor(&[("s", Type::String)], "f1")).unwrap();
    let f4 = s
        .put_descriptor(&stored_descriptor(
            &[("s", Type::String), ("n", Type::Number), ("d", Type::Date), ("b", Type::Boolean)],
            "f4",
        ))
        .unwrap();
    (s, Fns { f0, f1, f4 })
}

fn c0(f: &Fns) -> datumesh_core::Cell {
    slice_cell("f0", &f.f0, &[])
}

fn c1(f: &Fns, s: &str) -> datumesh_core::Cell {
    slice_cell("f1", &f.f1, &[("s", sc(Type::String, s))])
}

fn c4(f: &Fns, s: &str, n: &str, d: &str, b: &str) -> datumesh_core::Cell {
    slice_cell(
        "f4",
        &f.f4,
        &[
            ("s", sc(Type::String, s)),
            ("n", sc(Type::Number, n)),
            ("d", sc(Type::Date, d)),
            ("b", sc(Type::Boolean, b)),
        ],
    )
}

/// The mixed load, the same on both databases.
fn load(s: &PgStore, f: &Fns) {
    // Single put_receipt / put_version calls.
    let c = c0(f);
    let h = s.put_receipt(&rcpt(&c, "1", 1)).unwrap();
    s.put_version(&c.id(), &day(1), &h).unwrap();
    let c = c1(f, "single");
    let h = s.put_receipt(&rcpt(&c, "2", 2)).unwrap();
    s.put_version(&c.id(), &day(2), &h).unwrap();
    // A receipt without a version.
    s.put_receipt(&rcpt(&c4(f, "loose", "7", "2024-02-01", "true"), "3", 3)).unwrap();

    // A put_cells call of one item, of three, and of many (canonical decimals only: 1e3 and 10.50 are refused by the core).
    s.put_cells(&writes(&[item(&c1(f, "one"), "4", 4)])).unwrap();
    s.put_cells(&writes(&[
        item(&c4(f, "a", "1000", "2024-03-01", "false"), "5", 5),
        item(&c4(f, "a", "-0.5", "2024-03-02", "true"), "6", 6),
        item(&c4(f, "a", "10.5", "2024-03-03", "false"), "7", 7),
    ]))
    .unwrap();
    let mut many: Vec<Item> = Vec::new();
    for k in 0..40u32 {
        let t = 10 + k;
        match k % 4 {
            0 => many.push(item(&c0(f), &k.to_string(), t)),
            1 => many.push(item(&c1(f, &format!("m{k}")), &k.to_string(), t)),
            2 => many.push(item(&c4(f, &format!("m{k}"), &format!("{k}.25"), "2024-04-01", "true"), "1", t)),
            _ => many.push(item(&c4(f, "same", &k.to_string(), "2024-04-02", "false"), "1", t)),
        }
    }
    s.put_cells(&writes(&many)).unwrap();

    // The same cell twice in one call (two versions).
    let twice = c4(f, "twice", "2", "2024-05-01", "true");
    s.put_cells(&writes(&[item(&twice, "1", 60), item(&twice, "2", 61), item(&c1(f, "next-to-twice"), "9", 62)]))
        .unwrap();

    // A cell indexed by an earlier call gets a new version (and a new cell beside it).
    s.put_cells(&writes(&[item(&c1(f, "single"), "20", 70), item(&c1(f, "fresh"), "21", 71)])).unwrap();

    // A repeated identical call: a no-op (a version cannot repeat: `NotLater`, so the items
    // have none).
    let mut again = vec![item(&c1(f, "again"), "30", 80), item(&c4(f, "again", "3", "2024-06-01", "false"), "31", 81)];
    for i in &mut again {
        i.1 = None;
    }
    let first = s.put_cells(&writes(&again)).unwrap();
    assert_eq!(s.put_cells(&writes(&again)).unwrap(), first);

    // A failing call writes nothing (the third of four has a bad signature).
    let mut bad = vec![
        item(&c1(f, "doomed-1"), "1", 90),
        item(&c4(f, "doomed-2", "5", "2024-07-01", "true"), "1", 91),
        item(&c1(f, "doomed-3"), "1", 92),
        item(&c0(f), "1", 93),
    ];
    let good = bad[2].0.clone();
    let sig = Receipt::from_json(&good).unwrap().sig().to_string();
    let forged = format!("{}{}", if sig.starts_with('0') { "1" } else { "0" }, &sig[1..]);
    bad[2].0 = String::from_utf8(good).unwrap().replace(&sig, &forged).into_bytes();
    assert!(s.put_cells(&writes(&bad)).is_err());
}

#[test]
fn per_statement_trigger_makes_the_rows_of_the_per_row_trigger() {
    let (da, db) = (PgDb::migrated(), PgDb::migrated());

    // Migration 0008 is registered as version 8 and installs the statement trigger.
    let last = migrate::MIGRATIONS.last().unwrap();
    assert_eq!((last.version, last.name), (8, "0008_index_cell_per_statement.sql"));
    assert_eq!(
        triggers(&da).iter().filter(|t| t.starts_with("receipts_index_cell")).collect::<Vec<_>>(),
        ["receipts_index_cells"]
    );
    assert!(is_statement_trigger(&da, "receipts_index_cells"));
    assert_eq!(
        rows(&da, "SELECT version::text FROM datumesh_schema_migrations ORDER BY version").last().map(String::as_str),
        Some("8")
    );

    // B: the per-row trigger of 0.4.2 instead.
    db.client()
        .batch_execute(
            "DROP TRIGGER receipts_index_cells ON receipts;
             CREATE TRIGGER receipts_index_cell AFTER INSERT ON receipts
               FOR EACH ROW EXECUTE FUNCTION datumesh_receipts_index_cell();",
        )
        .unwrap();
    assert_eq!(
        triggers(&db).iter().filter(|t| t.starts_with("receipts_index_cell")).collect::<Vec<_>>(),
        ["receipts_index_cell"]
    );
    assert!(!is_statement_trigger(&db, "receipts_index_cell"));

    let (sa, fa) = setup(&da);
    let (sb, fb) = setup(&db);
    assert_eq!(fa.f4, fb.f4);
    load(&sa, &fa);
    load(&sb, &fb);

    // The rows are there, and equal in every column.
    let (ca, cb) = (cells_rows(&da), cells_rows(&db));
    let (aa, ab) = (args_rows(&da), args_rows(&db));
    // 1 f0 + 10 f0 in the many; distinct cells: see the counts of the receipts' cells.
    let distinct: i64 = da.client().query_one("SELECT count(DISTINCT cell_id) FROM receipts", &[]).unwrap().get(0);
    assert_eq!(ca.len() as i64, distinct);
    assert!(ca.len() > 30);
    assert!(aa.len() > 80);
    assert_eq!(ca, cb, "cells");
    assert_eq!(aa, ab, "cell_args");
    assert_eq!(n_of(&da, "receipts"), n_of(&db, "receipts"));
    assert_eq!(n_of(&da, "versions"), n_of(&db, "versions"));
    // The failing call stored nothing in either database.
    for d in [&da, &db] {
        let n: i64 = d
            .client()
            .query_one("SELECT count(*) FROM cells WHERE fn = 'f1' AND cell_id IN (SELECT cell_id FROM cell_args WHERE value LIKE 'doomed%')", &[])
            .unwrap()
            .get(0);
        assert_eq!(n, 0);
    }
    // The numbers kept their canonical text, and `num` is set for numbers only.
    let nums =
        rows(&da, "SELECT value || ' ' || coalesce(num::text, 'null') FROM cell_args WHERE name = 'n' ORDER BY 1");
    assert!(nums.iter().all(|r| !r.ends_with(" null")));
    let strs = rows(&da, "SELECT count(*)::text FROM cell_args WHERE name <> 'n' AND num IS NOT NULL");
    assert_eq!(strs, ["0"]);

    // The migration is applied: nothing more to apply.
    assert_eq!(migrate::apply(&sa).unwrap(), Vec::<u32>::new());
}
