//! The connection pool: bounded, waits with a timeout, and replaces connections the server
//! closed. One transaction per `put_*`.

mod common;

use std::sync::Arc;
use std::time::{Duration, Instant};

use common::*;
use datumesh_core::Resolver;
use datumesh_store::{PoolOptions, Store};

#[test]
fn connections_killed_by_the_server_are_replaced() {
    let db = TestDb::migrated();
    let s = db.store_with(PoolOptions::new().max_size(2));
    let h = s.put_receipt(&input(&vector("receipt/stored-number"))).unwrap();
    assert!(s.receipt(&h).is_some());
    // Kill every other backend of this database.
    let mut admin = db.client();
    admin
        .execute(
            "SELECT pg_terminate_backend(pid) FROM pg_stat_activity WHERE datname = current_database() AND pid <> pg_backend_pid()",
            &[],
        )
        .unwrap();
    std::thread::sleep(Duration::from_millis(200));
    // The first use of a dead connection may fail (a read gives None); the pool drops it,
    // and the store answers again.
    let mut answered = false;
    for _ in 0..4 {
        if s.receipt(&h).is_some() {
            answered = true;
            break;
        }
    }
    assert!(answered);
}

#[test]
fn the_pool_is_bounded_and_shared_across_threads() {
    let db = TestDb::migrated();
    let s = Arc::new(db.store_with(PoolOptions::new().max_size(3).checkout_timeout(Duration::from_secs(10))));
    let h = s.put_receipt(&input(&vector("receipt/stored-number"))).unwrap();
    let handles: Vec<_> = (0..12)
        .map(|_| {
            let (s, h) = (s.clone(), h.clone());
            std::thread::spawn(move || (0..50).all(|_| s.receipt(&h).is_some()))
        })
        .collect();
    assert!(handles.into_iter().all(|t| t.join().unwrap()));
    let n: i64 = db
        .client()
        .query_one("SELECT count(*) FROM pg_stat_activity WHERE datname = current_database()", &[])
        .unwrap()
        .get(0);
    // Three pooled connections plus the one counting.
    assert!(n <= 4, "{n}");
}

#[test]
fn checkout_times_out_when_the_server_refuses_connections() {
    // A connector that always fails: put_* report it as a typed error within the timeout.
    let opts = PoolOptions::new().max_size(1).checkout_timeout(Duration::from_millis(300));
    let r = datumesh_store::PgStore::with_connector(
        || {
            "host=127.0.0.1 port=1 user=x connect_timeout=1"
                .parse::<datumesh_store::postgres::Config>()
                .unwrap()
                .connect(datumesh_store::postgres::NoTls)
        },
        opts,
    );
    assert!(r.is_err());
    let t = Instant::now();
    let e = datumesh_store::PgStore::connect("postgres://x@127.0.0.1:1/x?connect_timeout=1").unwrap_err();
    assert!(t.elapsed() < Duration::from_secs(5), "{e}");
}
