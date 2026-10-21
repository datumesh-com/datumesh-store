//! Signatures are verified on the way in: `put_receipt` runs SPEC 10.1 steps 1-6 against the
//! key document stored for the receipt's signer host, and refuses anything that does not
//! verify with `verify`'s code. Nothing unverified enters the store.

mod common;

use common::*;
use datumesh_core::{ErrorCode, Receipt, Resolver};
use datumesh_store::{Error, Store};

fn count(db: &TestDb) -> i64 {
    db.client().query_one("SELECT count(*) FROM receipts", &[]).unwrap().get(0)
}

fn refused(r: datumesh_store::Result<String>, code: ErrorCode) {
    match r {
        Err(Error::Invalid(e)) => assert_eq!(e.code(), code, "{e}"),
        x => panic!("want {code:?}, got {x:?}"),
    }
}

#[test]
fn a_forged_copy_is_refused_and_the_real_one_then_accepted() {
    let db = TestDb::migrated();
    let s = db.store();
    let good = jcs(&input(&vector("receipt/stored-number")));
    let r = Receipt::from_json(&good).unwrap();
    // Same payload (same hash), another signature.
    let sig = r.sig().to_string();
    let forged_sig = format!("{}{}", if sig.starts_with('0') { "1" } else { "0" }, &sig[1..]);
    let forged = String::from_utf8(good.clone()).unwrap().replace(&sig, &forged_sig).into_bytes();
    assert_eq!(Receipt::from_json(&forged).unwrap().hash(), r.hash());
    refused(s.put_receipt(&forged), ErrorCode::Signature);
    // A tampered value (another hash, the original signature).
    refused(s.put_receipt(&input(&vector("invalid/receipt-tampered-value"))), ErrorCode::Signature);
    assert_eq!(count(&db), 0);
    assert_eq!(s.receipt(r.hash()), None);
    // The real one is stored, and the forged copy is still refused afterwards.
    assert_eq!(s.put_receipt(&good).unwrap(), r.hash());
    refused(s.put_receipt(&forged), ErrorCode::Signature);
    assert_eq!(&*s.receipt(r.hash()).unwrap(), good.as_slice());
    assert_eq!(count(&db), 1);
}

#[test]
fn a_receipt_before_its_namespace_is_refused() {
    let db = TestDb::migrated();
    let s = db.bare_store();
    let v = vector("receipt/stored-number");
    refused(s.put_receipt(&input(&v)), ErrorCode::KeyUnavailable);
    // The calc.example.net document does not help a rates.example.org receipt.
    s.put_namespace("calc.example.net", &key_doc(&v, "calc.example.net"), VALIDATED_AT).unwrap();
    refused(s.put_receipt(&input(&v)), ErrorCode::KeyUnavailable);
    assert_eq!(count(&db), 0);
    s.put_namespace("rates.example.org", &key_doc(&v, "rates.example.org"), VALIDATED_AT).unwrap();
    s.put_receipt(&input(&v)).unwrap();
    // A computed receipt needs its own signer's document, not its inputs'.
    let db2 = TestDb::migrated();
    let s2 = db2.bare_store();
    s2.put_namespace("rates.example.org", &key_doc(&v, "rates.example.org"), VALIDATED_AT).unwrap();
    refused(s2.put_receipt(&input(&vector("receipt/computed-with-lineage"))), ErrorCode::KeyUnavailable);
}

#[test]
fn unknown_key_ids_and_keys_outside_their_window_are_refused() {
    let db = TestDb::migrated();
    let s = db.bare_store();
    let v = vector("receipt/stored-number");
    // A document for rates.example.org that lists only the second key (d9a2...): the receipt
    // names d712..., which it does not publish.
    let full = String::from_utf8(key_doc(&v, "rates.example.org")).unwrap();
    let doc = datumesh_core::json::parse(full.as_bytes()).unwrap();
    let second = doc.get("keys").and_then(|k| match k {
        datumesh_core::json::Value::Array(a) => a.get(1).cloned(),
        _ => None,
    });
    let only_second = datumesh_core::json::Value::Object(vec![
        ("host".into(), datumesh_core::json::Value::String("rates.example.org".into())),
        ("keys".into(), datumesh_core::json::Value::Array(vec![second.unwrap()])),
        ("proto".into(), datumesh_core::json::Value::String("datumesh/1".into())),
    ])
    .to_jcs();
    s.put_namespace("rates.example.org", &only_second, VALIDATED_AT).unwrap();
    refused(s.put_receipt(&input(&v)), ErrorCode::KeyUnknown);

    // The vectors' own cases, each with its own key documents.
    for (id, code) in [
        ("invalid/receipt-unknown-key", ErrorCode::KeyUnknown),
        ("invalid/receipt-key-expired", ErrorCode::KeyNotValid),
        ("invalid/receipt-key-not-yet-valid", ErrorCode::KeyNotValid),
        ("invalid/receipt-wrong-signer", ErrorCode::Signature),
    ] {
        let db = TestDb::migrated();
        let s = db.bare_store();
        let iv = vector(id);
        put_vector_keys(&s, &iv);
        refused(s.put_receipt(&input(&iv)), code);
        assert_eq!(count(&db), 0, "{id}");
    }

    // Signed with the first key (valid until 2024-06-01) but issued after: outside the window.
    let db = TestDb::migrated();
    let s = db.store();
    refused(s.put_receipt(&stored_number_at("1.09", "2024-07-01T00:00:00.000000Z")), ErrorCode::KeyNotValid);
    // Inside it: accepted.
    s.put_receipt(&stored_number_at("1.09", "2024-05-31T23:59:59.999999Z")).unwrap();
}

#[test]
fn a_later_key_document_does_not_touch_stored_receipts() {
    // Revoking (dropping) a key afterwards: what was stored stays and stays readable; new
    // receipts by that key are refused.
    let db = TestDb::migrated();
    let s = db.store();
    let v = vector("receipt/stored-number");
    let h = s.put_receipt(&input(&v)).unwrap();
    let doc = String::from_utf8(key_doc(&v, "rates.example.org")).unwrap();
    let parsed = datumesh_core::json::parse(doc.as_bytes()).unwrap();
    let keys_v = match parsed.get("keys") {
        Some(datumesh_core::json::Value::Array(a)) => a[1..].to_vec(),
        _ => unreachable!(),
    };
    let revoked = datumesh_core::json::Value::Object(vec![
        ("host".into(), datumesh_core::json::Value::String("rates.example.org".into())),
        ("keys".into(), datumesh_core::json::Value::Array(keys_v)),
        ("proto".into(), datumesh_core::json::Value::String("datumesh/1".into())),
    ])
    .to_jcs();
    s.put_namespace("rates.example.org", &revoked, "2024-06-02T00:00:00.000000Z").unwrap();
    assert!(s.receipt(&h).is_some());
    // put_receipt always verifies against the current document: putting the stored receipt
    // again is now refused, and the stored copy stays.
    refused(s.put_receipt(&input(&v)), ErrorCode::KeyUnknown);
    assert!(s.receipt(&h).is_some());
    refused(s.put_receipt(&stored_number_at("1.09", "2024-03-20T00:00:00.000000Z")), ErrorCode::KeyUnknown);
}
