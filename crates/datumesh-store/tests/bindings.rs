//! `resolve_binding`: the name binding that answers as of A, with when it began.
//!
//! Before the first binding nothing; exactly at a binding's tx that binding; between two
//! bindings the earlier; binding a name again to a hash it meant before gives the later
//! start; malformed arguments are the core's codes, without a query. `resolve_name` agrees
//! with it everywhere. `Store` stays object-safe: all of it through `&dyn Store`.

mod common;

use common::*;
use datumesh_core::{Descriptor, ErrorCode};
use datumesh_store::{Binding, Store};

const NAME: &str = "rates.example.org/fx_rate";
const T1: &str = "2024-03-15T16:00:00.000000Z";
const T2: &str = "2024-03-18T10:00:00.000000Z";
const T3: &str = "2024-03-20T08:30:00.000000Z";
const T4: &str = "2024-03-22T00:00:00.000000Z";

/// A second stored function (another body) next to the vectors' fx_rate.
fn descriptor_b() -> Vec<u8> {
    let a = String::from_utf8(fx_rate_descriptor()).unwrap();
    let b = a.replace(r#""body":"""#, r#""body":"rates v2""#);
    assert_ne!(a, b);
    Descriptor::from_json(b.as_bytes()).unwrap().canonical()
}

/// (fn_hash, tx) of a binding, for comparing.
fn pair(b: Option<Binding>) -> Option<(String, String)> {
    b.map(|b| (b.fn_hash, b.tx))
}

#[test]
fn before_at_between_and_after_bindings() {
    let db = TestDb::migrated();
    // As the engine holds it: shared, behind a trait object.
    let pg: std::sync::Arc<dyn Store + Send + Sync> = std::sync::Arc::new(db.store());
    let s: &dyn Store = &*pg;
    let a = s.put_descriptor(&fx_rate_descriptor()).unwrap();
    let b = s.put_descriptor(&descriptor_b()).unwrap();
    assert_ne!(a, b);
    let at = |asof: &str| pair(s.resolve_binding(NAME, asof).unwrap());
    let want = |h: &str, tx: &str| Some((h.to_string(), tx.to_string()));

    // No binding at all yet.
    assert_eq!(at(T4), None);
    // a from T1, b from T2, a again from T3 (a hash the name meant before), a again from T4
    // (the same hash as the binding before it).
    s.bind_name(NAME, &a, T1).unwrap();
    s.bind_name(NAME, &b, T2).unwrap();
    s.bind_name(NAME, &a, T3).unwrap();
    s.bind_name(NAME, &a, T4).unwrap();

    assert_eq!(at("0001-01-01T00:00:00.000000Z"), None);
    assert_eq!(at("2024-03-15T15:59:59.999999Z"), None);
    assert_eq!(at(T1), want(&a, T1));
    assert_eq!(at("2024-03-17T00:00:00.000000Z"), want(&a, T1));
    assert_eq!(at("2024-03-18T09:59:59.999999Z"), want(&a, T1));
    assert_eq!(at(T2), want(&b, T2));
    assert_eq!(at("2024-03-20T08:29:59.999999Z"), want(&b, T2));
    // Bound to a again: the binding answering began at T3, not at T1.
    assert_eq!(at(T3), want(&a, T3));
    assert_eq!(at("2024-03-21T00:00:00.000000Z"), want(&a, T3));
    // And again to the same hash: the later start.
    assert_eq!(at(T4), want(&a, T4));
    assert_eq!(at("9999-12-31T23:59:59.999999Z"), want(&a, T4));

    // resolve_name is the binding's hash, everywhere.
    for asof in ["2024-03-15T15:59:59.999999Z", T1, "2024-03-17T00:00:00.000000Z", T2, T3, T4] {
        assert_eq!(s.resolve_name(NAME, asof).unwrap(), at(asof).map(|(h, _)| h), "{asof}");
    }
    // Another name has no binding.
    assert_eq!(s.resolve_binding("rates.example.org/other", T4).unwrap(), None);
}

#[test]
fn a_binding_is_a_value() {
    let db = TestDb::migrated();
    let s = db.store();
    let a = s.put_descriptor(&fx_rate_descriptor()).unwrap();
    s.bind_name(NAME, &a, T1).unwrap();
    let got = s.resolve_binding(NAME, T2).unwrap().unwrap();
    assert_eq!((got.fn_hash.as_str(), got.tx.as_str()), (a.as_str(), T1));
    // Clone, Eq, Hash, Debug.
    let same = s.resolve_binding(NAME, T1).unwrap().unwrap();
    assert_eq!(got.clone(), same);
    let set: std::collections::HashSet<Binding> = [got.clone(), same].into_iter().collect();
    assert_eq!(set.len(), 1);
    assert!(format!("{got:?}").contains(T1));
}

#[test]
fn malformed_arguments_are_the_cores_codes_without_a_query() {
    // An empty database: a query would fail with Error::Database.
    let db = TestDb::empty();
    let s = db.bare_store();
    let code = |r: datumesh_store::Result<Option<Binding>>| r.unwrap_err().code();
    for bad in ["", "fx_rate", "rates.example.org/", "Rates.example.org/fx_rate", "rates.example.org/fx-rate"] {
        assert_eq!(code(s.resolve_binding(bad, T1)), Some(ErrorCode::CanonName), "{bad}");
    }
    for bad in ["", "\0", "2024-03-15T16:00:00Z", "2024-03-15T16:00:00.000000+00:00", "yesterday"] {
        assert_eq!(code(s.resolve_binding(NAME, bad)), Some(ErrorCode::CanonInstant), "{bad}");
    }
    // Well-formed arguments do reach the (empty) database.
    assert!(matches!(s.resolve_binding(NAME, T1), Err(datumesh_store::Error::Database(_))));
}
