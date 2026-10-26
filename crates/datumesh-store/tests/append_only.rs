//! Append-only history: `versions` and `name_bindings` refuse UPDATE, DELETE and TRUNCATE in
//! the database itself (both backends; SQLite also a replacing insert), `put_version` / `bind_name` refuse a tx not later than the latest
//! with a typed error, and the tx order holds under concurrent writers, both through the
//! API and for raw SQL inserts racing each other.

#[macro_use]
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
fn seeded(db: &TestDb) -> (TStore, String, String, String) {
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

/// UPDATE and DELETE fail in the database (and TRUNCATE on PostgreSQL; SQLite has no
/// TRUNCATE, its `DELETE FROM` without WHERE is the same statement as any DELETE and fires
/// the triggers). SQLite also refuses `INSERT OR REPLACE` / `REPLACE` of an existing row,
/// which would otherwise delete it without a DELETE trigger firing.
fn update_delete_truncate_fail_in_the_database(b: Backend) {
    let db = TestDb::migrated(b);
    let (_s, cell, h1, h2) = seeded(&db);
    for table in ["versions", "name_bindings"] {
        db.refused(&format!("UPDATE {table} SET tx = tx"));
        db.refused(&format!("DELETE FROM {table}"));
        db.refused(&format!("DELETE FROM {table} WHERE tx = '{TX1}'"));
        if db.is_pg() {
            db.refused(&format!("TRUNCATE {table}"));
            db.refused(&format!("TRUNCATE {table} CASCADE"));
        }
    }
    db.refused(&format!("UPDATE versions SET receipt_hash = '{h2}'"));
    if db.is_pg() {
        // Truncating the tables they reference cannot remove history either.
        assert!(db.exec("TRUNCATE receipts CASCADE").is_err());
        assert!(db.exec("TRUNCATE descriptors CASCADE").is_err());
    } else {
        // Nor deleting them (foreign keys are enforced only on connections that ask, so
        // the tables they reference refuse deletes themselves).
        db.refused("DELETE FROM receipts");
        db.refused("DELETE FROM descriptors");
        db.refused("UPDATE receipts SET bytes = bytes");
        // A replacing insert of an existing row would delete it first.
        for sql in [
            format!("INSERT OR REPLACE INTO versions (cell_id, tx, receipt_hash) VALUES ('{cell}', '{TX1}', '{h1}')"),
            format!("REPLACE INTO versions (cell_id, tx, receipt_hash) VALUES ('{cell}', '{TX2}', '{h2}')"),
            "INSERT OR REPLACE INTO name_bindings SELECT * FROM name_bindings".to_string(),
            "INSERT OR REPLACE INTO receipts SELECT * FROM receipts".to_string(),
            "INSERT OR REPLACE INTO descriptors SELECT * FROM descriptors".to_string(),
        ] {
            let msg = db.exec(&sql).expect_err(&sql);
            assert!(msg.contains("append-only") || msg.contains("not later"), "{sql}: {msg}");
        }
    }
    // Nothing changed.
    assert_eq!(db.count("versions"), 2);
    assert_eq!(db.count("name_bindings"), 1);
    assert_eq!(db.texts("SELECT receipt_hash FROM versions ORDER BY tx"), [[h1], [h2]]);
}

fn raw_inserts_out_of_order_fail_in_the_database(b: Backend) {
    let db = TestDb::migrated(b);
    let (_s, cell, h1, _h2) = seeded(&db);
    let e = db.raw_version(&cell, "2024-03-17T00:00:00.000000Z", &h1).unwrap_err();
    assert!(e.contains("not later"), "{e}");
    // Equal to the latest is not later either.
    let e = db.raw_version(&cell, TX2, &h1).unwrap_err();
    assert!(e.contains("not later"), "{e}");
    assert_eq!(db.count("versions"), 2);
}

fn put_version_not_later_is_a_typed_error(b: Backend) {
    let db = TestDb::migrated(b);
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

fn bind_name_not_later_is_a_typed_error(b: Backend) {
    let db = TestDb::migrated(b);
    let (s, _cell, _h1, _h2) = seeded(&db);
    let fn_hash = datumesh_core::function_hash(&fx_rate_descriptor()).unwrap();
    for tx in [TX1, "2024-01-01T00:00:00.000000Z"] {
        assert!(matches!(s.bind_name("rates.example.org/fx_rate", &fn_hash, tx), Err(Error::NotLater { .. })));
    }
    s.bind_name("rates.example.org/fx_rate", &fn_hash, TX2).unwrap();
}

/// A raw connection holding a transaction open after one insert: `commit` ends it.
enum Open {
    Pg(datumesh_store::pg_unstable::postgres::Client),
    Lite(rusqlite::Connection),
}

impl Open {
    /// Begins a transaction (SQLite: `BEGIN IMMEDIATE`, the write lock) and runs `insert`.
    fn begin(db: &TestDb, insert: &str) -> Open {
        if db.is_pg() {
            let mut c = db.pg().client();
            c.batch_execute(&format!("BEGIN; {insert}")).unwrap();
            Open::Pg(c)
        } else {
            let c = db.lite().conn();
            c.execute_batch(&format!("BEGIN IMMEDIATE; {insert}")).unwrap();
            Open::Lite(c)
        }
    }

    fn commit(self) {
        match self {
            Open::Pg(mut c) => c.batch_execute("COMMIT").unwrap(),
            Open::Lite(c) => c.execute_batch("COMMIT").unwrap(),
        }
    }
}

/// Runs `sql` on a new raw connection, in a thread: its result (the database's message).
fn racing(db: &TestDb, sql: String) -> thread::JoinHandle<Result<(), String>> {
    if db.is_pg() {
        let config = db.pg().config.clone();
        thread::spawn(move || {
            let mut c = config.connect(datumesh_store::pg_unstable::postgres::NoTls).unwrap();
            c.batch_execute(&format!("BEGIN; {sql}; COMMIT")).map_err(|e| pg_message(&e))
        })
    } else {
        let path = db.lite().path.clone();
        thread::spawn(move || {
            let c = rusqlite::Connection::open(path).unwrap();
            c.busy_timeout(Duration::from_secs(30)).unwrap();
            c.execute_batch(&sql).map_err(|e| e.to_string())
        })
    }
}

/// Two raw transactions racing: the second insert waits for the first to commit
/// (PostgreSQL: the trigger takes a per-cell lock; SQLite: one writer at a time, and the
/// waiting insert then reads the committed row), then sees its row. Out of order it fails;
/// in order it succeeds. Without the wait both would see an empty history and both commit.
fn concurrent_raw_inserts_are_serialised_per_cell(b: Backend) {
    for (first, second, second_ok) in [(TX2, TX1, false), (TX1, TX2, true)] {
        let db = TestDb::migrated(b);
        let s = db.store();
        let r1 = stored_number_at("1.0892", TX1);
        let r2 = stored_number_at("1.0893", TX2);
        let h1 = s.put_receipt(&r1).unwrap();
        let h2 = s.put_receipt(&r2).unwrap();
        let cell = Receipt::from_json(&r1).unwrap().cell().id();
        let hash_of = |tx: &str| if tx == TX1 { h1.clone() } else { h2.clone() };
        let insert = |tx: &str| {
            let cols = db.pick("cell_id, tx, tx_at, receipt_hash", "cell_id, tx, receipt_hash");
            let at = if db.is_pg() { format!("'{tx}'::timestamptz, ") } else { String::new() };
            format!("INSERT INTO versions ({cols}) VALUES ('{cell}', '{tx}', {at}'{}')", hash_of(tx))
        };
        let a = Open::begin(&db, &insert(first));
        let racer = racing(&db, insert(second));
        thread::sleep(Duration::from_millis(500));
        assert!(!racer.is_finished(), "the second insert did not wait for the first transaction");
        a.commit();
        let r = racer.join().unwrap();
        assert_eq!(r.is_ok(), second_ok, "{first} then {second}: {r:?}");
        if let Err(e) = r {
            assert!(e.contains("not later"), "{e}");
        }
        let n = s.versions(&cell, "0001-01-01T00:00:00.000000Z", "9999-12-31T23:59:59.999999Z").unwrap().len();
        assert_eq!(n, if second_ok { 2 } else { 1 });
    }
}

fn concurrent_raw_name_bindings_are_serialised_per_name(b: Backend) {
    let db = TestDb::migrated(b);
    let s = db.store();
    let fn_hash = s.put_descriptor(&fx_rate_descriptor()).unwrap();
    let insert = |tx: &str| {
        let cols = db.pick("fn_name, tx, tx_at, fn_hash", "fn_name, tx, fn_hash");
        let at = if db.is_pg() { format!("'{tx}'::timestamptz, ") } else { String::new() };
        format!("INSERT INTO name_bindings ({cols}) VALUES ('rates.example.org/fx_rate', '{tx}', {at}'{fn_hash}')")
    };
    let a = Open::begin(&db, &insert(TX2));
    let racer = racing(&db, insert(TX1));
    thread::sleep(Duration::from_millis(500));
    assert!(!racer.is_finished());
    a.commit();
    let r = racer.join().unwrap();
    assert!(r.unwrap_err().contains("not later"));
}

/// Many threads append to one cell through the API, each with its own increasing
/// sequence of tx values interleaved with the others'. Every success is later than every
/// version committed before it, so the history read back is strictly increasing and holds
/// exactly the successes; every failure is NotLater.
fn concurrent_put_version_keeps_tx_order(b: Backend) {
    let db = TestDb::migrated(b);
    let s = db.store_sized(16);
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

backends!(
    update_delete_truncate_fail_in_the_database,
    raw_inserts_out_of_order_fail_in_the_database,
    put_version_not_later_is_a_typed_error,
    bind_name_not_later_is_a_typed_error,
    concurrent_raw_inserts_are_serialised_per_cell,
    concurrent_raw_name_bindings_are_serialised_per_name,
    concurrent_put_version_keeps_tx_order,
);
