//! Migration 0003 on a database holding 0.3.0 data: the typed-argument index (`cells`,
//! `cell_args`) is backfilled from the stored canonical receipt bytes, equal to the core's
//! parse for every receipt (each argument's type and lexical form, numbers also as exact
//! decimals); `check` is clean afterwards; re-running is a no-op; the 0.3.0 data answers
//! `slice_members`; and a writer that does not know the new tables (0.3.0's insert
//! statement) still has its receipts indexed.

mod common;

use common::*;
use datumesh_core::{Arg, Cell, Receipt, ReceiptBuilder, Scalar, SigningKey, Type};
use datumesh_store::pg_unstable::postgres::Client;
use datumesh_store::{migrate, Store};

/// 0.3.0's tracking table, receipt, edge and version insert statements, verbatim.
const TRACKING_0_3_0: &str = "CREATE TABLE IF NOT EXISTS datumesh_schema_migrations (
    version integer PRIMARY KEY,
    name text NOT NULL,
    checksum text NOT NULL,
    applied_at timestamptz NOT NULL DEFAULT now()
)";
const RECEIPT_PUT_0_3_0: &str =
    "INSERT INTO receipts (hash, bytes, cell_id, tx, tx_at, asof, asof_at, fn_hash, publisher)
                  VALUES ($1, $2, $3, $4, datumesh_instant($4), $5, datumesh_instant($5), $6, $7)
                  ON CONFLICT (hash) DO NOTHING";
const EDGES_PUT_0_3_0: &str = "INSERT INTO edges (receipt_hash, input_hash, kind)
                SELECT $1, i, 'receipt' FROM unnest($2::text[]) AS i ON CONFLICT DO NOTHING";
const VERSION_PUT_0_3_0: &str =
    "INSERT INTO versions (cell_id, tx, tx_at, receipt_hash) VALUES ($1, $2, datumesh_instant($2), $3)";

/// The schema exactly as 0.3.0 left it: migrations 1 and 2 applied and recorded.
fn at_0_3_0(c: &mut Client) {
    c.batch_execute(TRACKING_0_3_0).unwrap();
    for m in &migrate::MIGRATIONS[..2] {
        let mut t = c.transaction().unwrap();
        t.batch_execute(m.sql).unwrap();
        t.execute(
            "INSERT INTO datumesh_schema_migrations (version, name, checksum) VALUES ($1, $2, $3)",
            &[&(m.version as i32), &m.name, &m.checksum()],
        )
        .unwrap();
        t.commit().unwrap();
    }
    assert_eq!((migrate::MIGRATIONS[1].version, migrate::MIGRATIONS[1].name), (2, "0002_computed_versions.sql"));
}

/// Stores a receipt as 0.3.0's put_receipt did (canonical bytes, its columns, its edges).
fn put_0_3_0(c: &mut Client, bytes: &[u8]) -> Receipt {
    let r = Receipt::from_json(bytes).unwrap();
    let inputs: Vec<String> = r.inputs().to_vec();
    c.execute(
        RECEIPT_PUT_0_3_0,
        &[&r.hash(), &jcs(bytes), &r.cell().id(), &r.tx(), &r.asof(), &r.cell().fn_hash, &r.publisher()],
    )
    .unwrap();
    if !inputs.is_empty() {
        c.execute(EDGES_PUT_0_3_0, &[&r.hash(), &inputs]).unwrap();
    }
    r
}

/// Receipts of cells with arguments of every type, escapes, non-ASCII text, long numbers.
fn typed_receipts() -> Vec<Vec<u8>> {
    let key = SigningKey::from_seed(&[11u8; 32]);
    let long = format!("-{}.{}", "9".repeat(60), "1".repeat(60));
    let mut out = Vec::new();
    for (i, n) in
        ["0", "-1.5", "10", "0.000001", long.as_str(), "123456789012345678901234567890"].into_iter().enumerate()
    {
        let cell = Cell::from_json(
            format!(
                r#"{{"args":{{"b":{{"type":"boolean","value":{}}},"blob":{{"type":"blob","value":"{}:17:text/plain"}},"d":{{"type":"date","value":"2024-02-29"}},"i":{{"type":"instant","value":"0001-01-01T00:00:00.00000{i}Z"}},"n":{{"type":"number","value":"{n}"}},"s":{{"type":"string","value":"Grüße \"{i}\" \\ 😀"}}}},"fn":"typed","fn_hash":"{}","ns":"example.org"}}"#,
                i % 2 == 0,
                "a".repeat(64),
                "0".repeat(64)
            )
            .as_bytes(),
        )
        .unwrap();
        let r = ReceiptBuilder::new()
            .cell(cell)
            .value(Scalar::from_lexical(Type::Number, n).unwrap())
            .valid(Default::default())
            .asof("2024-03-15T16:00:00.000000Z")
            .tx("2024-03-15T16:00:00.000000Z")
            .issued("2024-03-15T16:00:00.000000Z")
            .publisher("example.org")
            .signer_host("example.org")
            .licence("CC-BY-4.0")
            .summary("Typed arguments.")
            .sign(&key)
            .unwrap();
        out.push(receipt_json(&r));
    }
    // A cell with no arguments.
    let cell = Cell::from_json(
        format!(r#"{{"args":{{}},"fn":"none","fn_hash":"{}","ns":"example.org"}}"#, "1".repeat(64)).as_bytes(),
    )
    .unwrap();
    let r = ReceiptBuilder::new()
        .cell(cell)
        .value(Scalar::from_lexical(Type::Number, "1").unwrap())
        .valid(Default::default())
        .asof("2024-03-15T16:00:00.000000Z")
        .tx("2024-03-15T16:00:00.000000Z")
        .issued("2024-03-15T16:00:00.000000Z")
        .publisher("example.org")
        .signer_host("example.org")
        .licence("CC-BY-4.0")
        .summary("No arguments.")
        .sign(&key)
        .unwrap();
    out.push(receipt_json(&r));
    out
}

/// The index rows of every cell agree with the core's parse of its receipts.
fn assert_index_is_the_cores(c: &mut Client, all: &[Receipt]) {
    let cells = c.query("SELECT cell_id, fn_hash, ns, fn, nargs FROM cells", &[]).unwrap();
    let mut ids: Vec<String> = all.iter().map(|r| r.cell().id()).collect();
    ids.sort();
    ids.dedup();
    assert_eq!(cells.len(), ids.len());
    for row in &cells {
        let (id, fn_hash, ns, f, nargs): (String, String, String, String, i32) =
            (row.get(0), row.get(1), row.get(2), row.get(3), row.get(4));
        let r = all.iter().find(|r| r.cell().id() == id).unwrap_or_else(|| panic!("{id}: no receipt"));
        let cell = r.cell();
        assert_eq!(
            (fn_hash.as_str(), ns.as_str(), f.as_str()),
            (cell.fn_hash.as_str(), cell.ns.as_str(), cell.fn_name.as_str())
        );
        assert_eq!(nargs as usize, cell.args.len(), "{id}");
        let args = c
            .query(
                "SELECT name, type, value, num::text, fn_hash FROM cell_args WHERE cell_id = $1 ORDER BY name COLLATE \"C\"",
                &[&id],
            )
            .unwrap();
        assert_eq!(args.len(), cell.args.len(), "{id}");
        for (row, (name, arg)) in args.iter().zip(cell.args.iter()) {
            let Arg::Value(v) = arg else { panic!("a slice marker in a stored cell") };
            let (n, t, value, num, fh): (String, String, String, Option<String>, String) =
                (row.get(0), row.get(1), row.get(2), row.get(3), row.get(4));
            assert_eq!(&n, name, "{id}");
            assert_eq!(t, v.ty().as_str(), "{id} {name}");
            assert_eq!(value, v.lexical(), "{id} {name}");
            assert_eq!(fh, cell.fn_hash, "{id} {name}");
            // Numbers also as exact decimals: numeric's text is the canonical form again.
            match v.ty() {
                Type::Number => assert_eq!(num.as_deref(), Some(v.lexical()), "{id} {name}"),
                _ => assert_eq!(num, None, "{id} {name}"),
            }
        }
    }
}

#[test]
fn migration_3_backfills_the_typed_argument_index() {
    let db = TestDb::empty();
    let mut c = db.client();
    at_0_3_0(&mut c);

    // 0.3.0 data: every receipt vector 0.3.0 could store (no slice entries), versions of one
    // cell (late-signed and not) and receipts with arguments of every type.
    let mut all: Vec<Receipt> = Vec::new();
    for v in vectors("receipt") {
        let b = input(&v);
        match Receipt::from_json(&b) {
            Ok(r) if r.inputs().iter().all(|e| !e.starts_with("slice:")) => all.push(put_0_3_0(&mut c, &b)),
            _ => {}
        }
    }
    for (i, (tx, asof)) in [
        ("2024-03-17T00:00:00.000000Z", "2024-03-17T00:00:00.000000Z"),
        ("2024-03-18T00:00:00.000000Z", "2024-03-20T12:34:56.789012Z"),
    ]
    .into_iter()
    .enumerate()
    {
        let r = put_0_3_0(&mut c, &stored_number_with(&format!("1.{}", i + 1), tx, asof));
        c.execute(VERSION_PUT_0_3_0, &[&r.cell().id(), &r.tx(), &r.hash()]).unwrap();
        all.push(r);
    }
    for b in typed_receipts() {
        all.push(put_0_3_0(&mut c, &b));
    }
    assert!(all.len() >= 25, "{}", all.len());

    let s = db.bare_store();
    // This migration and every later one.
    let later: Vec<u32> = migrate::MIGRATIONS[2..].iter().map(|m| m.version).collect();
    assert_eq!(migrate::apply(&s).unwrap(), later);
    assert_index_is_the_cores(&mut c, &all);

    // The schema is what the migrations make; re-running is a no-op.
    let scratch = TestDb::empty();
    let diff = migrate::check(&s, &scratch.bare_store()).unwrap();
    assert!(diff.is_empty(), "{diff:#?}");
    let before = migrate::catalog(&s).unwrap();
    assert!(migrate::apply(&s).unwrap().is_empty());
    assert_eq!(migrate::catalog(&s).unwrap(), before);
    assert!(s.check_statements().unwrap() >= 30);
    for table in ["cells", "cell_args", "manifests", "manifest_members", "slice_edges"] {
        assert!(before.iter().any(|l| l.starts_with(&format!("relation r {table}"))), "{table}");
    }

    // The 0.3.0 data answers slice_members: the EUR/USD fx_rate cell over its open date.
    let r = all.iter().find(|r| r.tx() == "2024-03-18T00:00:00.000000Z").unwrap();
    let cell = r.cell();
    let sl = datumesh_core::Slice::from_json(
        format!(
            r#"{{"args":{{"base":{{"type":"string","value":"EUR"}},"date":"*","quote":{{"type":"string","value":"USD"}}}},"fn":"{}","fn_hash":"{}","ns":"{}"}}"#,
            cell.fn_name, cell.fn_hash, cell.ns
        )
        .as_bytes(),
    )
    .unwrap();
    let ms = s.slice_members(&sl, "2024-03-21T00:00:00.000000Z").unwrap();
    assert_eq!(ms.len(), 1);
    assert_eq!(ms[0].version.receipt_hash, r.hash());
    let ms = s.slice_members(&sl, "2024-03-19T00:00:00.000000Z").unwrap();
    assert_eq!(ms.len(), 1);
    assert_eq!(ms[0].version.tx, "2024-03-17T00:00:00.000000Z");
}

/// A writer that does not know the index (0.3.0's statement, raw SQL) still has its
/// receipts' cells indexed: a trigger derives the rows from the bytes, once per cell.
#[test]
fn receipts_inserted_by_a_0_3_0_writer_are_indexed() {
    let db = TestDb::migrated();
    let mut c = db.client();
    let mut all = Vec::new();
    for b in typed_receipts() {
        all.push(put_0_3_0(&mut c, &b));
    }
    for (v, tx) in [("1.5", "2024-03-05T00:00:00.000000Z"), ("1.6", "2024-03-06T00:00:00.000000Z")] {
        all.push(put_0_3_0(&mut c, &stored_number_at(v, tx)));
    }
    assert_index_is_the_cores(&mut c, &all);
    // put_receipt indexes too, and a second receipt of a cell adds no row.
    let s = db.store();
    s.put_receipt(&stored_number_at("1.7", "2024-03-07T00:00:00.000000Z")).unwrap();
    all.push(Receipt::from_json(&stored_number_at("1.7", "2024-03-07T00:00:00.000000Z")).unwrap());
    assert_index_is_the_cores(&mut c, &all);
    for (sql, what) in [
        ("UPDATE cells SET nargs = nargs", "cells"),
        ("DELETE FROM cell_args", "cell_args"),
        ("TRUNCATE cells CASCADE", "cells"),
        ("TRUNCATE cell_args", "cell_args"),
    ] {
        let e = c.batch_execute(sql).expect_err(sql);
        let msg = e.as_db_error().map(|d| d.message().to_string()).unwrap_or_default();
        assert!(msg.contains("append-only"), "{what}: {sql}: {msg}");
    }
}
