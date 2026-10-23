//! Computed versions and answering as of A (0.2.0).
//!
//! * `put_computed`: insert-only, in any order, keyed on (cell_id, tx, asof) with the asof of
//!   the receipt; idempotent; another receipt at an existing key is `Error::Conflict`; two
//!   receipts at one tx with different asof are both kept; the receipt must be stored, of
//!   that cell, at that tx; malformed arguments are the core's codes, without a query;
//!   UPDATE, DELETE and TRUNCATE fail in the database; concurrent writers.
//! * `computed_asof`: the greatest asof <= A, ties the greatest tx.
//! * `stored_asof`: the latest version (by tx) with tx <= A whose receipt's asof <= A.
//! * `cell_dependents`: the cells of stored receipts that read any receipt of a cell.
//! * `Version::asof` is the receipt's asof, for every method returning a `Version`.

mod common;

use std::sync::{Arc, Barrier};
use std::thread;

use common::*;
use datumesh_core::{ErrorCode, Receipt, Resolver};
use datumesh_store::{Error, PgStore, PoolOptions, Store, Version};

/// Midnight of a day of March 2024.
fn day(d: u32) -> String {
    format!("2024-03-{d:02}T00:00:00.000000Z")
}

/// The EUR/USD cell's id.
fn usd_cell() -> String {
    Receipt::from_json(&input(&vector("receipt/stored-number"))).unwrap().cell().id()
}

/// Stores a stored-number receipt with `value` at `tx`, as of `asof`; returns its hash.
fn put(s: &PgStore, value: &str, tx: &str, asof: &str) -> String {
    s.put_receipt(&stored_number_with(value, tx, asof)).unwrap()
}

fn hash_of(v: Option<Version>) -> Option<String> {
    v.map(|v| v.receipt_hash)
}

#[test]
fn put_computed_is_idempotent_and_takes_any_order() {
    let db = TestDb::migrated();
    let s = db.store();
    let cell = usd_cell();
    let h5 = put(&s, "1.5", &day(5), &day(5));
    let h1 = put(&s, "1.1", &day(1), &day(1));
    let h3 = put(&s, "1.3", &day(3), &day(3));
    // Newest first, then an older one, then one in between: any order.
    s.put_computed(&cell, &day(5), &h5).unwrap();
    s.put_computed(&cell, &day(1), &h1).unwrap();
    s.put_computed(&cell, &day(3), &h3).unwrap();
    // Again: nothing changes.
    s.put_computed(&cell, &day(5), &h5).unwrap();
    s.put_computed(&cell, &day(1), &h1).unwrap();
    assert_eq!(count(&db, "computed_versions"), 3);
    // Computed versions are not stored versions.
    assert_eq!(s.latest(&cell).unwrap(), None);
    assert_eq!(count(&db, "versions"), 0);
    // Stored versions do not need computed ones either, and the two do not interfere.
    s.put_version(&cell, &day(3), &h3).unwrap();
    s.put_computed(&cell, &day(3), &h3).unwrap();
    assert_eq!(count(&db, "computed_versions"), 3);
}

#[test]
fn another_receipt_at_an_existing_key_is_a_conflict() {
    let db = TestDb::migrated();
    let s = db.store();
    let cell = usd_cell();
    let a = put(&s, "1.3", &day(3), &day(3));
    let b = put(&s, "1.4", &day(3), &day(3));
    assert_ne!(a, b);
    s.put_computed(&cell, &day(3), &a).unwrap();
    match s.put_computed(&cell, &day(3), &b) {
        Err(Error::Conflict { hash }) => assert_eq!(hash, b),
        x => panic!("{x:?}"),
    }
    // The first is kept.
    assert_eq!(hash_of(s.computed_asof(&cell, &day(3)).unwrap()), Some(a.clone()));
    assert_eq!(count(&db, "computed_versions"), 1);
    // The first again is still fine.
    s.put_computed(&cell, &day(3), &a).unwrap();
}

#[test]
fn receipts_at_one_tx_with_different_asof_are_both_kept() {
    let db = TestDb::migrated();
    let s = db.store();
    let cell = usd_cell();
    let v = put(&s, "1.3", &day(3), &day(3));
    let late = put(&s, "1.31", &day(3), &day(10));
    s.put_computed(&cell, &day(3), &late).unwrap();
    s.put_computed(&cell, &day(3), &v).unwrap();
    assert_eq!(count(&db, "computed_versions"), 2);
    let rows = db
        .client()
        .query("SELECT tx, asof, receipt_hash FROM computed_versions ORDER BY asof", &[])
        .unwrap()
        .iter()
        .map(|r| (r.get::<_, String>(0), r.get::<_, String>(1), r.get::<_, String>(2)))
        .collect::<Vec<_>>();
    assert_eq!(rows, [(day(3), day(3), v), (day(3), day(10), late)]);
}

#[test]
fn put_computed_checks_receipt_cell_and_tx() {
    let db = TestDb::migrated();
    let s = db.store();
    let v = vector("receipt/stored-number");
    let bytes = input(&v);
    let r = Receipt::from_json(&bytes).unwrap();
    let cell = r.cell().id();
    match s.put_computed(&cell, r.tx(), r.hash()) {
        Err(Error::NotFound { what, key }) => {
            assert_eq!(what, "receipt");
            assert_eq!(key, r.hash());
        }
        x => panic!("{x:?}"),
    }
    let h = s.put_receipt(&bytes).unwrap();
    let other = Receipt::from_json(&input(&vector("receipt/stored-number-second"))).unwrap().cell().id();
    match s.put_computed(&other, r.tx(), &h) {
        Err(Error::Mismatch { what, expected, found }) => {
            assert_eq!((what, expected.as_str(), found.as_str()), ("cell_id", other.as_str(), cell.as_str()));
        }
        x => panic!("{x:?}"),
    }
    // The tx is the receipt's tx, not its asof.
    match s.put_computed(&cell, r.asof(), &h) {
        Err(Error::Mismatch { what, found, .. }) => assert_eq!((what, found.as_str()), ("tx", r.tx())),
        x => panic!("{x:?}"),
    }
    assert_eq!(count(&db, "computed_versions"), 0);
    s.put_computed(&cell, r.tx(), &h).unwrap();
    let got = s.computed_asof(&cell, r.asof()).unwrap().unwrap();
    assert_eq!(
        (got.cell_id.as_str(), got.tx.as_str(), got.asof.as_str(), got.receipt_hash.as_str()),
        (cell.as_str(), r.tx(), r.asof(), h.as_str())
    );
}

/// Malformed arguments are refused with the core's codes, without a query: the store here
/// is on an empty database (no schema), where any query would be a database error.
#[test]
fn malformed_arguments_are_the_cores_codes_without_a_query() {
    let db = TestDb::empty();
    let s = db.bare_store();
    let cell = usd_cell();
    let h = "a".repeat(64);
    let t = day(3);
    let code = |e: Error| e.code();
    assert_eq!(code(s.put_computed(&cell.to_uppercase(), &t, &h).unwrap_err()), Some(ErrorCode::CanonHex));
    assert_eq!(code(s.put_computed(&cell, "2024-03-03T00:00:00Z", &h).unwrap_err()), Some(ErrorCode::CanonInstant));
    assert_eq!(code(s.put_computed(&cell, &t, "abc").unwrap_err()), Some(ErrorCode::CanonHex));
    assert_eq!(code(s.computed_asof("", &t).unwrap_err()), Some(ErrorCode::CanonHex));
    assert_eq!(code(s.computed_asof(&cell, "yesterday").unwrap_err()), Some(ErrorCode::CanonInstant));
    assert_eq!(code(s.stored_asof(&h[1..], &t).unwrap_err()), Some(ErrorCode::CanonHex));
    assert_eq!(
        code(s.stored_asof(&cell, "2024-03-03T00:00:00.000000+00:00").unwrap_err()),
        Some(ErrorCode::CanonInstant)
    );
    assert_eq!(code(s.cell_dependents("zz").unwrap_err()), Some(ErrorCode::CanonHex));
    assert_eq!(code(s.computed_asof_with_receipt(&cell, "\0").unwrap_err()), Some(ErrorCode::CanonInstant));
    assert_eq!(code(s.stored_asof_with_receipt("x", &t).unwrap_err()), Some(ErrorCode::CanonHex));
    // Well-formed arguments do reach the (empty) database.
    assert!(matches!(s.computed_asof(&cell, &t), Err(Error::Database(_))));
    assert!(matches!(s.cell_dependents(&cell), Err(Error::Database(_))));
}

fn sql_fails(db: &TestDb, sql: &str) {
    let e = db.client().batch_execute(sql).expect_err(sql);
    let msg = e.as_db_error().map(|d| d.message().to_string()).unwrap_or_else(|| e.to_string());
    assert!(msg.contains("append-only"), "{sql}: {msg}");
}

#[test]
fn update_delete_truncate_fail_in_the_database_and_raw_inserts_take_any_order() {
    let db = TestDb::migrated();
    let s = db.store();
    let cell = usd_cell();
    let h5 = put(&s, "1.5", &day(5), &day(5));
    let h1 = put(&s, "1.1", &day(1), &day(1));
    s.put_computed(&cell, &day(5), &h5).unwrap();
    sql_fails(&db, "UPDATE computed_versions SET tx = tx");
    sql_fails(&db, &format!("UPDATE computed_versions SET receipt_hash = '{h1}'"));
    sql_fails(&db, "DELETE FROM computed_versions");
    sql_fails(&db, "TRUNCATE computed_versions");
    sql_fails(&db, "TRUNCATE computed_versions CASCADE");
    assert!(db.client().batch_execute("TRUNCATE receipts CASCADE").is_err());
    assert_eq!(count(&db, "computed_versions"), 1);
    // No tx-order trigger: an older row inserted after a newer one in raw SQL is fine.
    db.client()
        .execute(
            "INSERT INTO computed_versions (cell_id, tx, tx_at, asof, asof_at, receipt_hash)
             VALUES ($1, $2, $2::text::timestamptz, $2, $2::text::timestamptz, $3)",
            &[&cell, &day(1), &h1],
        )
        .unwrap();
    assert_eq!(hash_of(s.computed_asof(&cell, &day(2)).unwrap()), Some(h1));
    // The foreign key: no row for a receipt the store does not have.
    assert!(db
        .client()
        .execute(
            "INSERT INTO computed_versions (cell_id, tx, tx_at, asof, asof_at, receipt_hash)
             VALUES ($1, $2, $2::text::timestamptz, $2, $2::text::timestamptz, $3)",
            &[&cell, &day(9), &"0".repeat(64)],
        )
        .is_err());
}

#[test]
fn concurrent_writers_of_one_row_and_of_one_key() {
    let db = TestDb::migrated();
    let s = Arc::new(db.store_with(PoolOptions::new().max_size(8)));
    let cell = usd_cell();
    const N: usize = 8;
    for round in 0..5u32 {
        let tx = format!("2024-03-{:02}T12:00:00.000000Z", round + 1);
        // The same row from every thread: all succeed, one row.
        let same = put(&s, &format!("2.{}", round + 1), &tx, &tx);
        // A different receipt per thread for one other key: exactly one wins.
        let tx2 = format!("2024-03-{:02}T13:00:00.000000Z", round + 1);
        let rivals: Vec<String> = (0..N).map(|i| put(&s, &format!("3.{}{}", round + 1, i + 1), &tx2, &tx2)).collect();
        let barrier = Arc::new(Barrier::new(N));
        let handles: Vec<_> = (0..N)
            .map(|i| {
                let (s, barrier, cell, tx, same, tx2, mine) = (
                    s.clone(),
                    barrier.clone(),
                    cell.clone(),
                    tx.clone(),
                    same.clone(),
                    tx2.clone(),
                    rivals[i].clone(),
                );
                thread::spawn(move || {
                    barrier.wait();
                    let a = s.put_computed(&cell, &tx, &same);
                    let b = s.put_computed(&cell, &tx2, &mine);
                    (a, b, mine)
                })
            })
            .collect();
        let results: Vec<_> = handles.into_iter().map(|h| h.join().unwrap()).collect();
        for (a, _, _) in &results {
            assert!(a.is_ok(), "{a:?}");
        }
        let winners: Vec<&String> = results.iter().filter(|(_, b, _)| b.is_ok()).map(|(_, _, h)| h).collect();
        assert_eq!(winners.len(), 1, "round {round}: {results:?}");
        for (_, b, mine) in &results {
            match b {
                Ok(()) => {}
                Err(Error::Conflict { hash }) => assert_eq!(hash, mine),
                Err(e) => panic!("round {round}: {e:?}"),
            }
        }
        assert_eq!(hash_of(s.computed_asof(&cell, &tx2).unwrap()).as_ref(), Some(winners[0]));
        assert_eq!(hash_of(s.computed_asof(&cell, &tx).unwrap()), Some(same));
    }
    assert_eq!(count(&db, "computed_versions"), 10);
}

#[test]
fn computed_asof_before_between_and_exactly_at() {
    let db = TestDb::migrated();
    let s = db.store();
    let cell = usd_cell();
    let h2 = put(&s, "1.2", &day(2), &day(2));
    let h6 = put(&s, "1.6", &day(6), &day(6));
    s.put_computed(&cell, &day(2), &h2).unwrap();
    s.put_computed(&cell, &day(6), &h6).unwrap();
    let at = |a: &str| hash_of(s.computed_asof(&cell, a).unwrap());
    assert_eq!(at("0001-01-01T00:00:00.000000Z"), None);
    assert_eq!(at("2024-03-01T23:59:59.999999Z"), None);
    assert_eq!(at(&day(2)), Some(h2.clone()));
    assert_eq!(at(&day(4)), Some(h2.clone()));
    assert_eq!(at("2024-03-05T23:59:59.999999Z"), Some(h2.clone()));
    assert_eq!(at(&day(6)), Some(h6.clone()));
    assert_eq!(at("9999-12-31T23:59:59.999999Z"), Some(h6));
    // Another cell has none.
    assert_eq!(s.computed_asof(&"0".repeat(64), &day(6)).unwrap(), None);
}

/// v at tx 3 as of 3, and v' at tx 3 as of 10 (late-signed): at 5, v; at 12, v'. Between
/// equal asof, the greatest tx answers.
#[test]
fn computed_asof_late_signed_and_ties() {
    let db = TestDb::migrated();
    let s = db.store();
    let cell = usd_cell();
    let v = put(&s, "1.3", &day(3), &day(3));
    let v_late = put(&s, "1.31", &day(3), &day(10));
    s.put_computed(&cell, &day(3), &v_late).unwrap();
    s.put_computed(&cell, &day(3), &v).unwrap();
    let at = |a: &str| s.computed_asof(&cell, a).unwrap();
    assert_eq!(at(&day(2)), None);
    assert_eq!(hash_of(at(&day(5))), Some(v.clone()));
    assert_eq!(hash_of(at("2024-03-09T23:59:59.999999Z")), Some(v.clone()));
    assert_eq!(hash_of(at(&day(10))), Some(v_late.clone()));
    let got = at(&day(12)).unwrap();
    assert_eq!(
        (got.tx.as_str(), got.asof.as_str(), got.receipt_hash.as_str()),
        (day(3).as_str(), day(10).as_str(), v_late.as_str())
    );
    let got = at(&day(5)).unwrap();
    assert_eq!((got.tx, got.asof), (day(3), day(3)));
    // A third at tx 4, also as of 10: same asof as v', greater tx: it answers from 10 on.
    let w = put(&s, "1.4", &day(4), &day(10));
    s.put_computed(&cell, &day(4), &w).unwrap();
    assert_eq!(hash_of(at(&day(12))), Some(w.clone()));
    assert_eq!(hash_of(at(&day(10))), Some(w));
    assert_eq!(hash_of(at(&day(9))), Some(v));
}

/// Historical backfill: a newer version first, an older one computed afterwards.
#[test]
fn computed_asof_after_historical_backfill() {
    let db = TestDb::migrated();
    let s = db.store();
    let cell = usd_cell();
    let new = put(&s, "1.2", &day(20), &day(20));
    s.put_computed(&cell, &day(20), &new).unwrap();
    assert_eq!(s.computed_asof(&cell, &day(7)).unwrap(), None);
    let old = put(&s, "1.05", &day(5), &day(5));
    s.put_computed(&cell, &day(5), &old).unwrap();
    assert_eq!(hash_of(s.computed_asof(&cell, &day(4)).unwrap()), None);
    assert_eq!(hash_of(s.computed_asof(&cell, &day(7)).unwrap()), Some(old.clone()));
    assert_eq!(hash_of(s.computed_asof(&cell, &day(19)).unwrap()), Some(old));
    assert_eq!(hash_of(s.computed_asof(&cell, &day(21)).unwrap()), Some(new));
}

/// The vectors' stored-number receipt: tx 2024-03-15T16:00, asof 2024-03-16T09:00. As of
/// 2024-03-16T00:00 it is not yet an answer (its asof is later), though `version_asof`,
/// which looks at tx only, returns it.
#[test]
fn stored_asof_needs_both_times() {
    let db = TestDb::migrated();
    let s = db.store();
    let v = vector("receipt/stored-number");
    let bytes = input(&v);
    let r = Receipt::from_json(&bytes).unwrap();
    assert_eq!((r.tx(), r.asof()), ("2024-03-15T16:00:00.000000Z", "2024-03-16T09:00:00.000000Z"));
    let h = s.put_receipt(&bytes).unwrap();
    let cell = r.cell().id();
    s.put_version(&cell, r.tx(), &h).unwrap();

    let midnight = "2024-03-16T00:00:00.000000Z";
    assert_eq!(s.stored_asof(&cell, midnight).unwrap(), None);
    assert_eq!(hash_of(s.version_asof(&cell, midnight).unwrap()), Some(h.clone()));
    assert_eq!(s.stored_asof(&cell, "2024-03-15T15:59:59.999999Z").unwrap(), None);
    assert_eq!(s.stored_asof(&cell, "2024-03-16T08:59:59.999999Z").unwrap(), None);
    let got = s.stored_asof(&cell, "2024-03-16T09:00:00.000000Z").unwrap().unwrap();
    assert_eq!(
        (got.cell_id.as_str(), got.tx.as_str(), got.asof.as_str(), got.receipt_hash.as_str()),
        (cell.as_str(), r.tx(), r.asof(), h.as_str())
    );
    assert_eq!(hash_of(s.stored_asof(&cell, "9999-12-31T23:59:59.999999Z").unwrap()), Some(h.clone()));

    // Version::asof is the receipt's asof for the other methods too.
    assert_eq!(s.version_asof(&cell, midnight).unwrap().unwrap().asof, r.asof());
    assert_eq!(s.latest(&cell).unwrap().unwrap().asof, r.asof());
    let all = s.versions(&cell, "0001-01-01T00:00:00.000000Z", "9999-12-31T23:59:59.999999Z").unwrap();
    assert_eq!(all.iter().map(|v| v.asof.as_str()).collect::<Vec<_>>(), [r.asof()]);
}

/// A late-signed second version (tx 5, as of 9) after v1 (tx 3, as of 3): between 5 and 9,
/// v1 still answers (version_asof says v2).
#[test]
fn stored_asof_with_a_late_signed_second_version() {
    let db = TestDb::migrated();
    let s = db.store();
    let cell = usd_cell();
    let v1 = put(&s, "1.3", &day(3), &day(3));
    let v2 = put(&s, "1.5", &day(5), &day(9));
    s.put_version(&cell, &day(3), &v1).unwrap();
    s.put_version(&cell, &day(5), &v2).unwrap();
    let at = |a: &str| hash_of(s.stored_asof(&cell, a).unwrap());
    assert_eq!(at(&day(2)), None);
    assert_eq!(at(&day(3)), Some(v1.clone()));
    assert_eq!(at(&day(4)), Some(v1.clone()));
    assert_eq!(at(&day(5)), Some(v1.clone()));
    assert_eq!(hash_of(s.version_asof(&cell, &day(5)).unwrap()), Some(v2.clone()));
    assert_eq!(at(&day(6)), Some(v1.clone()));
    assert_eq!(at("2024-03-08T23:59:59.999999Z"), Some(v1));
    assert_eq!(at(&day(9)), Some(v2.clone()));
    assert_eq!(at(&day(30)), Some(v2));
    let latest = s.latest(&cell).unwrap().unwrap();
    assert_eq!((latest.tx, latest.asof), (day(5), day(9)));
}

/// The receipt bytes come with the answer in one call.
#[test]
fn answers_with_their_receipts() {
    let db = TestDb::migrated();
    let s = db.store();
    let cell = usd_cell();
    let v1 = put(&s, "1.3", &day(3), &day(3));
    let v2 = put(&s, "1.5", &day(5), &day(9));
    s.put_version(&cell, &day(3), &v1).unwrap();
    s.put_version(&cell, &day(5), &v2).unwrap();
    s.put_computed(&cell, &day(5), &v2).unwrap();
    assert!(s.computed_asof_with_receipt(&cell, &day(8)).unwrap().is_none());
    assert!(s.stored_asof_with_receipt(&cell, &day(2)).unwrap().is_none());
    let (v, bytes) = s.stored_asof_with_receipt(&cell, &day(6)).unwrap().unwrap();
    assert_eq!(Some(v.clone()), s.stored_asof(&cell, &day(6)).unwrap());
    assert_eq!(v.receipt_hash, v1);
    assert_eq!(bytes.as_ref(), s.receipt(&v1).unwrap().as_ref());
    let (v, bytes) = s.computed_asof_with_receipt(&cell, &day(9)).unwrap().unwrap();
    assert_eq!(Some(v.clone()), s.computed_asof(&cell, &day(9)).unwrap());
    assert_eq!(v.receipt_hash, v2);
    assert_eq!(Receipt::from_json(&bytes).unwrap().hash(), v2);
}

/// The cells reading a cell, from the vectors (each vector's receipt and the input receipts
/// of its `recompute` object): computed-with-lineage reads stored-number; the spec-0.3.0
/// cross-rate vectors read stored-number and a EUR/GBP receipt; computed-two-inputs reads
/// stored-number and stored-number-second.
#[test]
fn cell_dependents_from_the_vectors() {
    let db = TestDb::migrated();
    let s = db.store();
    let ids = [
        "receipt/stored-number",
        "receipt/stored-number-second",
        "receipt/computed-with-lineage",
        "receipt/computed-input-equal-asof",
        "receipt/computed-inputs-earlier-asof",
        "receipt/computed-two-inputs",
        "receipt/eval-unavailable-computed",
    ];
    let mut receipts: Vec<Receipt> = Vec::new();
    let mut put_bytes = |b: &[u8]| {
        s.put_receipt(b).unwrap();
        let r = Receipt::from_json(b).unwrap();
        if !receipts.iter().any(|x| x.hash() == r.hash()) {
            receipts.push(r.clone());
        }
        r
    };
    let mut main = Vec::new();
    for id in ids {
        let v = vector(id);
        put_vector_keys(&s, &v);
        for (_, t) in v.get("recompute").and_then(|r| r.get("inputs")).and_then(|i| i.as_object()).unwrap_or(&[]) {
            put_bytes(t.as_str().unwrap().as_bytes());
        }
        main.push(put_bytes(&input(&v)));
    }
    let cell = |i: usize| main[i].cell().id();
    let (usd, gbp, lineage, cross, two) = (cell(0), cell(1), cell(2), cell(3), cell(5));
    assert_eq!(cell(3), cell(4), "the two cross-rate vectors are one cell");

    // Expected, from the receipts themselves: the cells of stored receipts listing any
    // stored receipt of the cell among their inputs.
    let expected = |c: &str| {
        let of_c: Vec<&str> = receipts.iter().filter(|r| r.cell().id() == c).map(|r| r.hash()).collect();
        let mut out: Vec<String> = receipts
            .iter()
            .filter(|r| r.inputs().iter().any(|i| of_c.contains(&i.as_str())))
            .map(|r| r.cell().id())
            .collect();
        out.sort();
        out.dedup();
        out
    };
    let got = s.cell_dependents(&usd).unwrap();
    assert_eq!(got, expected(&usd));
    for c in [&lineage, &cross, &two] {
        assert!(got.contains(c), "{c} missing from {got:?}");
    }
    assert!(got.windows(2).all(|w| w[0] < w[1]), "ascending and unique: {got:?}");
    // The cross rate reads the EUR/GBP cell through the receipt its recompute object holds
    // (not stored-number-second's own receipt, which nothing reads).
    let got = s.cell_dependents(&gbp).unwrap();
    assert_eq!(got, expected(&gbp));
    assert!(got.contains(&cross), "{got:?}");
    // Computed cells nobody reads.
    for c in [&lineage, &cross, &two] {
        assert!(s.cell_dependents(c).unwrap().is_empty());
    }
    // A cell the store has no receipt of; and a receipt hash is not a cell id.
    assert!(s.cell_dependents(&"9".repeat(64)).unwrap().is_empty());
    assert!(s.cell_dependents(main[0].hash()).unwrap().is_empty());
}

/// An input the store does not hold has no cell: it makes no dependent.
#[test]
fn cell_dependents_ignore_inputs_the_store_does_not_hold() {
    let db = TestDb::migrated();
    let s = db.store();
    let two = vector("receipt/computed-two-inputs");
    s.put_receipt(&input(&two)).unwrap();
    let usd = usd_cell();
    assert!(s.cell_dependents(&usd).unwrap().is_empty());
    s.put_receipt(&input(&vector("receipt/stored-number"))).unwrap();
    let two_cell = Receipt::from_json(&input(&two)).unwrap().cell().id();
    assert_eq!(s.cell_dependents(&usd).unwrap(), [two_cell]);
}

/// Any receipt of the cell counts, not only its stored versions or its latest one.
#[test]
fn cell_dependents_through_any_receipt_of_the_cell() {
    let db = TestDb::migrated();
    let s = db.store();
    let cell = usd_cell();
    let lineage_cell = Receipt::from_json(&input(&vector("receipt/computed-with-lineage"))).unwrap().cell().id();
    assert!(s.cell_dependents(&cell).unwrap().is_empty());
    let old = put(&s, "1.1", &day(1), &day(1));
    let _new = put(&s, "1.2", &day(2), &day(2));
    // Two receipts of the lineage cell read the old one: one dependent cell.
    s.put_receipt(&lineage_with(&[&old], "110", &day(1), &day(1))).unwrap();
    s.put_receipt(&lineage_with(&[&old], "111", &day(4), &day(4))).unwrap();
    assert_eq!(s.cell_dependents(&cell).unwrap(), std::slice::from_ref(&lineage_cell));
    assert!(s.cell_dependents(&lineage_cell).unwrap().is_empty());
}
