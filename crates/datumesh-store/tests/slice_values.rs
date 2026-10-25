//! `slice_values` and `slice_changed_after` (0.3.2).
//!
//! * `slice_values(slice, A)`: the members `slice_members(slice, A)` gives, in the same order
//!   and by the same answering rule, as each member's version (cell, tx, receipt hash, asof),
//!   its typed value and its cell (with its arguments, which the engine hands to the runtime),
//!   without the receipt bytes (value and cell are read from stored columns and the
//!   typed-argument index, not by parsing receipts).
//! * `slice_changed_after(slice, A)`: whether a matching cell of the slice has a stored
//!   version that could answer differently after A: a version with `tx > A` or a receipt
//!   `asof > A` (its answering start is after A). One indexed query.

mod common;

use common::*;
use datumesh_core::{ErrorCode, Receipt, ReceiptBuilder, Scalar, Type};
use datumesh_store::{PgStore, SliceValue, Store};

fn day(d: u32) -> String {
    format!("2024-01-{d:02}T00:00:00.000000Z")
}

const END: &str = "9999-12-31T23:59:59.999999Z";

/// `slice_values` agrees with `slice_members` at `asof`: same versions in the same order,
/// and each value is the receipt's.
fn agree(s: &PgStore, sl: &datumesh_core::Slice, asof: &str) -> Vec<SliceValue> {
    let ms = s.slice_members(sl, asof).unwrap();
    let vs = s.slice_values(sl, asof).unwrap();
    assert_eq!(ms.len(), vs.len());
    for (m, v) in ms.iter().zip(&vs) {
        assert_eq!(m.version, v.version);
        let r = Receipt::from_json(&m.receipt).unwrap();
        assert_eq!(&v.value, r.value());
        assert_eq!(&v.cell, r.cell());
        assert_eq!(v.cell.id(), v.version.cell_id);
    }
    vs
}

/// A receipt of `cell` with any typed value.
fn receipt_with(cell: &datumesh_core::Cell, value: Scalar, tx: &str, asof: &str) -> Vec<u8> {
    let r = ReceiptBuilder::new()
        .cell(cell.clone())
        .value(value)
        .valid(Default::default())
        .asof(asof)
        .tx(tx)
        .issued(asof)
        .publisher(SLICE_HOST)
        .signer_host(SLICE_HOST)
        .licence("CC0-1.0")
        .summary("A slice test value.")
        .sign(&slice_key())
        .unwrap();
    receipt_json(&r)
}

fn open(h: &str) -> datumesh_core::Slice {
    slice(&format!(r#"{{"args":{{"k":"*"}},"fn":"f","fn_hash":"{h}","ns":"{SLICE_HOST}"}}"#))
}

#[test]
fn values_of_every_type_in_slice_members_order() {
    let db = TestDb::migrated();
    let s = slice_store(&db);
    let h = s.put_descriptor(&stored_descriptor(&[("k", Type::Number)], "f")).unwrap();
    let blob = format!("{}:17:text/plain", "a".repeat(64));
    let values = [
        sc(Type::Number, "-1.5"),
        sc(Type::String, "Grüße \"x\" \\ \u{1F600}"),
        sc(Type::Boolean, "true"),
        sc(Type::Boolean, "false"),
        sc(Type::Date, "2024-02-29"),
        sc(Type::Instant, "2024-03-15T16:00:00.000001Z"),
        sc(Type::Blob, &blob),
        sc(Type::String, ""),
        sc(Type::Number, &format!("-{}.{}", "9".repeat(60), "1".repeat(60))),
    ];
    // Keys in reverse so that key order is not insertion order.
    for (i, v) in values.iter().enumerate() {
        let cell = slice_cell("f", &h, &[("k", sc(Type::Number, &format!("{}", 100 - i)))]);
        let b = receipt_with(&cell, v.clone(), &day(1), &day(1));
        let hash = s.put_receipt(&b).unwrap();
        s.put_version(&cell.id(), &day(1), &hash).unwrap();
    }
    let vs = agree(&s, &open(&h), &day(2));
    let mut want: Vec<Scalar> = values.to_vec();
    want.reverse();
    assert_eq!(vs.iter().map(|v| v.value.clone()).collect::<Vec<_>>(), want);
    // Ranged and bound slices too.
    let ranged = slice(&format!(
        r#"{{"args":{{"k":{{"hi":"98","lo":"95","type":"number"}}}},"fn":"f","fn_hash":"{h}","ns":"{SLICE_HOST}"}}"#
    ));
    assert_eq!(agree(&s, &ranged, &day(2)).len(), 4);
}

#[test]
fn values_answer_as_slice_members_does() {
    let db = TestDb::migrated();
    let s = slice_store(&db);
    let h = s.put_descriptor(&stored_descriptor(&[("k", Type::Number)], "f")).unwrap();
    let cell = |k: &str| slice_cell("f", &h, &[("k", sc(Type::Number, k))]);
    // k=1: v1 at day 2, a correction at day 6; k=2 late-signed (tx 3, asof 8); k=3 from day 10.
    put_member(&s, &cell("1"), "100", &day(2), &day(2));
    put_member(&s, &cell("1"), "101", &day(6), &day(6));
    put_member(&s, &cell("2"), "200", &day(3), &day(8));
    put_member(&s, &cell("3"), "300", &day(10), &day(10));
    let sl = open(&h);
    let vals = |a: &str| -> Vec<String> { agree(&s, &sl, a).iter().map(|v| v.value.lexical().to_string()).collect() };
    assert!(vals(&day(1)).is_empty());
    assert_eq!(vals(&day(2)), ["100"]);
    assert_eq!(vals(&day(5)), ["100"]);
    assert_eq!(vals(&day(6)), ["101"]);
    assert_eq!(vals(&day(8)), ["101", "200"]);
    assert_eq!(vals(END), ["101", "200", "300"]);
    // A version slice: versions_between's versions, with values.
    let vs = slice(&format!(
        r#"{{"cell":{},"from":"{}","to":"{}"}}"#,
        String::from_utf8(cell("1").canonical()).unwrap(),
        day(1),
        day(9)
    ));
    assert_eq!(vals_of(&agree(&s, &vs, END)), ["100", "101"]);
    assert_eq!(vals_of(&agree(&s, &vs, &day(5))), ["100"]);
    // Malformed asof.
    assert_eq!(s.slice_values(&sl, "x").unwrap_err().code(), Some(ErrorCode::CanonInstant));
    assert_eq!(s.slice_changed_after(&sl, "x").unwrap_err().code(), Some(ErrorCode::CanonInstant));
}

fn vals_of(vs: &[SliceValue]) -> Vec<String> {
    vs.iter().map(|v| v.value.lexical().to_string()).collect()
}

#[test]
fn changed_after_sees_later_versions_of_matching_cells_only() {
    let db = TestDb::migrated();
    let s = slice_store(&db);
    let args = [("a", Type::String), ("k", Type::Number)];
    let h = s.put_descriptor(&stored_descriptor(&args, "g")).unwrap();
    let cell = |a: &str, k: &str| slice_cell("g", &h, &[("a", sc(Type::String, a)), ("k", sc(Type::Number, k))]);
    let text = |args: &str| slice(&format!(r#"{{"args":{{{args}}},"fn":"g","fn_hash":"{h}","ns":"{SLICE_HOST}"}}"#));
    let all = text(r#""a":"*","k":"*""#);
    let x_low = text(r#""a":{"type":"string","value":"x"},"k":{"hi":"10","lo":"0","type":"number"}"#);
    // Empty: nothing ever changes.
    assert!(!s.slice_changed_after(&all, &day(1)).unwrap());
    put_member(&s, &cell("x", "1"), "1", &day(2), &day(2));
    put_member(&s, &cell("x", "5"), "5", &day(3), &day(3));
    let ch = |sl: &datumesh_core::Slice, a: &str| s.slice_changed_after(sl, a).unwrap();
    assert!(ch(&all, &day(1)));
    assert!(ch(&all, "2024-01-02T23:59:59.999999Z"));
    assert!(!ch(&all, &day(3)));
    assert!(!ch(&x_low, END));
    // A version of a cell outside the slice (another a, a k out of range) is no change of it.
    put_member(&s, &cell("y", "5"), "6", &day(9), &day(9));
    put_member(&s, &cell("x", "50"), "7", &day(9), &day(9));
    assert!(!ch(&x_low, &day(3)));
    assert!(ch(&all, &day(3)));
    // A correction of a member at day 6: changed for any A before 6.
    put_member(&s, &cell("x", "5"), "5.5", &day(6), &day(6));
    assert!(ch(&x_low, &day(5)));
    assert!(!ch(&x_low, &day(6)));
    // A late-signed version (tx 7, receipt asof 12): changed until its asof.
    put_member(&s, &cell("x", "1"), "1.5", &day(7), &day(12));
    assert!(ch(&x_low, &day(8)));
    assert!(ch(&x_low, "2024-01-11T23:59:59.999999Z"));
    assert!(!ch(&x_low, &day(12)));
    // A receipt stored without a version, or as a computed version, is no change.
    let c9 = cell("x", "9");
    let r9 = receipt_with(&c9, sc(Type::Number, "9"), &day(20), &day(20));
    let h9 = s.put_receipt(&r9).unwrap();
    s.put_computed(&c9.id(), &day(20), &h9).unwrap();
    assert!(!ch(&x_low, &day(12)));
    // A version slice: versions of its cell with tx in [from, to] answering after A.
    let vs = slice(&format!(
        r#"{{"cell":{},"from":"{}","to":"{}"}}"#,
        String::from_utf8(cell("x", "5").canonical()).unwrap(),
        day(1),
        day(4)
    ));
    assert!(ch(&vs, &day(2)));
    assert!(!ch(&vs, &day(3)));
    // (The correction at day 6 is outside [from, to].)
    assert!(!ch(&vs, &day(5)));
}

/// 100,000 cells: `slice_values` agrees with `slice_members`; `slice_changed_after` scans no
/// table sequentially and needs no member (an index of receipts by function and asof).
#[test]
fn values_and_changes_over_100000_cells() {
    let db = TestDb::migrated();
    let s = std::sync::Arc::new(db.store_with(datumesh_store::PoolOptions::new().max_size(8)));
    s.put_namespace(SLICE_HOST, &slice_keydoc(), VALIDATED_AT).unwrap();
    let h = s.put_descriptor(&stored_descriptor(&[("k", Type::Number)], "big")).unwrap();
    const N: usize = 100_000;
    let threads: Vec<_> = (0..8)
        .map(|t| {
            let (s, h) = (s.clone(), h.clone());
            std::thread::spawn(move || {
                for i in (t..N).step_by(8) {
                    let cell = slice_cell("f", &h, &[("k", sc(Type::Number, &i.to_string()))]);
                    put_member(&s, &cell, &i.to_string(), &day(1), &day(1));
                }
            })
        })
        .collect();
    for t in threads {
        t.join().unwrap();
    }
    db.client().batch_execute("ANALYZE").unwrap();
    let sl = open(&h);
    let vs = agree(&s, &sl, &day(2));
    assert_eq!(vs.len(), N);
    assert!(vs.iter().enumerate().all(|(i, v)| v.value.lexical() == i.to_string()));
    let seq_scans = || -> i64 {
        std::thread::sleep(std::time::Duration::from_millis(2500));
        db.client()
            .query_one(
                "SELECT coalesce(sum(seq_scan), 0)::bigint FROM pg_stat_user_tables
                  WHERE relname IN ('cells', 'cell_args', 'receipts', 'versions')",
                &[],
            )
            .unwrap()
            .get(0)
    };
    // Unchanged after A (the expensive answer, every member looked at if it were a scan).
    let before = seq_scans();
    let start = std::time::Instant::now();
    assert!(!s.slice_changed_after(&sl, &day(1)).unwrap());
    let took = start.elapsed();
    assert_eq!(seq_scans(), before, "slice_changed_after scanned a table sequentially");
    assert!(took < std::time::Duration::from_millis(500), "{took:?}");
    // Changed: the first match ends it (any plan may be chosen when every receipt matches).
    let start = std::time::Instant::now();
    assert!(s.slice_changed_after(&sl, "2023-12-31T00:00:00.000000Z").unwrap());
    assert!(start.elapsed() < std::time::Duration::from_millis(500), "{:?}", start.elapsed());
    // A bounded slice of a function whose other cells all changed after A: its own few
    // candidates decide, not the function's recent receipts (100,000 of them here).
    let early = "2023-12-31T00:00:00.000000Z";
    let range = |lo: u32, hi: u32| {
        slice(&format!(
            r#"{{"args":{{"k":{{"hi":"{hi}","lo":"{lo}","type":"number"}}}},"fn":"f","fn_hash":"{h}","ns":"{SLICE_HOST}"}}"#
        ))
    };
    for (sl, want) in [(range(500, 509), true), (range(200_000, 200_010), false), (range(99_990, 100_100), true)] {
        let start = std::time::Instant::now();
        assert_eq!(s.slice_changed_after(&sl, early).unwrap(), want);
        assert!(start.elapsed() < std::time::Duration::from_millis(200), "{:?}", start.elapsed());
    }
}
