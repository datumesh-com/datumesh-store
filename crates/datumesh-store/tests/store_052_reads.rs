//! Store 0.5.2, S4-S6 (CONTRACT-0.4.9): `stored_receipts_of`, the bounded `slice_values_limit`
//! and `versions_limit`, and `stored_cell_count`. Both backends.

#[macro_use]
mod common;

use common::*;
use datumesh_core::Type;

fn day(d: u32) -> String {
    format!("2024-01-{d:02}T00:00:00.000000Z")
}

const END: &str = "9999-12-31T23:59:59.999999Z";

/// Stores the receipt of `cell` (value `v`, at day 1) and returns its bytes.
fn put_bytes(s: &TStore, cell: &datumesh_core::Cell, v: &str) -> Vec<u8> {
    let bytes = slice_receipt(cell, v, &day(1), &day(1));
    let h = s.put_receipt(&bytes).unwrap();
    s.put_version(&cell.id(), &day(1), &h).unwrap();
    bytes
}

fn kcell(name: &str, h: &str, k: u32) -> datumesh_core::Cell {
    slice_cell(name, h, &[("k", sc(Type::Number, &k.to_string()))])
}

fn stored_receipts_of_are_of_one_function_lowest_cell_id_first(b: Backend) {
    let db = TestDb::migrated(b);
    let s = slice_store(&db);
    let h = s.put_descriptor(&stored_descriptor(&[("k", Type::Number)], "f")).unwrap();
    let h2 = s.put_descriptor(&stored_descriptor(&[("k", Type::Number)], "f2")).unwrap();
    // Five cells of f/h, two of g/h (the same hash under another name), two of f/h2.
    let mut mine: Vec<(String, Vec<u8>)> = Vec::new();
    for k in 1..=5 {
        let c = kcell("f", &h, k);
        mine.push((c.id(), put_bytes(&s, &c, &format!("{}", k * 10))));
    }
    for k in 1..=2 {
        put_bytes(&s, &kcell("g", &h, k), "7");
        put_bytes(&s, &kcell("f", &h2, k), "8");
    }
    mine.sort_by(|a, b| a.0.cmp(&b.0));
    let want: Vec<Vec<u8>> = mine.into_iter().map(|(_, r)| r).collect();
    // Lowest cell id first; a limit is a prefix; more than there are gives all.
    assert_eq!(s.stored_receipts_of(SLICE_HOST, "f", &h, 100).unwrap(), want);
    assert_eq!(s.stored_receipts_of(SLICE_HOST, "f", &h, 5).unwrap(), want);
    assert_eq!(s.stored_receipts_of(SLICE_HOST, "f", &h, 3).unwrap(), want[..3]);
    assert_eq!(s.stored_receipts_of(SLICE_HOST, "f", &h, 1).unwrap(), want[..1]);
    assert!(s.stored_receipts_of(SLICE_HOST, "f", &h, 0).unwrap().is_empty());
    // The same hash under another name: its own receipts only.
    assert_eq!(s.stored_receipts_of(SLICE_HOST, "g", &h, 100).unwrap().len(), 2);
    // The same name under another hash.
    assert_eq!(s.stored_receipts_of(SLICE_HOST, "f", &h2, 100).unwrap().len(), 2);
    // Other namespace, unknown name or hash: nothing, not an error.
    assert!(s.stored_receipts_of("other.example.org", "f", &h, 100).unwrap().is_empty());
    assert!(s.stored_receipts_of(SLICE_HOST, "nope", &h, 100).unwrap().is_empty());
    assert!(s.stored_receipts_of(SLICE_HOST, "f", &"ee".repeat(32), 100).unwrap().is_empty());
}

fn a_cell_with_several_versions_gives_its_receipts_within_the_limit(b: Backend) {
    let db = TestDb::migrated(b);
    let s = slice_store(&db);
    let h = s.put_descriptor(&stored_descriptor(&[("k", Type::Number)], "f")).unwrap();
    for k in 1..=4 {
        put_bytes(&s, &kcell("f", &h, k), "1");
    }
    // Never more than `limit` receipts, whatever the versions.
    let c = kcell("f", &h, 1);
    for d in 2..=4 {
        put_member(&s, &c, &format!("{d}"), &day(d), &day(d));
    }
    for limit in 1..=3 {
        assert!(s.stored_receipts_of(SLICE_HOST, "f", &h, limit).unwrap().len() <= limit, "limit {limit}");
    }
    assert!(!s.stored_receipts_of(SLICE_HOST, "f", &h, 1).unwrap().is_empty());
}

fn slice_values_limit_is_the_prefix_of_slice_values(b: Backend) {
    let db = TestDb::migrated(b);
    let s = slice_store(&db);
    let h = s.put_descriptor(&stored_descriptor(&[("k", Type::Number)], "f")).unwrap();
    // Keys inserted in reverse so that key order is not insertion order.
    for k in (1..=6).rev() {
        put_bytes(&s, &kcell("f", &h, k), &format!("{}", k * 100));
    }
    let sl = slice(&format!(r#"{{"args":{{"k":"*"}},"fn":"f","fn_hash":"{h}","ns":"{SLICE_HOST}"}}"#));
    let all = s.slice_values(&sl, &day(2)).unwrap();
    assert_eq!(all.len(), 6);
    for limit in [0, 1, 2, 5, 6, 7, 100] {
        let got = s.slice_values_limit(&sl, &day(2), limit).unwrap();
        assert_eq!(got, all[..limit.min(6)], "limit {limit}");
    }
    // Before any version: empty. Malformed asof: refused as slice_values does.
    assert!(s.slice_values_limit(&sl, "2023-01-01T00:00:00.000000Z", 3).unwrap().is_empty());
    assert_eq!(
        s.slice_values_limit(&sl, "x", 3).unwrap_err().code(),
        s.slice_values(&sl, "x").unwrap_err().code()
    );
    // A version slice too.
    let c = kcell("f", &h, 1);
    for d in 2..=4 {
        put_member(&s, &c, &format!("{d}"), &day(d), &day(d));
    }
    let vs = slice(&format!(
        r#"{{"cell":{},"from":"{}","to":"{}"}}"#,
        String::from_utf8(c.canonical()).unwrap(),
        day(1),
        day(9)
    ));
    let all = s.slice_values(&vs, END).unwrap();
    assert!(all.len() >= 3);
    for limit in 1..=all.len() + 1 {
        assert_eq!(s.slice_values_limit(&vs, END, limit).unwrap(), all[..limit.min(all.len())]);
    }
}

fn versions_limit_is_the_prefix_of_versions(b: Backend) {
    let db = TestDb::migrated(b);
    let s = slice_store(&db);
    let h = s.put_descriptor(&stored_descriptor(&[("k", Type::Number)], "f")).unwrap();
    let c = kcell("f", &h, 1);
    for d in [3, 1, 4, 2, 5] {
        put_member(&s, &c, &format!("{d}"), &day(d), &day(d));
    }
    let id = c.id();
    let all = s.versions(&id, &day(1), &day(9)).unwrap();
    assert_eq!(all.len(), 5);
    for limit in [0, 1, 2, 4, 5, 6, 100] {
        assert_eq!(s.versions_limit(&id, &day(1), &day(9), limit).unwrap(), all[..limit.min(5)], "limit {limit}");
    }
    // A narrower range keeps the same order.
    let part = s.versions(&id, &day(2), &day(4)).unwrap();
    assert_eq!(s.versions_limit(&id, &day(2), &day(4), 2).unwrap(), part[..2.min(part.len())]);
    // An unknown cell: empty.
    assert!(s.versions_limit(&"00".repeat(32), &day(1), &day(9), 3).unwrap().is_empty());
}

fn stored_cell_count_counts_by_name_when_two_names_share_a_hash(b: Backend) {
    let db = TestDb::migrated(b);
    let s = slice_store(&db);
    let h = s.put_descriptor(&stored_descriptor(&[("k", Type::Number)], "f")).unwrap();
    let h2 = s.put_descriptor(&stored_descriptor(&[("k", Type::Number)], "f2")).unwrap();
    assert_eq!(s.stored_cell_count(SLICE_HOST, "f", &h).unwrap(), 0);
    for k in 1..=3 {
        put_bytes(&s, &kcell("f", &h, k), "1");
    }
    for k in 1..=2 {
        put_bytes(&s, &kcell("g", &h, k), "1");
    }
    put_bytes(&s, &kcell("f", &h2, 1), "1");
    // A second version of a cell is not a second cell.
    put_member(&s, &kcell("f", &h, 1), "2", &day(2), &day(2));
    assert_eq!(s.stored_cell_count(SLICE_HOST, "f", &h).unwrap(), 3);
    assert_eq!(s.stored_cell_count(SLICE_HOST, "g", &h).unwrap(), 2);
    assert_eq!(s.stored_cell_count(SLICE_HOST, "f", &h2).unwrap(), 1);
    assert_eq!(s.stored_cell_count(SLICE_HOST, "g", &h2).unwrap(), 0);
    assert_eq!(s.stored_cell_count("other.example.org", "f", &h).unwrap(), 0);
    assert_eq!(s.stored_cell_count(SLICE_HOST, "f", &"ee".repeat(32)).unwrap(), 0);
}

backends!(
    stored_receipts_of_are_of_one_function_lowest_cell_id_first,
    a_cell_with_several_versions_gives_its_receipts_within_the_limit,
    slice_values_limit_is_the_prefix_of_slice_values,
    versions_limit_is_the_prefix_of_versions,
    stored_cell_count_counts_by_name_when_two_names_share_a_hash,
);
