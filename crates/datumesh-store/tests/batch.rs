//! Batched loading (0.4.3): `latest_many`, `receipts_many` and `put_cells`.
//!
//! * `latest_many` / `receipts_many`: one entry per input, in input order, each equal to what
//!   `latest` / `receipt` give for it.
//! * `put_cells`: exactly what `put_receipt` then (for an item with a version) `put_version`
//!   of each item in order would do: the same rows and bytes, the same checks and the same
//!   error for the first item that fails. On PostgreSQL one transaction: an error stores
//!   nothing from the call (SQLite loops and need not be atomic, so that is not asserted).
//!
//! Both backends, atomicity on PostgreSQL only.

#[macro_use]
mod common;

use common::*;
use datumesh_core::{Cell, ErrorCode, Receipt, ReceiptBuilder, Scalar, Type};
use datumesh_store::{CellWrite, Error};

fn day(d: u32) -> String {
    format!("2024-01-{d:02}T00:00:00.000000Z")
}

const TABLES: [&str; 7] = ["receipts", "versions", "cells", "cell_args", "edges", "slice_edges", "computed_versions"];

/// What an error looks like, for "the same error".
fn show<T: std::fmt::Debug>(r: Result<T, Error>) -> String {
    format!("{:?}", r.expect_err("an error"))
}

/// The slice store of `b` with the descriptor of a function `f(k: number, s: string)`.
fn setup(db: &TestDb) -> (TStore, String) {
    let s = slice_store(db);
    let h = s.put_descriptor(&stored_descriptor(&[("k", Type::Number), ("s", Type::String)], "f")).unwrap();
    (s, h)
}

fn cell_of(h: &str, k: u32, s: &str) -> Cell {
    slice_cell("f", h, &[("k", sc(Type::Number, &k.to_string())), ("s", sc(Type::String, s))])
}

/// A signed receipt of `cell`, with `inputs` (receipt hashes).
fn receipt_of(cell: &Cell, value: &str, tx: &str, asof: &str, inputs: &[String]) -> Vec<u8> {
    let r = ReceiptBuilder::new()
        .cell(cell.clone())
        .value(Scalar::from_lexical(Type::Number, value).unwrap())
        .valid(Default::default())
        .asof(asof)
        .tx(tx)
        .issued(asof)
        .publisher(SLICE_HOST)
        .signer_host(SLICE_HOST)
        .inputs(inputs.iter().cloned())
        .licence("CC0-1.0")
        .summary("A batch test value.")
        .sign(&slice_key())
        .unwrap();
    receipt_json(&r)
}

fn hash_of(bytes: &[u8]) -> String {
    Receipt::from_json(bytes).unwrap().hash().to_string()
}

fn put_one(s: &TStore, cell: &Cell, value: &str, tx: &str, asof: &str) -> String {
    let b = receipt_of(cell, value, tx, asof, &[]);
    let h = s.put_receipt(&b).unwrap();
    s.put_version(&cell.id(), tx, &h).unwrap();
    h
}

/// An item: a receipt and the (cell id, tx) of its version, if any.
type Item = (Vec<u8>, Option<(String, String)>);

fn item(cell: &Cell, value: &str, tx: &str, asof: &str) -> Item {
    (receipt_of(cell, value, tx, asof, &[]), Some((cell.id(), tx.to_string())))
}

fn writes(items: &[Item]) -> Vec<CellWrite<'_>> {
    items.iter().map(|(r, v)| CellWrite::new(r, v.as_ref().map(|(c, t)| (c.as_str(), t.as_str())))).collect()
}

/// `put_receipt` / `put_version` of each item in order, as `put_cells` is defined.
fn loop_put(s: &TStore, items: &[Item]) -> Result<Vec<String>, Error> {
    let mut out = Vec::new();
    for (r, v) in items {
        let h = s.put_receipt(r)?;
        if let Some((c, t)) = v {
            s.put_version(c, t, &h)?;
        }
        out.push(h);
    }
    Ok(out)
}

fn latest_many_equals_latest_per_id(b: Backend) {
    let db = TestDb::migrated(b);
    let (s, h) = setup(&db);
    let (a, bb, c, d, never) =
        (cell_of(&h, 1, "a"), cell_of(&h, 2, "b"), cell_of(&h, 3, "c"), cell_of(&h, 4, "d"), cell_of(&h, 5, "never"));
    // Insertion order interleaves the cells; c has three versions, d two.
    put_one(&s, &c, "1", &day(1), &day(1));
    put_one(&s, &d, "1", &day(2), &day(2));
    put_one(&s, &c, "2", &day(3), &day(3));
    put_one(&s, &bb, "1", &day(4), &day(4));
    put_one(&s, &d, "2", &day(5), &day(5));
    put_one(&s, &c, "3", &day(6), &day(8)); // late-signed: asof after tx
    let (ia, ib, ic, id, inever) = (a.id(), bb.id(), c.id(), d.id(), never.id());
    // `a` is a valid cell id that no receipt was ever stored for.
    let ids: Vec<&str> = vec![&ic, &inever, &ib, &id, &ic, &ia, &id, &inever, &ib];
    let many = s.latest_many(&ids).unwrap();
    assert_eq!(many.len(), ids.len());
    for (i, id) in ids.iter().enumerate() {
        assert_eq!(many[i], s.latest(id).unwrap(), "entry {i}");
    }
    assert_eq!(many[0].as_ref().unwrap().tx, day(6));
    assert_eq!(many[0].as_ref().unwrap().asof, day(8));
    assert_eq!(many[1], None);
    assert_eq!(many[3].as_ref().unwrap().tx, day(5));
    assert_eq!(many[5], None);
    // A single id, and the same id many times.
    assert_eq!(s.latest_many(&[&ib]).unwrap(), vec![s.latest(&ib).unwrap()]);
    let same = vec![ic.as_str(); 50];
    let many = s.latest_many(&same).unwrap();
    assert_eq!(many.len(), 50);
    assert!(many.iter().all(|v| *v == s.latest(&ic).unwrap()));
    // Empty input, empty result.
    assert_eq!(s.latest_many(&[]).unwrap(), vec![]);
}

fn latest_many_bad_id_is_latests_error(b: Backend) {
    let db = TestDb::migrated(b);
    let (s, h) = setup(&db);
    let c = cell_of(&h, 1, "a");
    put_one(&s, &c, "1", &day(1), &day(1));
    let id = c.id();
    for bad in ["", "abc", "' OR '1'='1", "é", "\0", &"a".repeat(100_000)] {
        let want = show(s.latest(bad));
        assert_eq!(show(s.latest_many(&[bad])), want, "{bad:?}");
        assert_eq!(show(s.latest_many(&[&id, bad, &id])), want, "{bad:?} in the middle");
    }
    // The first bad id is the error.
    assert_eq!(show(s.latest_many(&[&id, "abc", "é"])), show(s.latest("abc")));
    assert_eq!(show(s.latest_many(&["é", "abc"])), show(s.latest("é")));
}

fn receipts_many_equals_receipt_per_hash(b: Backend) {
    let db = TestDb::migrated(b);
    let (s, h) = setup(&db);
    let c = cell_of(&h, 1, "a");
    let h1 = put_one(&s, &c, "1", &day(1), &day(1));
    let h2 = put_one(&s, &c, "2", &day(2), &day(2));
    // A receipt stored without a version.
    let loose = receipt_of(&cell_of(&h, 2, "b"), "7", &day(3), &day(3), &[]);
    let h3 = s.put_receipt(&loose).unwrap();
    let missing = "0".repeat(64);
    let missing2 = "f".repeat(64);
    let hashes: Vec<&str> = vec![&h2, &missing, &h1, &h3, &h1, &missing2, &missing, &h2];
    let many = s.receipts_many(&hashes).unwrap();
    assert_eq!(many.len(), hashes.len());
    for (i, hh) in hashes.iter().enumerate() {
        assert_eq!(many[i], s.receipt(hh).map(|b| b.into_owned()), "entry {i}");
    }
    assert_eq!(many[3].as_deref(), Some(jcs(&loose).as_slice()));
    assert_eq!(many[1], None);
    assert!(many[0].is_some() && many[2].is_some() && many[4].is_some());
    assert_eq!(s.receipts_many(&[&h1]).unwrap(), vec![s.receipt(&h1).map(|b| b.into_owned())]);
    assert_eq!(s.receipts_many(&[&missing]).unwrap(), vec![None]);
    assert_eq!(s.receipts_many(&[]).unwrap(), Vec::<Option<Vec<u8>>>::new());
}

/// The items of the equivalence test: several cells with two arguments, one cell with three
/// versions (one late-signed), a receipt with inputs, receipts without versions, a repeated
/// receipt.
fn equivalence_items(h: &str) -> Vec<Item> {
    let mut items = Vec::new();
    for k in 1..=6u32 {
        let c = cell_of(h, k, &format!("s{}", k % 3));
        items.push(item(&c, &format!("{k}.5"), &day(k), &day(k)));
    }
    let c1 = cell_of(h, 1, "s1");
    items.push(item(&c1, "10", &day(10), &day(12)));
    items.push(item(&c1, "11", &day(13), &day(13)));
    // With inputs (edges), and a receipt that is not made a version.
    let c7 = cell_of(h, 7, "x");
    let inputs = vec![format!("{:064x}", 1), format!("{:064x}", 2)];
    items.push((receipt_of(&c7, "70", &day(14), &day(14), &inputs), Some((c7.id(), day(14)))));
    let c8 = cell_of(h, 8, "y");
    items.push((receipt_of(&c8, "80", &day(15), &day(15), &[]), None));
    // The same receipt twice (no version: a version of a stored tx is `NotLater` for
    // `put_version`, so only a receipt can repeat): idempotent.
    items.push((receipt_of(&c8, "80", &day(15), &day(15), &[]), None));
    items
}

fn put_cells_equals_the_loop(b: Backend) {
    let db1 = TestDb::migrated(b);
    let db2 = TestDb::migrated(b);
    let (s1, h1) = setup(&db1);
    let (s2, h2) = setup(&db2);
    assert_eq!(h1, h2);
    let items = equivalence_items(&h1);
    let hashes1 = s1.put_cells(&writes(&items)).unwrap();
    let hashes2 = loop_put(&s2, &items).unwrap();
    assert_eq!(hashes1, hashes2);
    assert_eq!(hashes1.len(), items.len());
    for (i, (r, _)) in items.iter().enumerate() {
        assert_eq!(hashes1[i], hash_of(r));
    }
    // Receipts byte for byte.
    for hh in &hashes1 {
        let a = s1.receipt(hh).map(|b| b.into_owned());
        assert!(a.is_some());
        assert_eq!(a, s2.receipt(hh).map(|b| b.into_owned()));
    }
    // Latest and version_asof for every cell, at several instants.
    for (r, _) in &items {
        let cell = Receipt::from_json(r).unwrap().cell().clone();
        let id = cell.id();
        assert_eq!(s1.latest(&id).unwrap(), s2.latest(&id).unwrap());
        for d in [1, 2, 6, 10, 12, 13, 14, 20] {
            assert_eq!(s1.version_asof(&id, &day(d)).unwrap(), s2.version_asof(&id, &day(d)).unwrap(), "{id} {d}");
        }
    }
    assert_eq!(s1.latest(&cell_of(&h1, 1, "s1").id()).unwrap().unwrap().tx, day(13));
    // Slice values.
    let sl = slice(&format!(r#"{{"args":{{"k":"*","s":"*"}},"fn":"f","fn_hash":"{h1}","ns":"{SLICE_HOST}"}}"#));
    for d in [1, 5, 10, 12, 13, 14, 20] {
        let v1 = s1.slice_values(&sl, &day(d)).unwrap();
        assert_eq!(v1, s2.slice_values(&sl, &day(d)).unwrap(), "day {d}");
        if d == 20 {
            assert_eq!(v1.len(), 7); // k=1..7; k=8 has no version
        }
    }
    // The same rows in every table.
    for t in TABLES {
        assert_eq!(db1.count(t), db2.count(t), "{t}");
    }
    for t in ["receipts", "versions", "cells", "cell_args", "edges"] {
        assert!(db1.count(t) > 0, "{t}");
    }
    assert_eq!(db1.count("receipts"), 10);
    assert_eq!(db1.count("versions"), 9);
}

fn put_cells_empty_call_and_repeat_are_no_ops(b: Backend) {
    let db = TestDb::migrated(b);
    let (s, h) = setup(&db);
    assert_eq!(s.put_cells(&[]).unwrap(), Vec::<String>::new());
    assert_eq!(db.count("receipts"), 0);
    let items = equivalence_items(&h);
    let first = s.put_cells(&writes(&items)).unwrap();
    let counts: Vec<i64> = TABLES.iter().map(|t| db.count(t)).collect();
    let before = s.latest(&cell_of(&h, 1, "s1").id()).unwrap();
    // Repeating the receipts of the whole call (no versions: `put_version` of a tx already
    // there is `NotLater`, so a version cannot repeat) changes nothing, same hashes.
    let receipts_only: Vec<Item> = items.iter().map(|(r, _)| (r.clone(), None)).collect();
    assert_eq!(s.put_cells(&writes(&receipts_only)).unwrap(), first);
    assert_eq!(TABLES.iter().map(|t| db.count(t)).collect::<Vec<_>>(), counts);
    assert_eq!(s.latest(&cell_of(&h, 1, "s1").id()).unwrap(), before);
    // Repeating it with its versions fails as the loop does, at the first item with one;
    // on PostgreSQL nothing changes.
    let want = {
        let db2 = TestDb::migrated(b);
        let (s2, _) = setup(&db2);
        loop_put(&s2, &items).unwrap();
        show(loop_put(&s2, &items))
    };
    let got = s.put_cells(&writes(&items)).unwrap_err();
    assert!(matches!(got, Error::NotLater { what: "version", .. }), "{got:?}");
    assert_eq!(format!("{got:?}"), want);
    if b == Backend::Pg {
        assert_eq!(TABLES.iter().map(|t| db.count(t)).collect::<Vec<_>>(), counts);
    }
}

fn put_cells_same_cell_twice_with_increasing_tx_stores_both(b: Backend) {
    let db = TestDb::migrated(b);
    let (s, h) = setup(&db);
    let c = cell_of(&h, 1, "a");
    let items = vec![item(&c, "1", &day(1), &day(1)), item(&c, "2", &day(2), &day(2))];
    let hashes = s.put_cells(&writes(&items)).unwrap();
    assert_eq!(hashes.len(), 2);
    assert_eq!(db.count("versions"), 2);
    let vs = s.versions(&c.id(), &day(1), &day(2)).unwrap();
    assert_eq!(vs.iter().map(|v| v.receipt_hash.clone()).collect::<Vec<_>>(), hashes);
    assert_eq!(s.latest(&c.id()).unwrap().unwrap().tx, day(2));
    // A later call continues the history, and an earlier tx is refused.
    s.put_cells(&writes(&[item(&c, "3", &day(3), &day(3))])).unwrap();
    assert_eq!(db.count("versions"), 3);
    // The same tx as the latest is not later.
    let again = [item(&c, "0", &day(3), &day(3))];
    assert!(matches!(s.put_cells(&writes(&again)).unwrap_err(), Error::NotLater { what: "version", .. }));
}

fn put_cells_same_cell_not_later_in_the_same_call_is_not_later(b: Backend) {
    let db = TestDb::migrated(b);
    let (s, h) = setup(&db);
    let c = cell_of(&h, 1, "a");
    let other = cell_of(&h, 2, "b");
    for second_tx in [day(2), day(1)] {
        let items = vec![
            item(&other, "9", &day(1), &day(1)),
            item(&c, "1", &day(2), &day(2)),
            item(&c, "2", &second_tx, &second_tx),
        ];
        let got = s.put_cells(&writes(&items)).unwrap_err();
        match &got {
            Error::NotLater { what: "version", key, given, latest } => {
                assert_eq!((key, given, latest), (&c.id(), &second_tx, &day(2)));
            }
            e => panic!("not NotLater: {e:?}"),
        }
        // The same as the loop on a fresh store.
        let db2 = TestDb::migrated(b);
        let (s2, _) = setup(&db2);
        assert_eq!(format!("{got:?}"), show(loop_put(&s2, &items)));
        if b == Backend::Pg {
            assert_eq!(db.count("receipts"), 0);
            assert_eq!(db.count("versions"), 0);
        } else {
            break; // SQLite need not be atomic: the first round may have stored rows
        }
    }
}

fn put_cells_not_later_than_a_stored_version(b: Backend) {
    let db = TestDb::migrated(b);
    let (s, h) = setup(&db);
    let c = cell_of(&h, 1, "a");
    put_one(&s, &c, "1", &day(5), &day(5));
    let items = vec![item(&c, "2", &day(4), &day(4))];
    let e = s.put_cells(&writes(&items)).unwrap_err();
    assert!(matches!(e, Error::NotLater { what: "version", .. }), "{e:?}");
    // A receipt only (no version) at that tx is fine.
    let loose = vec![(receipt_of(&c, "3", &day(4), &day(4), &[]), None)];
    s.put_cells(&writes(&loose)).unwrap();
    assert_eq!(s.latest(&c.id()).unwrap().unwrap().tx, day(5));
}

fn put_cells_bad_signature_is_put_receipts_error_and_atomic(b: Backend) {
    let db = TestDb::migrated(b);
    let (s, h) = setup(&db);
    let mut items: Vec<Item> = (1..=5u32).map(|k| item(&cell_of(&h, k, "a"), "1", &day(k), &day(k))).collect();
    // The third item: another signature over the same payload.
    let good = items[2].0.clone();
    let sig = Receipt::from_json(&good).unwrap().sig().to_string();
    let forged_sig = format!("{}{}", if sig.starts_with('0') { "1" } else { "0" }, &sig[1..]);
    let forged = String::from_utf8(good).unwrap().replace(&sig, &forged_sig).into_bytes();
    items[2].0 = forged.clone();
    let want = show(s.put_receipt(&forged));
    assert_eq!(db.count("receipts"), 0);
    let got = s.put_cells(&writes(&items)).unwrap_err();
    assert_eq!(got.code(), Some(ErrorCode::Signature));
    assert_eq!(format!("{got:?}"), want);
    if b == Backend::Pg {
        // All or nothing.
        for t in TABLES {
            assert_eq!(db.count(t), 0, "{t}");
        }
        assert_eq!(s.latest(&cell_of(&h, 1, "a").id()).unwrap(), None);
    }
    // The first item that fails is the error, whichever fails later (a missing key too).
    // (a fresh store: SQLite need not be atomic)
    items[3].0 = Vec::new();
    let db2 = TestDb::migrated(b);
    let (s2, _) = setup(&db2);
    assert_eq!(format!("{:?}", s2.put_cells(&writes(&items)).unwrap_err()), want);
}

fn put_cells_suspended_signer_host(b: Backend) {
    let db = TestDb::migrated(b);
    let (s, h) = setup(&db);
    let ok = item(&cell_of(&h, 1, "a"), "1", &day(1), &day(1));
    let (r2, _) = item(&cell_of(&h, 2, "a"), "1", &day(2), &day(2));
    // Another receipt, stored before the suspension, is still idempotent.
    let stored = s.put_receipt(&ok.0).unwrap();
    s.record_namespace_failure(SLICE_HOST, &day(3), 1).unwrap();
    let items = vec![ok.clone(), (r2.clone(), Some((cell_of(&h, 2, "a").id(), day(2))))];
    let want = show(s.put_receipt(&r2));
    let got = s.put_cells(&writes(&items)).unwrap_err();
    assert!(matches!(got, Error::NamespaceSuspended { .. }), "{got:?}");
    assert_eq!(format!("{got:?}"), want);
    assert!(s.receipt(&stored).is_some());
    if b == Backend::Pg {
        assert_eq!(db.count("receipts"), 1);
        assert_eq!(db.count("versions"), 0);
    }
}

fn put_cells_stored_hash_with_other_bytes_is_a_conflict(b: Backend) {
    let db = TestDb::migrated(b);
    let (s, h) = setup(&db);
    let items: Vec<Item> = (1..=3u32).map(|k| item(&cell_of(&h, k, "a"), "1", &day(k), &day(k))).collect();
    // Plant other bytes under the second receipt's hash, as the idempotence tests do.
    let r = Receipt::from_json(&items[1].0).unwrap();
    let mut v = datumesh_core::json::parse(&items[1].0).unwrap();
    if let datumesh_core::json::Value::Object(members) = &mut v {
        for (k, val) in members.iter_mut() {
            if k == "sig" {
                *val = datumesh_core::json::Value::String("planted".into());
            }
        }
    }
    let planted = v.to_jcs();
    assert_ne!(planted, items[1].0);
    db.plant_receipt(r.hash(), &planted, &r);
    let want = {
        let e = s.put_receipt(&items[1].0).unwrap_err();
        assert!(matches!(e, Error::Conflict { .. }), "{e:?}");
        format!("{e:?}")
    };
    let got = s.put_cells(&writes(&items)).unwrap_err();
    assert!(matches!(&got, Error::Conflict { hash } if hash == r.hash()), "{got:?}");
    assert_eq!(format!("{got:?}"), want);
    if b == Backend::Pg {
        assert_eq!(db.count("receipts"), 1); // only the planted row
        assert_eq!(db.count("versions"), 0);
    }
    assert_eq!(s.receipt(r.hash()).map(|b| b.into_owned()), Some(planted));
}

fn put_cells_version_tx_not_the_receipts_is_a_mismatch(b: Backend) {
    let db = TestDb::migrated(b);
    let (s, h) = setup(&db);
    let mut items: Vec<Item> = (1..=3u32).map(|k| item(&cell_of(&h, k, "a"), "1", &day(k), &day(k))).collect();
    let c2 = cell_of(&h, 2, "a");
    items[1].1 = Some((c2.id(), day(9)));
    let want = show(loop_put(&setup(&TestDb::migrated(b)).0, &items));
    let got = s.put_cells(&writes(&items)).unwrap_err();
    assert!(matches!(got, Error::Mismatch { what: "tx", .. }), "{got:?}");
    assert_eq!(format!("{got:?}"), want);
    // A version naming another cell is a mismatch of the cell id (a fresh store: SQLite
    // need not be atomic).
    let db2 = TestDb::migrated(b);
    let (s, _) = setup(&db2);
    items[1].1 = Some((cell_of(&h, 3, "a").id(), day(2)));
    let got = s.put_cells(&writes(&items)).unwrap_err();
    assert!(matches!(got, Error::Mismatch { what: "cell_id", .. }), "{got:?}");
    if b == Backend::Pg {
        for t in TABLES {
            assert_eq!(db.count(t), 0, "{t}");
            assert_eq!(db2.count(t), 0, "{t}");
        }
    }
}

fn put_cells_returns_the_receipts_own_hash_for_its_version(b: Backend) {
    // The version takes the hash of the item's own receipt, stored in the same call.
    let db = TestDb::migrated(b);
    let (s, h) = setup(&db);
    let c = cell_of(&h, 1, "a");
    let it = item(&c, "1", &day(1), &day(1));
    let hashes = s.put_cells(&writes(std::slice::from_ref(&it))).unwrap();
    assert_eq!(s.latest(&c.id()).unwrap().unwrap().receipt_hash, hashes[0]);
    assert_eq!(hashes[0], hash_of(&it.0));
}

backends!(
    latest_many_equals_latest_per_id,
    latest_many_bad_id_is_latests_error,
    receipts_many_equals_receipt_per_hash,
    put_cells_equals_the_loop,
    put_cells_empty_call_and_repeat_are_no_ops,
    put_cells_same_cell_twice_with_increasing_tx_stores_both,
    put_cells_same_cell_not_later_in_the_same_call_is_not_later,
    put_cells_not_later_than_a_stored_version,
    put_cells_bad_signature_is_put_receipts_error_and_atomic,
    put_cells_suspended_signer_host,
    put_cells_stored_hash_with_other_bytes_is_a_conflict,
    put_cells_version_tx_not_the_receipts_is_a_mismatch,
    put_cells_returns_the_receipts_own_hash_for_its_version,
);
