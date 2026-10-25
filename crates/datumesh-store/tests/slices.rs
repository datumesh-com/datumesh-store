//! Receipts whose `inputs` cite slices (0.3.1, spec-0.4.0 slice-as-lineage).
//!
//! * A receipt with slice entries is stored and read back byte for byte (its canonical form,
//!   as every receipt), whether or not its manifests are stored yet: the manifest is needed
//!   only to recompute (SPEC 10.1 step 7.5), so `put_receipt` does not require it.
//! * Lineage: each slice entry is recorded (`slice_entries`, `slice_dependents`); the
//!   receipt-hash edges are unchanged (`inputs` and `dependents` list receipt hashes only);
//!   `cell_dependents` also finds the cells that read a cell through a slice whose stored
//!   manifest lists one of its receipts.
//! * The specification's slice-lineage receipt vectors, loaded into the store (descriptors,
//!   members, manifests, receipts), verify through it: manifests and members come from the
//!   store, the evaluation (a formula, which the store cannot evaluate) from the vector.
//!   Valid vectors reach their level and code; invalid ones fail with their code, or the store
//!   refused the part that is wrong.

mod common;

use std::borrow::Cow;

use common::*;
use datumesh_core::json::Value;
use datumesh_core::{verify, Cell, ErrorCode, Evaluation, Level, Manifest, Receipt, Resolver, Scalar};
use datumesh_store::{PgStore, Store};

/// The store for descriptors, receipts and manifests; the vector's `recompute.value` (and
/// `lineage`) for the evaluation.
struct WithValue<'a> {
    store: &'a PgStore,
    value: Option<Scalar>,
    lineage: Option<Vec<String>>,
}

impl Resolver for WithValue<'_> {
    fn descriptor(&self, fn_hash: &str) -> Option<Cow<'_, [u8]>> {
        self.store.descriptor(fn_hash)
    }
    fn receipt(&self, hash: &str) -> Option<Cow<'_, [u8]>> {
        self.store.receipt(hash)
    }
    fn manifest(&self, entry: &str) -> Option<Cow<'_, [u8]>> {
        self.store.manifest(entry)
    }
    fn evaluate(&self, _: &Cell, _: &str) -> Option<Scalar> {
        self.value.clone()
    }
    fn evaluation(&self, _: &Cell, _: &str) -> Option<Evaluation> {
        let value = self.value.clone()?;
        Some(match &self.lineage {
            Some(l) => Evaluation::with_lineage(value, l.clone()),
            None => Evaluation::new(value),
        })
    }
}

fn with_value<'a>(store: &'a PgStore, v: &Value) -> WithValue<'a> {
    let rc = v.get("recompute");
    WithValue {
        store,
        value: rc.and_then(|r| r.get("value")).map(|x| Scalar::from_json(&x.to_jcs()).unwrap()),
        lineage: match rc.and_then(|r| r.get("lineage")) {
            Some(Value::Array(a)) => Some(a.iter().filter_map(Value::as_str).map(String::from).collect()),
            _ => None,
        },
    }
}

fn slice_vectors(group: &str) -> Vec<Value> {
    vectors(group).into_iter().filter(|v| str_of(v, "kind") == "receipt" && str_of(v, "id").contains("slice")).collect()
}

fn entries(r: &Receipt) -> Vec<String> {
    r.inputs().iter().filter(|e| e.starts_with("slice:")).cloned().collect()
}

fn hashes(r: &Receipt) -> Vec<String> {
    r.inputs().iter().filter(|e| !e.starts_with("slice:")).cloned().collect()
}

#[test]
fn slice_lineage_vectors_verify_through_the_store() {
    let valid = slice_vectors("receipt");
    assert!(valid.len() >= 13, "{}", valid.len());
    let mut recomputed = 0;
    for v in &valid {
        let id = str_of(v, "id");
        let db = TestDb::migrated();
        let s = db.bare_store();
        let failed = put_vector_world(&s, v);
        assert!(failed.is_empty(), "{id}: {failed:?}");
        let bytes = input(v);
        let r = Receipt::from_json(&bytes).unwrap();
        // Byte for byte: the canonical form, which parses back to the same receipt.
        let back = s.receipt(r.hash()).unwrap();
        assert_eq!(&*back, jcs(&bytes).as_slice(), "{id}");
        assert_eq!(Receipt::from_json(&back).unwrap(), r, "{id}");

        // Through the store when the vector recomputes; a signed-only vector without it.
        let resolver = with_value(&s, v);
        let out = verify(&bytes, &keys(v), v.get("recompute").map(|_| &resolver as &dyn Resolver), None);
        let level = match str_of(v, "level") {
            "recomputed" => Level::Recomputed,
            _ => Level::Signed,
        };
        assert!(out.ok, "{id}: {out:?}");
        assert_eq!(out.level, Some(level), "{id}: {out:?}");
        assert_eq!(out.code.map(ErrorCode::as_str), v.get("code").and_then(Value::as_str), "{id}: {out:?}");
        recomputed += usize::from(level == Level::Recomputed);
    }
    assert!(recomputed >= 10, "{recomputed}");
}

#[test]
fn invalid_slice_lineage_vectors_fail_through_the_store() {
    let invalid = slice_vectors("invalid");
    assert!(invalid.len() >= 17, "{}", invalid.len());
    for v in &invalid {
        let id = str_of(v, "id");
        let want = str_of(v, "error");
        let db = TestDb::migrated();
        let s = db.bare_store();
        let failed = put_vector_world(&s, v);
        let out = verify(&input(v), &keys(v), Some(&with_value(&s, v)), None);
        assert!(!out.ok, "{id}: {out:?}");
        if failed.is_empty() {
            // Everything was stored: the store's resolver gives the vector's code.
            assert_eq!(out.code.map(ErrorCode::as_str), Some(want), "{id}: {out:?}");
        } else {
            // The store refused what is wrong: the receipt itself with the vector's code, or
            // a manifest or member, which is then unavailable to verify.
            let refused_with_code = failed.iter().any(|(_, e)| e.code().map(ErrorCode::as_str) == Some(want));
            assert!(
                refused_with_code || out.code == Some(ErrorCode::InputUnavailable),
                "{id}: refused {failed:?}, verify {out:?}"
            );
        }
    }
}

#[test]
fn slice_entries_are_recorded_and_receipt_edges_unchanged() {
    let db = TestDb::migrated();
    let s = db.bare_store();
    // slice-mixed-inputs reads single cells and a slice.
    let v = vector("receipt/slice-mixed-inputs");
    let r = Receipt::from_json(&input(&v)).unwrap();
    assert!(!entries(&r).is_empty() && !hashes(&r).is_empty(), "{:?}", r.inputs());
    // The receipt first, before its manifests and inputs: not required.
    put_vector_keys(&s, &v);
    let h = s.put_receipt(&input(&v)).unwrap();
    assert_eq!(h, r.hash());
    assert_eq!(s.slice_entries(&h).unwrap(), entries(&r));
    for e in entries(&r) {
        assert_eq!(s.slice_dependents(&e).unwrap(), std::slice::from_ref(&h));
        assert_eq!(s.manifest(&e), None);
    }
    // The receipt-hash edges are as before: receipt hashes only.
    assert_eq!(s.inputs(&h).unwrap(), hashes(&r));
    for i in hashes(&r) {
        assert_eq!(s.dependents(&i).unwrap(), std::slice::from_ref(&h));
    }
    // Then everything else; again idempotent.
    assert!(put_vector_world(&s, &v).is_empty());
    assert_eq!(s.slice_entries(&h).unwrap(), entries(&r));
    assert_eq!(count(&db, "slice_edges"), entries(&r).len() as i64);

    // A receipt without slice entries has none; unknown and malformed arguments.
    let stored = vector("receipt/stored-number");
    put_vector_keys(&s, &stored);
    let hs = s.put_receipt(&input(&stored)).unwrap();
    assert!(s.slice_entries(&hs).unwrap().is_empty());
    assert!(s.slice_entries(&"0".repeat(64)).unwrap().is_empty());
    let unknown = format!("slice:{}:{}", "0".repeat(64), "1".repeat(64));
    assert!(s.slice_dependents(&unknown).unwrap().is_empty());
    for bad in ["", "abc", &h.to_uppercase(), &h, &unknown.to_uppercase(), &format!("{unknown}0"), &unknown[..134]] {
        assert_eq!(s.slice_dependents(bad).unwrap_err().code(), Some(ErrorCode::CanonHex), "{bad:?}");
    }
    for bad in ["", "abc", &h.to_uppercase(), unknown.as_str()] {
        assert_eq!(s.slice_entries(bad).unwrap_err().code(), Some(ErrorCode::CanonHex), "{bad:?}");
    }
}

#[test]
fn members_find_their_manifests_and_the_cells_reading_them() {
    let db = TestDb::migrated();
    let s = db.bare_store();
    let v = vector("receipt/slice-weighted-mean");
    let r = Receipt::from_json(&input(&v)).unwrap();
    let rc = v.get("recompute").unwrap();
    let manifests: Vec<Manifest> = rc
        .get("manifests")
        .and_then(Value::as_object)
        .unwrap()
        .iter()
        .map(|(_, m)| Manifest::from_json(m.as_str().unwrap().as_bytes()).unwrap())
        .collect();
    assert_eq!(manifests.len(), 2);

    // Members and the reading receipt, but no manifest yet: no reader is known from a member.
    put_vector_keys(&s, &v);
    for (_, m) in rc.get("inputs").and_then(Value::as_object).unwrap() {
        s.put_receipt(m.as_str().unwrap().as_bytes()).unwrap();
    }
    s.put_receipt(&input(&v)).unwrap();
    let member = &manifests[0].members()[0];
    let member_cell = Receipt::from_json(&s.receipt(member).unwrap()).unwrap().cell().id();
    assert!(s.member_manifests(member).unwrap().is_empty());
    assert!(s.cell_dependents(&member_cell).unwrap().is_empty());

    // With the manifests: each member names its manifests, and the cell reading the slice is
    // a dependent of every member's cell.
    for m in &manifests {
        assert_eq!(s.put_manifest(&m.canonical()).unwrap(), m.entry());
    }
    for m in &manifests {
        for h in m.members() {
            let found = s.member_manifests(h).unwrap();
            assert!(found.contains(&m.entry()), "{h}: {found:?}");
            let mut sorted = found.clone();
            sorted.sort();
            assert_eq!(found, sorted);
            let cell = Receipt::from_json(&s.receipt(h).unwrap()).unwrap().cell().id();
            assert_eq!(s.cell_dependents(&cell).unwrap(), [r.cell().id()], "{h}");
        }
        assert_eq!(s.slice_dependents(&m.entry()).unwrap(), [r.hash().to_string()]);
    }
    // A receipt that is in no manifest; malformed arguments.
    assert!(s.member_manifests(r.hash()).unwrap().is_empty());
    for bad in ["", "abc", &member.to_uppercase(), &manifests[0].entry()] {
        assert_eq!(s.member_manifests(bad).unwrap_err().code(), Some(ErrorCode::CanonHex), "{bad:?}");
    }
}

#[test]
fn put_receipt_of_a_slice_receipt_is_idempotent() {
    let db = TestDb::migrated();
    let s = db.bare_store();
    let v = vector("receipt/slice-join");
    assert!(put_vector_world(&s, &v).is_empty());
    let (receipts, edges) = (count(&db, "receipts"), count(&db, "slice_edges"));
    assert!(put_vector_world(&s, &v).is_empty());
    assert_eq!((count(&db, "receipts"), count(&db, "slice_edges")), (receipts, edges));
    // Its canonical form put again returns the same hash, and the bytes are unchanged.
    let h = Receipt::from_json(&input(&v)).unwrap().hash().to_string();
    let back = s.receipt(&h).unwrap().into_owned();
    assert_eq!(s.put_receipt(&back).unwrap(), h);
    assert_eq!(&*s.receipt(&h).unwrap(), back.as_slice());
}

#[test]
fn the_slice_edges_are_insert_only() {
    let db = TestDb::migrated();
    let s = db.bare_store();
    assert!(put_vector_world(&s, &vector("receipt/slice-range")).is_empty());
    let mut c = db.client();
    for sql in ["UPDATE slice_edges SET entry = entry", "DELETE FROM slice_edges", "TRUNCATE slice_edges"] {
        let e = c.batch_execute(sql).expect_err(sql);
        let msg = e.as_db_error().map(|d| d.message().to_string()).unwrap_or_default();
        assert!(msg.contains("append-only"), "{sql}: {msg}");
    }
}
