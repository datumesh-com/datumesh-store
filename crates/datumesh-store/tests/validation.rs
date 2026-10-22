//! Input validation, and failures.
//!
//! * `put_version` checks that the tx is an instant, that the receipt exists, and that its
//!   cell id and tx equal the arguments.
//! * `put_namespace` parses the key document with `keys_from_response` semantics (a 200 with
//!   this body for this host: `E_KEY_DOC` otherwise), and checks that `validated_at` is an
//!   instant; a namespace is re-validated only forward in time.
//! * Store reads refuse malformed arguments with the core's code (no query); absent
//!   things are `Ok(None)` / `Ok(empty)`; a database failure is `Err`. The Resolver methods
//!   answer all three with `None`. Nothing panics.

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
    assert_eq!(s.latest(&cell).unwrap().unwrap().receipt_hash, h);
}

#[test]
fn put_namespace_parses_the_key_document() {
    let db = TestDb::migrated();
    let s = db.bare_store();
    let v = vector("receipt/stored-number");
    let doc = key_doc(&v, "rates.example.org");
    let t1 = "2024-03-16T09:00:00.000000Z";
    s.put_namespace("rates.example.org", &doc, t1).unwrap();
    let ns = s.namespace("rates.example.org").unwrap().unwrap();
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
    assert_eq!(s.namespace("rates.example.org").unwrap().unwrap().validated_at, t2);
    assert!(matches!(s.put_namespace("rates.example.org", &doc, t1), Err(Error::NotLater { .. })));
    assert_eq!(s.namespace("calc.example.net").unwrap(), None);
}

#[test]
fn reads_with_malformed_arguments_are_invalid() {
    // Store reads refuse a malformed argument with the core's code, without a query;
    // Resolver lookups answer None.
    let db = TestDb::migrated();
    let s = db.store();
    let v = vector("receipt/stored-number");
    let bytes = input(&v);
    let h = s.put_receipt(&bytes).unwrap();
    let r = Receipt::from_json(&bytes).unwrap();
    s.put_version(&r.cell().id(), r.tx(), &h).unwrap();
    let cell = r.cell().id();
    let code = |e: Error| e.code();
    let hex = Some(ErrorCode::CanonHex);
    let instant = Some(ErrorCode::CanonInstant);
    let bad = ["", "\0", "abc", "' OR '1'='1", "é", &"a".repeat(100_000), &h.to_uppercase(), &format!("{h}\0")];
    for x in bad {
        assert_eq!(s.receipt(x), None);
        assert_eq!(s.descriptor(x), None);
        assert_eq!(s.evaluate(r.cell(), x), None);
        assert_eq!(code(s.blob(x).unwrap_err()), hex);
        assert_eq!(code(s.latest(x).unwrap_err()), hex);
        assert_eq!(code(s.version_asof(x, r.tx()).unwrap_err()), hex);
        assert_eq!(code(s.version_asof(&cell, x).unwrap_err()), instant);
        assert_eq!(code(s.versions(x, r.tx(), r.tx()).unwrap_err()), hex);
        assert_eq!(code(s.versions(&cell, x, r.tx()).unwrap_err()), instant);
        assert_eq!(code(s.versions(&cell, r.tx(), x).unwrap_err()), instant);
        assert_eq!(code(s.dependents(x).unwrap_err()), hex);
        assert_eq!(code(s.inputs(x).unwrap_err()), hex);
        assert!(s.namespace(x).unwrap_err().code().is_some(), "{x:?}");
        assert_eq!(code(s.resolve_name(x, r.tx()).unwrap_err()), Some(ErrorCode::CanonName));
        assert_eq!(code(s.resolve_name("rates.example.org/fx_rate", x).unwrap_err()), instant);
    }
    // Well-formed but absent: Ok(None) / Ok(empty).
    let absent = "0".repeat(64);
    assert_eq!(s.blob(&absent).unwrap(), None);
    assert_eq!(s.latest(&absent).unwrap(), None);
    assert!(s.versions(&absent, r.tx(), r.tx()).unwrap().is_empty());
    assert!(s.dependents(&absent).unwrap().is_empty());
    assert_eq!(s.namespace("example.org").unwrap(), None);
    assert_eq!(s.resolve_name("example.org/f", r.tx()).unwrap(), None);
    // And present ones answer.
    assert!(s.version_asof(&cell, r.tx()).unwrap().is_some());
}

/// A store whose connections all fail from some point on: every Store read is an Err,
/// every Resolver method None, nothing panics.
#[test]
fn a_database_outage_is_err_from_store_reads_and_none_from_resolver() {
    use std::sync::atomic::{AtomicBool, Ordering};
    use std::sync::Arc;

    let db = TestDb::migrated();
    let down = Arc::new(AtomicBool::new(false));
    let (config, flag) = (db.config.clone(), down.clone());
    let s = datumesh_store::pg_unstable::with_connector(
        move || {
            if flag.load(Ordering::SeqCst) {
                "host=127.0.0.1 port=1 user=x connect_timeout=1"
                    .parse::<datumesh_store::pg_unstable::postgres::Config>()
                    .unwrap()
                    .connect(datumesh_store::pg_unstable::postgres::NoTls)
            } else {
                config.connect(datumesh_store::pg_unstable::postgres::NoTls)
            }
        },
        datumesh_store::PoolOptions::new().max_size(2),
    )
    .unwrap();
    put_vector_keys(&s, &vector("receipt/stored-number"));
    let fn_hash = s.put_descriptor(&fx_rate_descriptor()).unwrap();
    let v = vector("receipt/stored-number");
    let bytes = input(&v);
    let h = s.put_receipt(&bytes).unwrap();
    let r = Receipt::from_json(&bytes).unwrap();
    let cell = r.cell().id();
    s.put_version(&cell, r.tx(), &h).unwrap();
    assert!(s.latest(&cell).unwrap().is_some());

    // The outage: no new connection succeeds, and the open ones are killed.
    down.store(true, Ordering::SeqCst);
    db.client()
        .execute(
            "SELECT pg_terminate_backend(pid) FROM pg_stat_activity WHERE datname = current_database() AND pid <> pg_backend_pid()",
            &[],
        )
        .unwrap();
    std::thread::sleep(std::time::Duration::from_millis(200));
    // Twice: the first use may meet a killed connection, the second a failing connector.
    for _ in 0..2 {
        let db_err = |e: Error| assert!(matches!(e, Error::Database(_) | Error::PoolTimeout), "{e:?}");
        db_err(s.blob(&h).unwrap_err());
        db_err(s.version_asof(&cell, r.tx()).unwrap_err());
        db_err(s.versions(&cell, r.tx(), r.tx()).unwrap_err());
        db_err(s.latest(&cell).unwrap_err());
        db_err(s.dependents(&h).unwrap_err());
        db_err(s.inputs(&h).unwrap_err());
        db_err(s.namespace("rates.example.org").unwrap_err());
        db_err(s.resolve_name("rates.example.org/fx_rate", r.tx()).unwrap_err());
        assert_eq!(s.descriptor(&fn_hash), None);
        assert_eq!(s.receipt(&h), None);
        assert_eq!(s.evaluate(r.cell(), r.asof()), None);
        db_err(s.put_receipt(&bytes).unwrap_err());
    }
    // verify through the store: the descriptor is unavailable.
    let out = datumesh_core::verify(&bytes, &keys(&v), Some(&s as &dyn Resolver), None);
    assert_eq!(out.code, Some(ErrorCode::FnUnavailable));
    // Back up: answers again.
    down.store(false, Ordering::SeqCst);
    assert!(s.latest(&cell).unwrap().is_some());
}

#[test]
fn a_renamed_table_is_a_database_error_with_the_servers_message() {
    let db = TestDb::migrated();
    let s = db.store();
    let h = s.put_receipt(&input(&vector("receipt/stored-number"))).unwrap();
    db.client()
        .batch_execute(
            "ALTER TABLE receipts RENAME TO receipts_gone; ALTER TABLE descriptors RENAME TO descriptors_gone; \
             ALTER TABLE edges RENAME TO edges_gone",
        )
        .unwrap();
    // Fresh connections, so no statement prepared against the old tables is reused.
    let s = db.bare_store();
    assert_eq!(s.receipt(&h), None);
    let e = s.put_descriptor(&fx_rate_descriptor()).unwrap_err();
    assert!(matches!(e, Error::Database(_)), "{e:?}");
    // The server's message is in the error's text, not just "db error".
    assert!(e.to_string().contains("descriptors"), "{e}");
    assert_eq!(e.code(), None);
    let e = s.dependents(&h).unwrap_err();
    assert!(matches!(e, Error::Database(_)), "{e:?}");
}

/// `Error::Database` carries a `DatabaseError`: the SQLSTATE, the server's message, and the
/// underlying error as its source (type erased).
#[test]
fn a_database_error_has_the_sqlstate_the_servers_message_and_a_source() {
    // An empty database: the store connects (it prepares nothing up front), and the first
    // read meets a missing table.
    let db = TestDb::empty();
    let s = db.bare_store();
    let cell = "0".repeat(64);
    let e = s.latest(&cell).unwrap_err();
    let Error::Database(d) = &e else { panic!("{e:?}") };
    assert_eq!(d.code(), Some("42P01"));
    assert_eq!(d.message(), "relation \"versions\" does not exist");
    let source = std::error::Error::source(d).expect("the driver's error");
    assert!(!source.to_string().is_empty());
    // Display as in 0.1.0: the server's message and the SQLSTATE.
    assert_eq!(d.to_string(), "relation \"versions\" does not exist (42P01)");
    assert_eq!(e.to_string(), "database: relation \"versions\" does not exist (42P01)");
    assert!(std::error::Error::source(&e).is_some());
    // The core's code is for Error::Invalid only.
    assert_eq!(e.code(), None);

    // Another SQLSTATE: a table without the columns the statement names is 42703.
    db.client().batch_execute("CREATE TABLE versions (cell_id text)").unwrap();
    let s = db.bare_store();
    let Error::Database(d) = s.latest(&cell).unwrap_err() else { panic!() };
    assert_eq!(d.code(), Some("42703"), "{d}");
    assert!(d.message().contains("does not exist"), "{d}");

    // A failure that is not the server's (nothing listens): no SQLSTATE, the driver's text.
    let e = datumesh_store::PgStore::connect("postgres://x@127.0.0.1:1/x?connect_timeout=1").unwrap_err();
    let Error::Database(d) = &e else { panic!("{e:?}") };
    assert_eq!(d.code(), None);
    assert!(!d.message().is_empty());
    assert!(std::error::Error::source(d).is_some());
    assert_eq!(e.to_string(), format!("database: {}", d.message()));
}
