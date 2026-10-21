//! Input validation of the write methods, and reads that never fail loudly.
//!
//! * `put_version` checks that the tx is an instant, that the receipt exists, and that its
//!   cell id and tx equal the arguments.
//! * `put_namespace` parses the key document with `keys_from_response` semantics (a 200 with
//!   this body for this host: `E_KEY_DOC` otherwise), and checks that `validated_at` is an
//!   instant; a namespace is re-validated only forward in time.
//! * Every read method answers malformed or unknown arguments with `None` / empty, without
//!   a database round trip and without panicking.

mod common;

use common::*;
use datumesh_core::{ErrorCode, Receipt, Resolver};
use datumesh_store::{Error, Store};

#[test]
fn put_version_checks_receipt_cell_and_tx() {
    let db = TestDb::migrated();
    let s = db.store();
    let v = vector("receipt/stored-number");
    let bytes = input(&v);
    let r = Receipt::from_json(&bytes).unwrap();
    let cell = r.cell().id();
    // The receipt is not stored yet.
    match s.put_version(&cell, r.tx(), r.hash()) {
        Err(Error::NotFound { what, key }) => {
            assert_eq!(what, "receipt");
            assert_eq!(key, r.hash());
        }
        x => panic!("{x:?}"),
    }
    let h = s.put_receipt(&bytes).unwrap();
    // Another cell id.
    let other = vector("receipt/stored-number-second");
    let other_cell = Receipt::from_json(&input(&other)).unwrap().cell().id();
    assert!(matches!(s.put_version(&other_cell, r.tx(), &h), Err(Error::Mismatch { what: "cell_id", .. })));
    // Another tx.
    assert!(matches!(s.put_version(&cell, "2024-03-15T16:00:00.000001Z", &h), Err(Error::Mismatch { what: "tx", .. })));
    // Malformed arguments: the core's codes.
    assert_eq!(s.put_version(&cell, "2024-03-15 16:00:00", &h).unwrap_err().code(), Some(ErrorCode::CanonInstant));
    assert_eq!(s.put_version(&cell.to_uppercase(), r.tx(), &h).unwrap_err().code(), Some(ErrorCode::CanonHex));
    assert_eq!(s.put_version(&cell, r.tx(), "abc").unwrap_err().code(), Some(ErrorCode::CanonHex));
    // The right one.
    s.put_version(&cell, r.tx(), &h).unwrap();
    assert_eq!(s.latest(&cell).unwrap().receipt_hash, h);
}

#[test]
fn put_namespace_parses_the_key_document() {
    let db = TestDb::migrated();
    let s = db.store();
    let v = vector("receipt/stored-number");
    let doc = key_doc(&v, "rates.example.org");
    let t1 = "2024-03-16T09:00:00.000000Z";
    s.put_namespace("rates.example.org", &doc, t1).unwrap();
    let ns = s.namespace("rates.example.org").unwrap();
    assert_eq!(ns.host, "rates.example.org");
    assert_eq!(ns.keydoc, jcs(&doc));
    assert_eq!(ns.validated_at, t1);
    assert_eq!(ns.suspended_at, None);
    // The stored document reads back as keys.
    assert!(datumesh_core::keys_from_response("rates.example.org", 200, Some(&ns.keydoc)).is_ok());

    // The document of another host, not JSON, not a key document, a host that is not a DNS
    // name: E_KEY_DOC (the codes keys_from_response gives).
    for (host, body) in [
        ("calc.example.net", doc.clone()),
        ("rates.example.org", b"{".to_vec()),
        ("rates.example.org", br#"{"host":"rates.example.org","keys":[],"proto":"datumesh/1"}"#.to_vec()),
        ("localhost", doc.clone()),
        ("Rates.Example.org", doc.clone()),
    ] {
        let e = s.put_namespace(host, &body, t1).unwrap_err();
        let want = datumesh_core::keys_from_response(host, 200, Some(&body)).unwrap_err().code();
        assert_eq!(e.code(), Some(want), "{host}: {e}");
        assert_eq!(want, ErrorCode::KeyDoc);
    }
    // validated_at must be an instant.
    let e = s.put_namespace("rates.example.org", &doc, "yesterday").unwrap_err();
    assert_eq!(e.code(), Some(ErrorCode::CanonInstant));
    // Re-validation moves forward (or repeats); never back.
    s.put_namespace("rates.example.org", &doc, t1).unwrap();
    let t2 = "2024-03-17T09:00:00.000000Z";
    s.put_namespace("rates.example.org", &doc, t2).unwrap();
    assert_eq!(s.namespace("rates.example.org").unwrap().validated_at, t2);
    assert!(matches!(s.put_namespace("rates.example.org", &doc, t1), Err(Error::NotLater { .. })));
    assert_eq!(s.namespace("calc.example.net"), None);
}

#[test]
fn reads_with_malformed_arguments_are_empty() {
    let db = TestDb::migrated();
    let s = db.store();
    let v = vector("receipt/stored-number");
    let bytes = input(&v);
    let h = s.put_receipt(&bytes).unwrap();
    let r = Receipt::from_json(&bytes).unwrap();
    s.put_version(&r.cell().id(), r.tx(), &h).unwrap();
    let cell = r.cell().id();
    let bad = ["", "\0", "abc", "' OR '1'='1", "é", &"a".repeat(100_000), &h.to_uppercase(), &format!("{h}\0")];
    for x in bad {
        assert_eq!(s.receipt(x), None);
        assert_eq!(s.descriptor(x), None);
        assert_eq!(s.blob(x), None);
        assert_eq!(s.latest(x), None);
        assert_eq!(s.version_asof(x, r.tx()), None);
        assert_eq!(s.version_asof(&cell, x), None);
        assert!(s.versions(x, r.tx(), r.tx()).is_empty());
        assert!(s.versions(&cell, x, r.tx()).is_empty());
        assert!(s.versions(&cell, r.tx(), x).is_empty());
        assert!(s.dependents(x).is_empty());
        assert!(s.inputs(x).is_empty());
        assert_eq!(s.namespace(x), None);
        assert_eq!(s.resolve_name(x, r.tx()), None);
        assert_eq!(s.evaluate(r.cell(), x), None);
    }
    // And the good ones answer.
    assert!(s.version_asof(&cell, r.tx()).is_some());
}

#[test]
fn read_errors_are_none_and_logged_not_panics() {
    // A store whose database lost its tables: reads are None / empty, writes are typed
    // database errors.
    let db = TestDb::migrated();
    let s = db.store();
    let h = s.put_receipt(&input(&vector("receipt/stored-number"))).unwrap();
    db.client()
        .batch_execute(
            "ALTER TABLE receipts RENAME TO receipts_gone; ALTER TABLE descriptors RENAME TO descriptors_gone",
        )
        .unwrap();
    // Fresh connections, so no statement prepared against the old tables is reused.
    let s = db.store();
    assert_eq!(s.receipt(&h), None);
    let e = s.put_descriptor(&fx_rate_descriptor()).unwrap_err();
    assert!(matches!(e, Error::Database(_)), "{e:?}");
    // The server's message is in the error's text, not just "db error".
    assert!(e.to_string().contains("descriptors"), "{e}");
    assert_eq!(e.code(), None);
}
