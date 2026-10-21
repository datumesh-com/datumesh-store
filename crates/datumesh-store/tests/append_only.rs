//! Append-only history: `versions` and `name_bindings` refuse UPDATE, DELETE and TRUNCATE in
//! the database itself, `put_version` / `bind_name` refuse a tx not later than the latest
//! with a typed error, and the tx order holds under concurrent writers, both through the
//! API and for raw SQL inserts racing each other.

mod common;

use std::sync::{Arc, Barrier};
use std::thread;
use std::time::Duration;

use common::*;
use datumesh_core::Receipt;
use datumesh_store::{Error, Store};

const TX1: &str = "2024-03-15T16:00:00.000000Z";
const TX2: &str = "2024-03-18T10:00:00.000000Z";

/// A store holding the fx_rate descriptor, two versions of the EUR/USD cell and a name with
/// two bindings. Returns the store, the cell id and the two receipt hashes.
fn seeded(db: &TestDb) -> (datumesh_store::PgStore, String, String, String) {
    let s = db.store();
    let fn_hash = s.put_descriptor(&fx_rate_descriptor()).unwrap();
    let r1 = stored_number_at("1.0892", TX1);
    let r2 = stored_number_at("1.0893", TX2);
    let h1 = s.put_receipt(&r1).unwrap();
    let h2 = s.put_receipt(&r2).unwrap();
    let cell = Receipt::from_json(&r1).unwrap().cell().id();
    s.put_version(&cell, TX1, &h1).unwrap();
    s.put_version(&cell, TX2, &h2).unwrap();
    s.bind_name("rates.example.org/fx_rate", &fn_hash, TX1).unwrap();
    (s, cell, h1, h2)
}

/// The server's message of a database error.
fn db_message(e: &datumesh_store::postgres::Error) -> String {
    e.as_db_error().map(|d| d.message().to_string()).unwrap_or_else(|| e.to_string())
}

fn sql_fails(db: &TestDb, sql: &str) -> String {
    let mut c = db.client();
    let e = c.batch_execute(sql).expect_err(sql);
    let msg = db_message(&e);
    assert!(msg.contains("append-only"), "{sql}: {msg}");
    msg
}

#[test]
fn update_delete_truncate_fail_in_the_database() {
    let db = TestDb::migrated();
    let (_s, _cell, _h1, h2) = seeded(&db);
    for table in ["versions", "name_bindings"] {
        sql_fails(&db, &format!("UPDATE {table} SET tx = tx"));
        sql_fails(&db, &format!("DELETE FROM {table}"));
        sql_fails(&db, &format!("TRUNCATE {table}"));
        sql_fails(&db, &format!("TRUNCATE {table} CASCADE"));
    }
    sql_fails(&db, &format!("UPDATE versions SET receipt_hash = '{h2}'"));
    // Truncating the tables they reference cannot remove history either.
    let mut c = db.client();
    assert!(c.batch_execute("TRUNCATE receipts CASCADE").is_err());
    assert!(c.batch_execute("TRUNCATE descriptors CASCADE").is_err());
    // Nothing changed.
    let n: i64 = c.query_one("SELECT count(*) FROM versions", &[]).unwrap().get(0);
    assert_eq!(n, 2);
    let n: i64 = c.query_one("SELECT count(*) FROM name_bindings", &[]).unwrap().get(0);
    assert_eq!(n, 1);
}

#[test]
fn raw_inserts_out_of_order_fail_in_the_database() {
    let db = TestDb::migrated();
    let (_s, cell, h1, _h2) = seeded(&db);
    let mut c = db.client();
    let e = c
        .execute(
            "INSERT INTO versions (cell_id, tx, tx_at, receipt_hash) VALUES ($1, $2, $2::text::timestamptz, $3)",
            &[&cell, &"2024-03-17T00:00:00.000000Z", &h1],
        )
        .unwrap_err();
    assert!(db_message(&e).contains("not later"), "{e:?}");
}

#[test]
fn put_version_not_later_is_a_typed_error() {
    let db = TestDb::migrated();
    let (s, cell, h1, h2) = seeded(&db);
    // Equal to the latest, and earlier than it.
    for (tx, h) in [(TX2, &h2), (TX1, &h1)] {
        match s.put_version(&cell, tx, h) {
            Err(Error::NotLater { given, latest, .. }) => {
                assert_eq!(given, tx);
                assert_eq!(latest, TX2);
            }
            other => panic!("{other:?}"),
        }
    }
    // An earlier, never-seen tx with its own receipt: still refused.
    let r0 = stored_number_at("1.08", "2024-03-14T16:00:00.000000Z");
    let h0 = s.put_receipt(&r0).unwrap();
    assert!(matches!(s.put_version(&cell, "2024-03-14T16:00:00.000000Z", &h0), Err(Error::NotLater { .. })));
    // A later one is accepted.
    let r3 = stored_number_at("1.0894", "2024-03-19T10:00:00.000000Z");
    let h3 = s.put_receipt(&r3).unwrap();
    s.put_version(&cell, "2024-03-19T10:00:00.000000Z", &h3).unwrap();
    assert_eq!(s.latest(&cell).unwrap().unwrap().tx, "2024-03-19T10:00:00.000000Z");
}

#[test]
fn bind_name_not_later_is_a_typed_error() {
    let db = TestDb::migrated();
    let (s, _cell, _h1, _h2) = seeded(&db);
    let fn_hash = datumesh_core::function_hash(&fx_rate_descriptor()).unwrap();
    for tx in [TX1, "2024-01-01T00:00:00.000000Z"] {
        assert!(matches!(s.bind_name("rates.example.org/fx_rate", &fn_hash, tx), Err(Error::NotLater { .. })));
    }
    s.bind_name("rates.example.org/fx_rate", &fn_hash, TX2).unwrap();
}

/// Two raw transactions racing: the second insert waits for the first to commit (the
/// trigger takes a per-cell lock), then sees its row. Out of order it fails; in order it
/// succeeds. Without the lock both would see an empty history and both would commit.
#[test]
fn concurrent_raw_inserts_are_serialised_per_cell() {
    for (first, second, second_ok) in [(TX2, TX1, false), (TX1, TX2, true)] {
        let db = TestDb::migrated();
        let s = db.store();
        let r1 = stored_number_at("1.0892", TX1);
        let r2 = stored_number_at("1.0893", TX2);
        let h1 = s.put_receipt(&r1).unwrap();
        let h2 = s.put_receipt(&r2).unwrap();
        let cell = Receipt::from_json(&r1).unwrap().cell().id();
        let hash_of = |tx: &str| if tx == TX1 { h1.clone() } else { h2.clone() };
        let insert =
            "INSERT INTO versions (cell_id, tx, tx_at, receipt_hash) VALUES ($1, $2, $2::text::timestamptz, $3)";

        let mut a = db.client();
        let mut ta = a.transaction().unwrap();
        ta.execute(insert, &[&cell, &first, &hash_of(first)]).unwrap();

        let config = db.config.clone();
        let (cell2, h_second) = (cell.clone(), hash_of(second));
        let second_s = second.to_string();
        let b = thread::spawn(move || {
            let mut b = config.connect(datumesh_store::postgres::NoTls).unwrap();
            let mut tb = b.transaction().unwrap();
            let r = tb.execute(insert, &[&cell2, &second_s, &h_second]);
            let ok = r.is_ok();
            if ok {
                tb.commit().unwrap();
            }
            (ok, r.err().map(|e| db_message(&e)))
        });
        thread::sleep(Duration::from_millis(500));
        assert!(!b.is_finished(), "the second insert did not wait for the first transaction");
        ta.commit().unwrap();
        let (ok, err) = b.join().unwrap();
        assert_eq!(ok, second_ok, "{first} then {second}: {err:?}");
        let n = s.versions(&cell, "0001-01-01T00:00:00.000000Z", "9999-12-31T23:59:59.999999Z").unwrap().len();
        assert_eq!(n, if second_ok { 2 } else { 1 });
    }
}

#[test]
fn concurrent_raw_name_bindings_are_serialised_per_name() {
    let db = TestDb::migrated();
    let s = db.store();
    let fn_hash = s.put_descriptor(&fx_rate_descriptor()).unwrap();
    let insert = "INSERT INTO name_bindings (fn_name, tx, tx_at, fn_hash) VALUES ($1, $2, $2::text::timestamptz, $3)";
    let mut a = db.client();
    let mut ta = a.transaction().unwrap();
    ta.execute(insert, &[&"rates.example.org/fx_rate", &TX2, &fn_hash]).unwrap();
    let config = db.config.clone();
    let h = fn_hash.clone();
    let b = thread::spawn(move || {
        let mut b = config.connect(datumesh_store::postgres::NoTls).unwrap();
        b.execute(insert, &[&"rates.example.org/fx_rate", &TX1, &h]).map_err(|e| db_message(&e))
    });
    thread::sleep(Duration::from_millis(500));
    assert!(!b.is_finished());
    ta.commit().unwrap();
    let r = b.join().unwrap();
    assert!(r.unwrap_err().contains("not later"));
}

/// Many threads append to one cell through the API, each with its own increasing
/// sequence of tx values interleaved with the others'. Every success is later than every
/// version committed before it, so the history read back is strictly increasing and holds
/// exactly the successes; every failure is NotLater.
#[test]
fn concurrent_put_version_keeps_tx_order() {
    let db = TestDb::migrated();
    let s = Arc::new(db.store_with(datumesh_store::PoolOptions::new().max_size(16)));
    let threads = 8;
    let per_thread = 25;
    // Receipts for minute offsets 0..threads*per_thread on 2024-03-15.
    let mut receipts = Vec::new();
    for i in 0..threads * per_thread {
        let tx = format!("2024-03-15T{:02}:{:02}:00.000000Z", i / 60, i % 60);
        let bytes = stored_number_at(&format!("{}", 1000 + i), &tx);
        let h = s.put_receipt(&bytes).unwrap();
        receipts.push((tx, h));
    }
    let cell = Receipt::from_json(&stored_number_at("1", "2024-03-15T00:00:00.000000Z")).unwrap().cell().id();
    let receipts = Arc::new(receipts);
    let barrier = Arc::new(Barrier::new(threads));
    let mut handles = Vec::new();
    for t in 0..threads {
        let (s, receipts, barrier, cell) = (s.clone(), receipts.clone(), barrier.clone(), cell.clone());
        handles.push(thread::spawn(move || {
            barrier.wait();
            let mut ok = Vec::new();
            // Thread t takes offsets t, t + threads, t + 2*threads, ...
            for k in 0..per_thread {
                let (tx, h) = &receipts[t + k * threads];
                match s.put_version(&cell, tx, h) {
                    Ok(()) => ok.push(tx.clone()),
                    Err(Error::NotLater { .. }) => {}
                    Err(e) => panic!("{e}"),
                }
            }
            ok
        }));
    }
    let mut successes: Vec<String> = handles.into_iter().flat_map(|h| h.join().unwrap()).collect();
    successes.sort();
    let history: Vec<String> = s
        .versions(&cell, "0001-01-01T00:00:00.000000Z", "9999-12-31T23:59:59.999999Z")
        .unwrap()
        .into_iter()
        .map(|v| v.tx)
        .collect();
    assert_eq!(history, successes);
    assert!(history.windows(2).all(|w| w[0] < w[1]));
    assert!(!history.is_empty());
}
