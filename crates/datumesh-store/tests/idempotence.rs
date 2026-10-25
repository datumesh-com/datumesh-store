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
    assert_eq!(s.blob(&b1).unwrap().as_deref(), Some(content.as_slice()));
    let empty = s.put_blob(b"").unwrap();
    assert_eq!(s.blob(&empty).unwrap().as_deref(), Some(&b""[..]));
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

/// spec-0.4.0 descriptors: `signature.order` and argument `extent` (formula descriptors
/// always carry `order`) are stored, canonical, under the core's function hash (0.3.1;
/// until 0.3.0 the store refused them with `E_SCHEMA`, as a spec-0.3.0 verifier does).
#[test]
fn descriptors_with_order_and_extent_are_stored() {
    let db = TestDb::migrated();
    let s = db.store();
    let mut seen = 0;
    for v in vectors("function") {
        let bytes = input(&v);
        let text = String::from_utf8(bytes.clone()).unwrap();
        if !text.contains("\"order\"") && !text.contains("\"extent\"") {
            continue;
        }
        let id = str_of(&v, "id");
        let h = s.put_descriptor(&bytes).unwrap();
        assert_eq!(h, function_hash(&bytes).unwrap(), "{id}");
        assert_eq!(h, str_of(&v, "sha256"), "{id}");
        assert_eq!(&*s.descriptor(&h).unwrap(), str_of(&v, "canonical").as_bytes(), "{id}");
        assert_eq!(s.put_descriptor(&bytes).unwrap(), h, "{id}");
        seen += 1;
    }
    assert!(seen >= 4, "{seen}");
    // The slice vectors' formula descriptors too.
    let rc = vector("receipt/slice-weighted-mean");
    let d = rc.get("recompute").and_then(|r| r.get("descriptor")).unwrap().to_jcs();
    assert_eq!(s.put_descriptor(&d).unwrap(), function_hash(&d).unwrap());
}

#[test]
fn same_hash_with_other_bytes_is_a_conflict() {
    // After verification, a second receipt with a stored hash but other bytes can only be
    // another valid signature by the same key (the hash covers computed_by, so a rotated key
    // gives another hash; Ed25519 as RFC 8032 signs deterministically, so this needs a
    // randomised signer). Such bytes are planted here with SQL; the store keeps them and
    // reports the conflict.
    let db = TestDb::migrated();
    let s = db.store();
    let v = vector("receipt/stored-number");
    let good = input(&v);
    let r = Receipt::from_json(&good).unwrap();
    // JSON with the receipt's members (its asof included: the database derives the asof
    // column from the bytes) and another signature.
    let mut planted_v = datumesh_core::json::parse(&good).unwrap();
    if let datumesh_core::json::Value::Object(members) = &mut planted_v {
        for (k, v) in members.iter_mut() {
            if k == "sig" {
                *v = datumesh_core::json::Value::String("planted".into());
            }
        }
    }
    let planted = planted_v.to_jcs();
    assert_ne!(planted, jcs(&good));
    db.client()
        .execute(
            "INSERT INTO receipts (hash, bytes, cell_id, tx, tx_at, fn_hash, publisher)
             VALUES ($1, $2, $3, $4, $4::text::timestamptz, $5, $6)",
            &[&r.hash(), &planted, &r.cell().id(), &r.tx(), &r.cell().fn_hash, &r.publisher()],
        )
        .unwrap();
    match s.put_receipt(&good) {
        Err(Error::Conflict { hash }) => assert_eq!(hash, r.hash()),
        x => panic!("{x:?}"),
    }
    assert_eq!(&*s.receipt(r.hash()).unwrap(), planted.as_slice());
}
