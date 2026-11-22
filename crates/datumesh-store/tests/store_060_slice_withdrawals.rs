//! Store 0.6.0, the 8.2 rule: `Store::slice_withdrawn_latest`. Both backends.
//!
//! A withdrawal that left a cell out of a slice counts for the reader's `tx` and `asof` (SPEC
//! 8.2, spec-0.7.0): the reader of a slice needs the latest withdrawal among the cells it would
//! have read. API assumed (ADDITIVE; a new `Store` method with a default body that is
//! correct, by the existing reads, or `Err(Error::Unsupported)`):
//!
//! ```text
//! // on Store:
//! fn slice_withdrawn_latest(&self, slice: &Slice, asof: &str) -> Result<Option<(String, String)>>;
//! //                                                                        (tx, asof) of the receipt
//! ```
//!
//! Rules:
//! - Among the cells that match `slice` whose latest version as of `asof` (the answering rule
//!   of `stored_asof`: latest tx <= asof whose receipt's asof is also <= asof) is a withdrawal,
//!   the greatest withdrawal, compared by `tx` and then by `asof`; the result is that
//!   withdrawal's `(tx, asof)`.
//! - `None` when no matching cell is withdrawn as of `asof`: none withdrawn at all, the
//!   withdrawal is not yet in effect as of `asof`, the withdrawn cell is outside the slice's
//!   range, or a later valued version ended the withdrawal.
//! - A malformed `asof` is `E_CANON_INSTANT`.
//! - One indexed query; nothing is asserted about plans. (Only one withdrawal per slice is
//!   available among the signed spec vectors, so "the greatest of several" is not tested here.)
//!
//! Worlds: the vectors `formula-eval/withdrawn-slice-count` (ten daily readings 2026-03-01 to
//! 2026-03-10, reading of 4 March withdrawn at 2026-03-20T18:00) and `formula-eval/withdrawn-ended`
//! (bolt's price withdrawn at 2026-05-01T09:00, a value again at 2026-05-20T09:00).

#[macro_use]
mod common;

use common::*;
use datumesh_core::json::Value;
use datumesh_core::{Cell, ErrorCode};

const W_TX: &str = "2026-03-20T18:00:00.000000Z";

fn arr(v: &Value) -> Option<&[Value]> {
    match v {
        Value::Array(a) => Some(a),
        _ => None,
    }
}

/// A store with the vector's world; returns it with the hash of the stored function `fn_name`.
fn world(b: Backend, vector_id: &str, fn_name: &str) -> (TestDb, TStore, String) {
    let db = TestDb::migrated(b);
    let s = db.bare_store();
    put_vector_keys(&s, &vector("receipt/slice-join"));
    let v = vector(vector_id);
    let w = v.get("world").unwrap();
    let mut fn_hash = String::new();
    for (name, f) in w.get("functions").and_then(|f| f.as_object()).unwrap() {
        let h = s.put_descriptor(&f.get("descriptor").unwrap().to_jcs()).unwrap();
        if name == fn_name {
            fn_hash = h;
        }
    }
    for r in arr(w.get("receipts").unwrap()).unwrap() {
        let cell = Cell::from_value(r.get("cell").unwrap()).unwrap();
        let h = s.put_receipt(&r.to_jcs()).unwrap();
        s.put_version(&cell.id(), str_of(r, "tx"), &h).unwrap();
    }
    (db, s, fn_hash)
}

fn daily(h: &str, date: &str) -> datumesh_core::Slice {
    slice(&format!(r#"{{"args":{{"date":{date}}},"fn":"daily","fn_hash":"{h}","ns":"data.example.org"}}"#))
}

fn range(lo: &str, hi: &str) -> String {
    format!(r#"{{"hi":"{hi}","lo":"{lo}","type":"date"}}"#)
}

fn the_withdrawal_in_effect_is_the_latest_withdrawn(b: Backend) {
    let (_db, s, h) = world(b, "formula-eval/withdrawn-slice-count", "data.example.org/daily");
    let sl = daily(&h, r#""*""#);
    for a in [W_TX, "2026-03-21T00:00:00.000000Z", "2026-06-01T00:00:00.000000Z", "9999-12-31T23:59:59.999999Z"] {
        assert_eq!(s.slice_withdrawn_latest(&sl, a).unwrap(), Some((W_TX.to_string(), W_TX.to_string())), "{a}");
    }
}

fn before_the_withdrawal_is_in_effect_there_is_none(b: Backend) {
    let (_db, s, h) = world(b, "formula-eval/withdrawn-slice-count", "data.example.org/daily");
    let sl = daily(&h, r#""*""#);
    for a in ["0001-01-01T00:00:00.000000Z", "2026-03-10T18:00:00.000000Z", "2026-03-20T17:59:59.999999Z"] {
        assert_eq!(s.slice_withdrawn_latest(&sl, a).unwrap(), None, "{a}");
    }
}

fn a_withdrawal_outside_the_slices_range_is_not_counted(b: Backend) {
    let (_db, s, h) = world(b, "formula-eval/withdrawn-slice-count", "data.example.org/daily");
    let a = "2026-06-01T00:00:00.000000Z";
    // 4 March is outside 5-10 March, and outside the one-day range of 5 March (a slice needs an open or ranged argument).
    assert_eq!(s.slice_withdrawn_latest(&daily(&h, &range("2026-03-05", "2026-03-10")), a).unwrap(), None);
    assert_eq!(s.slice_withdrawn_latest(&daily(&h, &range("2026-03-05", "2026-03-05")), a).unwrap(), None);
    // Inside the range, or the cell itself: counted.
    let want = Some((W_TX.to_string(), W_TX.to_string()));
    assert_eq!(s.slice_withdrawn_latest(&daily(&h, &range("2026-03-01", "2026-03-10")), a).unwrap(), want);
    assert_eq!(s.slice_withdrawn_latest(&daily(&h, &range("2026-03-04", "2026-03-04")), a).unwrap(), want);
    assert_eq!(s.slice_withdrawn_latest(&daily(&h, &range("2026-03-04", "2026-03-04")), a).unwrap(), want);
}

fn a_withdrawal_ended_by_a_later_value_is_not_counted(b: Backend) {
    let (_db, s, h) = world(b, "formula-eval/withdrawn-ended", "data.example.org/price");
    let sl = slice(&format!(r#"{{"args":{{"item":"*"}},"fn":"price","fn_hash":"{h}","ns":"data.example.org"}}"#));
    let tx = "2026-05-01T09:00:00.000000Z".to_string();
    assert_eq!(s.slice_withdrawn_latest(&sl, "2026-04-30T23:59:59.999999Z").unwrap(), None);
    for a in ["2026-05-01T09:00:00.000000Z", "2026-05-10T00:00:00.000000Z", "2026-05-20T08:59:59.999999Z"] {
        assert_eq!(s.slice_withdrawn_latest(&sl, a).unwrap(), Some((tx.clone(), tx.clone())), "{a}");
    }
    // Ended at 2026-05-20T09:00 by a value.
    for a in ["2026-05-20T09:00:00.000000Z", "2026-06-01T00:00:00.000000Z"] {
        assert_eq!(s.slice_withdrawn_latest(&sl, a).unwrap(), None, "{a}");
    }
}

fn an_empty_store_and_a_malformed_instant(b: Backend) {
    let db = TestDb::migrated(b);
    let s = db.bare_store();
    let sl = daily(&"ab".repeat(32), r#""*""#);
    assert_eq!(s.slice_withdrawn_latest(&sl, "2026-06-01T00:00:00.000000Z").unwrap(), None);
    let e = s.slice_withdrawn_latest(&sl, "2026-06-01").unwrap_err();
    assert_eq!(e.code(), Some(ErrorCode::CanonInstant), "{e:?}");
}

backends!(
    the_withdrawal_in_effect_is_the_latest_withdrawn,
    before_the_withdrawal_is_in_effect_there_is_none,
    a_withdrawal_outside_the_slices_range_is_not_counted,
    a_withdrawal_ended_by_a_later_value_is_not_counted,
    an_empty_store_and_a_malformed_instant,
);
