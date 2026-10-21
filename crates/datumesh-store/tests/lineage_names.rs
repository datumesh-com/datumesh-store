//! Lineage and names.
//!
//! Lineage: `receipt/computed-with-lineage` and its input from the vectors; `inputs` and
//! `dependents` follow the edges `put_receipt` records, also to receipts the store does not
//! have; `verify` through the store reaches `recomputed` for the stored input and stops at
//! `E_EVAL_UNAVAILABLE` (signed) for the computed receipt, whose function the store cannot
//! evaluate.
//!
//! Names: bind A at tx1 and B at tx2; `resolve_name` per time; receipts of both functions
//! still verify through the store.

mod common;

use common::*;
use datumesh_core::{verify, Cell, Descriptor, ErrorCode, Level, Receipt, ReceiptBuilder, Resolver, Scalar, Type};
use datumesh_store::{Error, Store};

#[test]
fn inputs_and_dependents_follow_the_edges() {
    let db = TestDb::migrated();
    let s = db.store();
    let lineage = vector("receipt/computed-with-lineage");
    let stored = vector("receipt/stored-number");
    let rc = lineage.get("recompute").unwrap();
    s.put_descriptor(&rc.get("descriptor").unwrap().to_jcs()).unwrap();
    s.put_descriptor(&fx_rate_descriptor()).unwrap();
    let h_in = s.put_receipt(&input(&stored)).unwrap();
    let h = s.put_receipt(&input(&lineage)).unwrap();
    let r_in = Receipt::from_json(&input(&stored)).unwrap();
    s.put_version(&r_in.cell().id(), r_in.tx(), &h_in).unwrap();

    assert_eq!(s.inputs(&h), std::slice::from_ref(&h_in));
    assert_eq!(s.dependents(&h_in), std::slice::from_ref(&h));
    assert!(s.inputs(&h_in).is_empty());
    assert!(s.dependents(&h).is_empty());

    // Two inputs, one of which the store does not hold: both edges are recorded.
    let two = vector("receipt/computed-two-inputs");
    let h2 = s.put_receipt(&input(&two)).unwrap();
    let want: Vec<String> = Receipt::from_json(&input(&two)).unwrap().inputs().to_vec();
    assert_eq!(s.inputs(&h2), want);
    let mut deps = s.dependents(&h_in);
    deps.sort();
    let mut expect = vec![h.clone(), h2.clone()];
    expect.sort();
    assert_eq!(deps, expect);
    assert_eq!(s.dependents(&want[1]), std::slice::from_ref(&h2));

    // verify through the store.
    let keys = keys(&lineage);
    let out = verify(&input(&stored), &keys, Some(&s as &dyn Resolver), None);
    assert_eq!((out.ok, out.level), (true, Some(Level::Recomputed)), "{out:?}");
    let out = verify(&input(&lineage), &keys, Some(&s as &dyn Resolver), None);
    assert!(out.ok, "{out:?}");
    assert_eq!(out.level, Some(Level::Signed));
    assert_eq!(out.code, Some(ErrorCode::EvalUnavailable));
}

/// A second stored fx_rate function (another body), its hash, and a receipt of the EUR/USD
/// cell under it at `tx`.
fn function_b(tx: &str) -> (Vec<u8>, String, Vec<u8>) {
    let a = String::from_utf8(fx_rate_descriptor()).unwrap();
    let b = a.replace(r#""body":"""#, r#""body":"rates v2""#).into_bytes();
    assert_ne!(a.as_bytes(), b.as_slice());
    let d = Descriptor::from_json(&b).unwrap();
    assert_eq!(d.lang, "stored");
    let hash = d.hash();
    let v = vector("receipt/stored-number");
    let base = Receipt::from_json(&input(&v)).unwrap();
    let mut cell_json = String::from_utf8(base.cell().canonical()).unwrap();
    cell_json = cell_json.replace(&base.cell().fn_hash, &hash);
    let cell = Cell::from_json(cell_json.as_bytes()).unwrap();
    let r = ReceiptBuilder::new()
        .cell(cell)
        .value(Scalar::from_lexical(Type::Number, "1.0893").unwrap())
        .valid(base.valid().clone())
        .asof(tx)
        .tx(tx)
        .issued(tx)
        .publisher(base.publisher())
        .signer_host(base.signer_host())
        .licence(base.licence())
        .summary("1 EUR was worth 1.0893 USD at the 15 March 2024 reference rate.")
        .sign(&signing_key(&v))
        .unwrap();
    (b, hash, receipt_json(&r))
}

#[test]
fn names_resolve_per_time_and_both_functions_verify() {
    let db = TestDb::migrated();
    let s = db.store();
    let tx1 = "2024-03-15T16:00:00.000000Z";
    let tx2 = "2024-03-18T10:00:00.000000Z";
    let name = "rates.example.org/fx_rate";
    let a = s.put_descriptor(&fx_rate_descriptor()).unwrap();
    let (b_bytes, b, rb) = function_b(tx2);
    assert_eq!(s.put_descriptor(&b_bytes).unwrap(), b);
    assert_ne!(a, b);

    assert_eq!(s.resolve_name(name, tx2), None);
    s.bind_name(name, &a, tx1).unwrap();
    s.bind_name(name, &b, tx2).unwrap();
    assert_eq!(s.resolve_name(name, "2024-03-15T15:59:59.999999Z"), None);
    assert_eq!(s.resolve_name(name, tx1).as_deref(), Some(a.as_str()));
    assert_eq!(s.resolve_name(name, "2024-03-17T00:00:00.000000Z").as_deref(), Some(a.as_str()));
    assert_eq!(s.resolve_name(name, tx2).as_deref(), Some(b.as_str()));
    assert_eq!(s.resolve_name(name, "9999-12-31T23:59:59.999999Z").as_deref(), Some(b.as_str()));
    assert_eq!(s.resolve_name("rates.example.org/other", tx2), None);

    // Receipts of both functions verify, recomputed, through the store.
    let v = vector("receipt/stored-number");
    let ra = input(&v);
    for bytes in [ra, rb] {
        let h = s.put_receipt(&bytes).unwrap();
        let r = Receipt::from_json(&bytes).unwrap();
        s.put_version(&r.cell().id(), r.tx(), &h).unwrap();
        let out = verify(&bytes, &keys(&v), Some(&s as &dyn Resolver), None);
        assert_eq!((out.ok, out.level), (true, Some(Level::Recomputed)), "{out:?}");
    }
}

#[test]
fn bind_name_validates_its_arguments() {
    let db = TestDb::migrated();
    let s = db.store();
    let a = s.put_descriptor(&fx_rate_descriptor()).unwrap();
    let tx = "2024-03-15T16:00:00.000000Z";
    // Not a fully qualified function name.
    for bad in ["fx_rate", "rates.example.org/", "Rates.example.org/fx_rate", "rates.example.org/fx-rate", ""] {
        let e = s.bind_name(bad, &a, tx).unwrap_err();
        assert_eq!(e.code(), Some(ErrorCode::CanonName), "{bad}: {e}");
    }
    // Not an instant.
    let e = s.bind_name("rates.example.org/fx_rate", &a, "2024-03-15T16:00:00Z").unwrap_err();
    assert_eq!(e.code(), Some(ErrorCode::CanonInstant));
    // Not a hash, and a hash the store has no descriptor for.
    let e = s.bind_name("rates.example.org/fx_rate", &a.to_uppercase(), tx).unwrap_err();
    assert_eq!(e.code(), Some(ErrorCode::CanonHex));
    assert!(matches!(s.bind_name("rates.example.org/fx_rate", &"0".repeat(64), tx), Err(Error::NotFound { .. })));
    // Reads with malformed arguments are None, not errors.
    assert_eq!(s.resolve_name("", tx), None);
    assert_eq!(s.resolve_name("rates.example.org/fx_rate", "\0"), None);
}
