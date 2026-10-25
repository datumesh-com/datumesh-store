//! Migration 0002 on a database holding 0.1.2 data: `receipts.asof` (and `asof_at`) is
//! backfilled from the stored canonical bytes, equal to the core's parse for every receipt;
//! `check` is clean afterwards; re-running is a no-op; and a writer that does not know the
//! column (0.1.2's insert statement) still stores receipts with their asof.

mod common;

use common::*;
use datumesh_core::{Cell, Receipt, ReceiptBuilder, Scalar, SigningKey, Type};
use datumesh_store::pg_unstable::postgres::Client;
use datumesh_store::{migrate, Store};

/// 0.1.2's tracking table and its receipt insert statement, verbatim.
const TRACKING_0_1_2: &str = "CREATE TABLE IF NOT EXISTS datumesh_schema_migrations (
    version integer PRIMARY KEY,
    name text NOT NULL,
    checksum text NOT NULL,
    applied_at timestamptz NOT NULL DEFAULT now()
)";
const RECEIPT_PUT_0_1_2: &str = "INSERT INTO receipts (hash, bytes, cell_id, tx, tx_at, fn_hash, publisher)
                  VALUES ($1, $2, $3, $4, datumesh_instant($4), $5, $6) ON CONFLICT (hash) DO NOTHING";
const EDGES_PUT_0_1_2: &str = "INSERT INTO edges (receipt_hash, input_hash, kind)
                SELECT $1, i, 'receipt' FROM unnest($2::text[]) AS i ON CONFLICT DO NOTHING";
const VERSION_PUT_0_1_2: &str =
    "INSERT INTO versions (cell_id, tx, tx_at, receipt_hash) VALUES ($1, $2, datumesh_instant($2), $3)";

/// The schema exactly as 0.1.2 left it: migration 1 applied and recorded.
fn at_0_1_2(c: &mut Client) {
    let m = &migrate::MIGRATIONS[0];
    assert_eq!((m.version, m.name), (1, "0001_initial.sql"));
    c.batch_execute(TRACKING_0_1_2).unwrap();
    let mut t = c.transaction().unwrap();
    t.batch_execute(m.sql).unwrap();
    t.execute(
        "INSERT INTO datumesh_schema_migrations (version, name, checksum) VALUES ($1, $2, $3)",
        &[&1i32, &m.name, &m.checksum()],
    )
    .unwrap();
    t.commit().unwrap();
}

/// Stores a receipt as 0.1.2's put_receipt did (canonical bytes, its columns, its edges).
fn put_0_1_2(c: &mut Client, bytes: &[u8]) -> Receipt {
    let r = Receipt::from_json(bytes).unwrap();
    let canonical = jcs(bytes);
    let inputs: Vec<String> = r.inputs().to_vec();
    c.execute(RECEIPT_PUT_0_1_2, &[&r.hash(), &canonical, &r.cell().id(), &r.tx(), &r.cell().fn_hash, &r.publisher()])
        .unwrap();
    if !inputs.is_empty() {
        c.execute(EDGES_PUT_0_1_2, &[&r.hash(), &inputs]).unwrap();
    }
    r
}

/// Receipts whose text holds `asof` look-alikes: a cell argument and a value and a summary
/// quoting `"asof":"..."` (an argument may not be named `asof`), non-ASCII text and escapes.
fn tricky_receipts() -> Vec<Vec<u8>> {
    let key = SigningKey::from_seed(&[7u8; 32]);
    let cell = Cell::from_json(
        br#"{"args":{"t":{"type":"instant","value":"1999-01-01T00:00:00.000000Z"},"x":{"type":"string","value":"\"asof\":\"1888-01-01T00:00:00.000000Z\""}},"fn":"f","fn_hash":"0000000000000000000000000000000000000000000000000000000000000000","ns":"example.org"}"#,
    )
    .unwrap();
    let mut out = Vec::new();
    for (i, (tx, asof)) in [
        ("2024-03-15T16:00:00.000000Z", "2024-03-15T16:00:00.000000Z"),
        ("2024-03-15T16:00:00.000000Z", "2031-12-31T23:59:59.999999Z"),
        ("0001-01-01T00:00:00.000000Z", "0001-01-01T00:00:00.000001Z"),
        ("2024-02-29T00:00:00.000000Z", "9999-12-31T23:59:59.999999Z"),
    ]
    .into_iter()
    .enumerate()
    {
        let r = ReceiptBuilder::new()
            .cell(cell.clone())
            .value(Scalar::from_lexical(Type::String, "\"asof\":\"2000-01-01T00:00:00.000000Z\"").unwrap())
            .valid(cell.args.iter().filter(|(k, _)| k.as_str() == "t").map(|(k, v)| (k.clone(), v.clone())).collect())
            .asof(asof)
            .tx(tx)
            .issued(asof)
            .publisher("example.org")
            .signer_host("example.org")
            .licence("CC-BY-4.0")
            .summary(format!("Grüße {i}: \"asof\":\"1777-07-07T07:07:07.000000Z\" \\ \u{1F600}"))
            .sign(&key)
            .unwrap();
        out.push(receipt_json(&r));
    }
    out
}

#[test]
fn migration_2_backfills_asof_from_the_stored_bytes() {
    let db = TestDb::empty();
    let mut c = db.client();
    at_0_1_2(&mut c);

    // 0.1.2 data: every receipt vector the core accepts and 0.1.2 could store (no slice
    // entries: spec-0.4.0), generated versions of one cell (late-signed and not), and the
    // tricky receipts.
    let mut all: Vec<Receipt> = Vec::new();
    for v in vectors("receipt") {
        let b = input(&v);
        if Receipt::from_json(&b).is_ok_and(|r| r.inputs().iter().all(|e| !e.starts_with("slice:"))) {
            all.push(put_0_1_2(&mut c, &b));
        }
    }
    for (i, (tx, asof)) in [
        ("2024-03-17T00:00:00.000000Z", "2024-03-17T00:00:00.000000Z"),
        ("2024-03-18T00:00:00.000000Z", "2024-03-20T12:34:56.789012Z"),
    ]
    .into_iter()
    .enumerate()
    {
        let r = put_0_1_2(&mut c, &stored_number_with(&format!("1.{}", i + 1), tx, asof));
        c.execute(VERSION_PUT_0_1_2, &[&r.cell().id(), &r.tx(), &r.hash()]).unwrap();
        all.push(r);
    }
    for b in tricky_receipts() {
        all.push(put_0_1_2(&mut c, &b));
    }
    let n: i64 = c.query_one("SELECT count(*) FROM receipts", &[]).unwrap().get(0);
    assert!(n >= 20, "{n}");

    // Migrate through a store, as `datumesh-store migrate` does (and the migrations after 2).
    let s = db.bare_store();
    assert_eq!(migrate::apply(&s).unwrap(), [2, 3]);

    // Every stored receipt's asof is the core's.
    let rows = c
        .query(
            "SELECT hash, bytes, asof, to_char(asof_at AT TIME ZONE 'UTC', 'YYYY-MM-DD\"T\"HH24:MI:SS.US\"Z\"') FROM receipts",
            &[],
        )
        .unwrap();
    assert_eq!(rows.len() as i64, n);
    for row in &rows {
        let (hash, bytes, asof, asof_at): (String, Vec<u8>, String, String) =
            (row.get(0), row.get(1), row.get(2), row.get(3));
        let r = Receipt::from_json(&bytes).unwrap();
        assert_eq!(r.hash(), hash);
        assert_eq!(asof, r.asof(), "{hash}");
        assert_eq!(asof_at, r.asof(), "{hash}");
    }
    for r in &all {
        assert!(rows.iter().any(|row| row.get::<_, String>(0) == r.hash()));
    }

    // The schema is what the migrations make; re-running is a no-op.
    let scratch = TestDb::empty();
    let diff = migrate::check(&s, &scratch.bare_store()).unwrap();
    assert!(diff.is_empty(), "{diff:#?}");
    let before = migrate::catalog(&s).unwrap();
    assert!(migrate::apply(&s).unwrap().is_empty());
    assert_eq!(migrate::catalog(&s).unwrap(), before);
    assert!(s.check_statements().unwrap() >= 20);

    // The 0.1.2 data answers through the new methods.
    let cell = all.iter().find(|r| r.tx() == "2024-03-18T00:00:00.000000Z").unwrap().cell().id();
    let at = |a: &str| s.stored_asof(&cell, a).unwrap().map(|v| (v.tx, v.asof));
    assert_eq!(at("2024-03-19T00:00:00.000000Z").unwrap().0, "2024-03-17T00:00:00.000000Z");
    assert_eq!(
        at("2024-03-21T00:00:00.000000Z").unwrap(),
        ("2024-03-18T00:00:00.000000Z".to_string(), "2024-03-20T12:34:56.789012Z".to_string())
    );
}

/// A writer that does not name the new columns (0.1.2's statement, raw SQL) still stores
/// the receipt's asof: a trigger derives it from the bytes.
#[test]
fn receipts_inserted_without_asof_get_it_from_their_bytes() {
    let db = TestDb::migrated();
    let mut c = db.client();
    for b in tricky_receipts().into_iter().chain([stored_number_with(
        "1.5",
        "2024-03-05T00:00:00.000000Z",
        "2024-03-09T00:00:00.000000Z",
    )]) {
        let r = put_0_1_2(&mut c, &b);
        let row = c
            .query_one(
                "SELECT asof, asof_at = $2::text::timestamptz FROM receipts WHERE hash = $1",
                &[&r.hash(), &r.asof()],
            )
            .unwrap();
        assert_eq!(row.get::<_, String>(0), r.asof());
        assert!(row.get::<_, bool>(1));
    }
    // put_receipt fills it too.
    let s = db.store();
    let b = stored_number_with("1.6", "2024-03-06T00:00:00.000000Z", "2024-03-07T00:00:00.000000Z");
    let h = s.put_receipt(&b).unwrap();
    let asof: String = c.query_one("SELECT asof FROM receipts WHERE hash = $1", &[&h]).unwrap().get(0);
    assert_eq!(asof, "2024-03-07T00:00:00.000000Z");
    // An asof that is not the receipt's is refused.
    let b = stored_number_with("1.7", "2024-03-08T00:00:00.000000Z", "2024-03-08T00:00:00.000000Z");
    let r = Receipt::from_json(&b).unwrap();
    let e = c
        .execute(
            "INSERT INTO receipts (hash, bytes, cell_id, tx, tx_at, asof, asof_at, fn_hash, publisher)
             VALUES ($1, $2, $3, $4, datumesh_instant($4), $5, datumesh_instant($5), $6, $7)",
            &[
                &r.hash(),
                &jcs(&b),
                &r.cell().id(),
                &r.tx(),
                &"2024-03-09T00:00:00.000000Z",
                &r.cell().fn_hash,
                &r.publisher(),
            ],
        )
        .unwrap_err();
    assert!(e.as_db_error().is_some(), "{e}");
}
