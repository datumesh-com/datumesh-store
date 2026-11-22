//! Store 0.6.0, R1 at verify time: the members a slice lists as of an instant leave a withdrawn
//! cell out (SPEC 8.1 `spec-0.7.0`: a withdrawal is no member of a slice). Both backends.
//!
//! The formula engine builds the slice entry it reads (`slice:<id>:<members hash>`) from the
//! store's `Store::slice_members(slice, asof)`; when that listed the withdrawn cell, the
//! engine's computed receipt failed `verify` with `E_INPUT_SUPERSEDED` ("the evaluation read
//! slice a45784ce... with other members"). The world is that of the vector
//! `formula-eval/withdrawn-slice-count`: ten daily readings (2026-03-01 to 2026-03-10, the
//! slice of `data.example.org/daily` with `date: *`) and, at 2026-03-20, a withdrawal of the
//! reading of 4 March (a valued receipt of that cell is stored at 2026-03-04).
//!
//! API: no new signature. `Store::slice_members` (and `slice_values`) as of an instant at or
//! after the withdrawal's asof lists nine members, not the withdrawn cell, and the manifest of
//! their receipt hashes is the vector's lineage entry
//! `slice:a45784ce...:e189c4...`; as of before the withdrawal, all ten.

#[macro_use]
mod common;

use common::*;
use datumesh_core::{Cell, Manifest, Receipt};

const BEFORE: &str = "2026-03-19T00:00:00.000000Z";
const AT: &str = "2026-03-20T18:00:00.000000Z";
const AFTER: &str = "2026-06-01T00:00:00.000000Z";

fn arr(v: &datumesh_core::json::Value) -> Option<&[datumesh_core::json::Value]> {
    match v {
        datumesh_core::json::Value::Array(a) => Some(a),
        _ => None,
    }
}

struct World {
    s: TStore,
    _db: TestDb,
    slice: datumesh_core::Slice,
    lineage: String,
    withdrawn_cell: String,
}

fn world(b: Backend) -> World {
    let db = TestDb::migrated(b);
    let s = db.bare_store();
    put_vector_keys(&s, &vector("receipt/slice-join"));
    let v = vector("formula-eval/withdrawn-slice-count");
    let w = v.get("world").unwrap();
    let mut fn_hash = String::new();
    for (name, f) in w.get("functions").and_then(|f| f.as_object()).unwrap() {
        let h = s.put_descriptor(&f.get("descriptor").unwrap().to_jcs()).unwrap();
        if name == "data.example.org/daily" {
            fn_hash = h;
        }
    }
    let mut withdrawn_cell = String::new();
    for r in w.get("receipts").and_then(|r| arr(r)).unwrap() {
        let bytes = r.to_jcs();
        let cell = Cell::from_value(r.get("cell").unwrap()).unwrap();
        let h = s.put_receipt(&bytes).unwrap();
        s.put_version(&cell.id(), str_of(r, "tx"), &h).unwrap();
        if r.get("withdrawn").is_some() {
            withdrawn_cell = cell.id();
        }
    }
    let slice =
        slice(&format!(r#"{{"args":{{"date":"*"}},"fn":"daily","fn_hash":"{fn_hash}","ns":"data.example.org"}}"#));
    let lineage = v.get("lineage").and_then(|l| arr(l)).unwrap()[0].as_str().unwrap().to_string();
    assert!(lineage.starts_with(&format!("slice:{}:", slice.id())), "the slice is the vector's");
    World { s, _db: db, slice, lineage, withdrawn_cell }
}

fn slice_members_as_of_the_withdrawal_leave_the_withdrawn_cell_out(b: Backend) {
    let x = world(b);
    for a in [AT, AFTER] {
        let ms = x.s.slice_members(&x.slice, a).unwrap();
        assert_eq!(ms.len(), 9, "{a}");
        assert!(ms.iter().all(|m| m.version.cell_id != x.withdrawn_cell), "{a}");
        // Every member is a valued receipt.
        assert!(ms.iter().all(|m| Receipt::from_json(&m.receipt).is_ok()), "{a}");
        // The manifest of the members is the entry the engine's receipt reads.
        let mut hashes: Vec<String> = ms.iter().map(|m| m.version.receipt_hash.clone()).collect();
        hashes.sort();
        let m = Manifest::new(x.slice.clone(), hashes).unwrap();
        assert_eq!(m.entry(), x.lineage, "{a}");
        assert_eq!(x.s.slice_values(&x.slice, a).unwrap().len(), 9, "{a}");
        assert_eq!(x.s.slice_values_limit(&x.slice, a, 100).unwrap().len(), 9, "{a}");
    }
}

fn slice_members_before_the_withdrawal_list_the_cell(b: Backend) {
    let x = world(b);
    for a in ["2026-03-10T18:00:00.000000Z", BEFORE, "2026-03-20T17:59:59.999999Z"] {
        let ms = x.s.slice_members(&x.slice, a).unwrap();
        assert_eq!(ms.len(), 10, "{a}");
        let m = ms.iter().find(|m| m.version.cell_id == x.withdrawn_cell).expect("the cell is a member before");
        assert_eq!(Receipt::from_json(&m.receipt).unwrap().value().lexical(), "15", "{a}");
        assert_eq!(x.s.slice_values(&x.slice, a).unwrap().len(), 10, "{a}");
    }
    // The manifest of the ten differs from the nine's.
    let mut hashes: Vec<String> =
        x.s.slice_members(&x.slice, BEFORE).unwrap().iter().map(|m| m.version.receipt_hash.clone()).collect();
    hashes.sort();
    assert_ne!(Manifest::new(x.slice.clone(), hashes).unwrap().entry(), x.lineage);
}

backends!(
    slice_members_as_of_the_withdrawal_leave_the_withdrawn_cell_out,
    slice_members_before_the_withdrawal_list_the_cell,
);
