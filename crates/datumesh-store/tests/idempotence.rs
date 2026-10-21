//! Idempotence: the same descriptor, receipt or blob stored twice is stored once, under the
//! same hash. Input the core rejects is refused with the core's error code, and nothing is
//! stored.

mod common;

use common::*;
use datumesh_core::{function_hash, sha256_hex, ErrorCode, Receipt, Resolver};
use datumesh_store::{Error, Store};

fn count(db: &TestDb, table: &str) -> i64 {
    db.client().query_one(&format!("SELECT count(*) FROM {table}"), &[]).unwrap().get(0)
}

#[test]
fn same_descriptor_receipt_blob_twice_is_stored_once() {
    let db = TestDb::migrated();
    let s = db.store();
    let desc = str_of(&vector("function/computed-convert"), "input").as_bytes().to_vec();
    let d1 = s.put_descriptor(&desc).unwrap();
    let d2 = s.put_descriptor(&desc).unwrap();
    let d3 = s.put_descriptor(&jcs(&desc)).unwrap();
    assert_eq!(d1, function_hash(&desc).unwrap());
    assert_eq!((d1.as_str(), d2.as_str()), (d3.as_str(), d3.as_str()));
    assert_eq!(count(&db, "descriptors"), 1);
    // Stored canonical.
    assert_eq!(&*s.descriptor(&d1).unwrap(), jcs(&desc).as_slice());

    let v = vector("receipt/computed-two-inputs");
    let bytes = input(&v);
    let h1 = s.put_receipt(&bytes).unwrap();
    let h2 = s.put_receipt(&bytes).unwrap();
    let h3 = s.put_receipt(&jcs(&bytes)).unwrap();
    assert_eq!(h1, str_of(&v, "sha256"));
    assert!(h1 == h2 && h2 == h3);
    assert_eq!(count(&db, "receipts"), 1);
    assert_eq!(count(&db, "edges"), 2);

    let content = b"col1,col2\n1,2\n".to_vec();
    let b1 = s.put_blob(&content).unwrap();
    let b2 = s.put_blob(&content).unwrap();
    assert_eq!(b1, sha256_hex(&content));
    assert_eq!(b1, b2);
    assert_eq!(count(&db, "blobs"), 1);
    assert_eq!(s.blob(&b1).as_deref(), Some(content.as_slice()));
    let empty = s.put_blob(b"").unwrap();
    assert_eq!(s.blob(&empty).as_deref(), Some(&b""[..]));
    let len: i64 = db.client().query_one("SELECT length FROM blobs WHERE sha256 = $1", &[&b1]).unwrap().get(0);
    assert_eq!(len, content.len() as i64);
}

#[test]
fn receipts_the_core_rejects_are_refused_with_its_code() {
    let db = TestDb::migrated();
    let s = db.store();
    let mut seen = 0;
    for v in vectors("invalid").into_iter().filter(|v| str_of(v, "kind") == "receipt") {
        let bytes = input(&v);
        let Err(core) = Receipt::from_json(&bytes) else { continue };
        seen += 1;
        match s.put_receipt(&bytes) {
            Err(Error::Invalid(e)) => {
                assert_eq!(e.code(), core.code(), "{}", str_of(&v, "id"));
                assert_eq!(e.code().as_str(), str_of(&v, "error"), "{}", str_of(&v, "id"));
            }
            other => panic!("{}: {other:?}", str_of(&v, "id")),
        }
    }
    assert!(seen >= 18, "{seen}");
    assert_eq!(count(&db, "receipts"), 0);
    // Not JSON at all.
    let e = s.put_receipt(b"{").unwrap_err();
    assert_eq!(e.code().map(ErrorCode::as_str), Some("E_PARSE_JSON"), "{e}");
}

#[test]
fn descriptors_the_core_rejects_are_refused_with_its_code() {
    let db = TestDb::migrated();
    let s = db.store();
    let mut seen = 0;
    for v in vectors("invalid").into_iter().filter(|v| str_of(v, "kind") == "function") {
        let bytes = input(&v);
        let Err(core) = function_hash(&bytes) else { continue };
        seen += 1;
        let e = s.put_descriptor(&bytes).unwrap_err();
        assert_eq!(e.code(), Some(core.code()), "{}", str_of(&v, "id"));
    }
    assert!(seen >= 20, "{seen}");
    assert_eq!(count(&db, "descriptors"), 0);
}

#[test]
fn same_hash_with_another_signature_is_a_conflict() {
    // The receipt hash does not cover sig: a receipt with the same payload and another
    // signature has the same hash. The store keeps what it has and reports the conflict
    // instead of replacing a receipt it cannot judge (it does not verify signatures).
    let db = TestDb::migrated();
    let s = db.store();
    let v = vector("receipt/stored-number");
    let good = input(&v);
    let h = s.put_receipt(&good).unwrap();
    let mut other = jcs(&good);
    let sig = str_of(&json_of(&good), "sig").to_string();
    let flipped = format!("{}{}", if sig.starts_with('0') { "1" } else { "0" }, &sig[1..]);
    let text = String::from_utf8(other.clone()).unwrap().replace(&sig, &flipped);
    other = text.into_bytes();
    assert_eq!(Receipt::from_json(&other).unwrap().hash(), h);
    match s.put_receipt(&other) {
        Err(Error::Conflict { hash }) => assert_eq!(hash, h),
        x => panic!("{x:?}"),
    }
    assert_eq!(&*s.receipt(&h).unwrap(), jcs(&good).as_slice());
}

fn json_of(b: &[u8]) -> datumesh_core::json::Value {
    datumesh_core::json::parse(b).unwrap()
}
