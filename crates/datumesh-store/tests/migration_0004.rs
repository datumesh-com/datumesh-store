//! Migration 0004 on a database holding 0.3.1 data: each receipt's value (`value_type`,
//! `value`, read by `slice_values`) is backfilled from the stored canonical bytes, equal to
//! the core's parse for every receipt; `check` is clean; re-running is a no-op; a writer
//! that does not name the new columns (0.3.1's insert statement) still stores them (a
//! trigger takes them from the bytes); a value that is not the receipt's is refused.

mod common;

use common::*;
use datumesh_core::{Receipt, Type};
use datumesh_store::pg_unstable::postgres::Client;
use datumesh_store::{migrate, Store};

const TRACKING: &str = "CREATE TABLE IF NOT EXISTS datumesh_schema_migrations (
    version integer PRIMARY KEY,
    name text NOT NULL,
    checksum text NOT NULL,
    applied_at timestamptz NOT NULL DEFAULT now()
)";
/// 0.3.1's receipt insert statement, verbatim.
const RECEIPT_PUT_0_3_1: &str = "INSERT INTO receipts (hash, bytes, cell_id, tx, tx_at, asof, asof_at, fn_hash, publisher)
                  VALUES ($1, $2, $3, $4, datumesh_instant($4), $5, datumesh_instant($5), $6, $7)
                  ON CONFLICT (hash) DO NOTHING";

/// The schema as 0.3.1 left it: migrations 1 to 3 applied and recorded.
fn at_0_3_1(c: &mut Client) {
    c.batch_execute(TRACKING).unwrap();
    for m in &migrate::MIGRATIONS[..3] {
        let mut t = c.transaction().unwrap();
        t.batch_execute(m.sql).unwrap();
        t.execute(
            "INSERT INTO datumesh_schema_migrations (version, name, checksum) VALUES ($1, $2, $3)",
            &[&(m.version as i32), &m.name, &m.checksum()],
        )
        .unwrap();
        t.commit().unwrap();
    }
    assert_eq!(migrate::MIGRATIONS[2].name, "0003_slices.sql");
}

fn put_0_3_1(c: &mut Client, bytes: &[u8]) -> Receipt {
    let r = Receipt::from_json(bytes).unwrap();
    c.execute(
        RECEIPT_PUT_0_3_1,
        &[&r.hash(), &jcs(bytes), &r.cell().id(), &r.tx(), &r.asof(), &r.cell().fn_hash, &r.publisher()],
    )
    .unwrap();
    r
}

/// Every receipt's value columns are the core's parse.
fn assert_values_are_the_cores(c: &mut Client, all: &[Receipt]) {
    let rows = c.query("SELECT hash, value_type, value FROM receipts", &[]).unwrap();
    assert_eq!(rows.len(), all.len());
    for row in &rows {
        let (h, t, v): (String, String, String) = (row.get(0), row.get(1), row.get(2));
        let r = all.iter().find(|r| r.hash() == h).unwrap();
        assert_eq!((t.as_str(), v.as_str()), (r.ty().as_str(), r.value().lexical()), "{h}");
    }
}

/// Receipts of every value type (the slice-lineage vectors' receipts and generated ones).
fn receipts() -> Vec<Vec<u8>> {
    let mut out: Vec<Vec<u8>> = vectors("receipt")
        .iter()
        .map(input)
        .filter(|b| Receipt::from_json(b).is_ok())
        .collect();
    let cell = slice_cell("f", &"0".repeat(64), &[("k", sc(Type::Number, "1"))]);
    for (i, v) in ["-1.5", "0", &format!("{}.{}", "1".repeat(60), "2".repeat(60))].iter().enumerate() {
        out.push(slice_receipt(&cell, v, &format!("2024-01-0{}T00:00:00.000000Z", i + 1), "2024-02-01T00:00:00.000000Z"));
    }
    out.sort();
    out.dedup();
    out
}

#[test]
fn migration_4_backfills_receipt_values() {
    let db = TestDb::empty();
    let mut c = db.client();
    at_0_3_1(&mut c);
    let mut all: Vec<Receipt> = Vec::new();
    for b in receipts() {
        let r = put_0_3_1(&mut c, &b);
        if !all.iter().any(|x| x.hash() == r.hash()) {
            all.push(r);
        }
    }
    let types: std::collections::BTreeSet<&str> = all.iter().map(|r| r.ty().as_str()).collect();
    assert!(types.len() >= 5, "{types:?}");

    let s = db.bare_store();
    assert_eq!(migrate::apply(&s).unwrap(), [4]);
    assert_values_are_the_cores(&mut c, &all);

    let scratch = TestDb::empty();
    let diff = migrate::check(&s, &scratch.bare_store()).unwrap();
    assert!(diff.is_empty(), "{diff:#?}");
    let before = migrate::catalog(&s).unwrap();
    assert!(migrate::apply(&s).unwrap().is_empty());
    assert_eq!(migrate::catalog(&s).unwrap(), before);
    assert!(s.check_statements().unwrap() >= 46);
}

#[test]
fn receipts_inserted_by_a_0_3_1_writer_get_their_values() {
    let db = TestDb::migrated();
    let mut c = db.client();
    let mut all: Vec<Receipt> = Vec::new();
    for b in receipts() {
        let r = put_0_3_1(&mut c, &b);
        if !all.iter().any(|x| x.hash() == r.hash()) {
            all.push(r);
        }
    }
    assert_values_are_the_cores(&mut c, &all);
    // put_receipt stores them too.
    let s = slice_store(&db);
    let cell = slice_cell("f", &"1".repeat(64), &[("k", sc(Type::Number, "2"))]);
    let b = slice_receipt(&cell, "42", "2024-01-01T00:00:00.000000Z", "2024-01-01T00:00:00.000000Z");
    let h = s.put_receipt(&b).unwrap();
    let row = c.query_one("SELECT value_type, value FROM receipts WHERE hash = $1", &[&h]).unwrap();
    assert_eq!((row.get::<_, String>(0), row.get::<_, String>(1)), ("number".to_string(), "42".to_string()));
    // A value that is not the receipt's is refused.
    let b = slice_receipt(&cell, "43", "2024-01-02T00:00:00.000000Z", "2024-01-02T00:00:00.000000Z");
    let r = Receipt::from_json(&b).unwrap();
    let e = c
        .execute(
            "INSERT INTO receipts (hash, bytes, cell_id, tx, tx_at, asof, asof_at, fn_hash, publisher, value_type, value)
             VALUES ($1, $2, $3, $4, datumesh_instant($4), $5, datumesh_instant($5), $6, $7, 'number', '44')",
            &[&r.hash(), &jcs(&b), &r.cell().id(), &r.tx(), &r.asof(), &r.cell().fn_hash, &r.publisher()],
        )
        .unwrap_err();
    assert!(e.as_db_error().is_some(), "{e}");
}
