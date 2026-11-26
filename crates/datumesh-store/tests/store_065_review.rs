//! Store 0.6.5, the fixes from the independent review of Part G (CONTRACT-guard-patch.md, "Decided
//! 26 Nov: fixes from the independent review of Part G"), items 2, 3 and 7. Both backends unless
//! named. Failing tests: the fixes are not written yet.
//!
//! API assumed: `fn barrier(&self) -> Result<()>` on `Store`: waits for the writers of versions in
//! flight (PostgreSQL: `pg_advisory_lock_shared(7412030007)` taken and released at once; SQLite: an
//! in-process equivalent) and returns at once with none.
//!
//! Item 2: `recorded` is assigned inside the writing transaction, after the version lock is held
//! (PostgreSQL `pg_advisory_xact_lock(7412030007)`, SQLite `BEGIN IMMEDIATE`): a writer that waited
//! records the clock as it reads once it holds the lock. Item 3: a version that exists with the same
//! (cell_id, tx) and the same receipt is a no-op; with another receipt a clean NotLater or Conflict,
//! never a database key error. Item 7: on SQLite a raw insert with no seq gets one from a trigger,
//! which updates function_seqs.

#[macro_use]
mod common;

use std::sync::atomic::{AtomicBool, AtomicI64, Ordering};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use common::*;
use datumesh_core::{Cell, Type};
use datumesh_store::{CellWrite, Error, PoolOptions, SqliteOptions, SqliteStore};

const T1: &str = "2024-01-01T00:00:00.000000Z";
const T2: &str = "2024-01-02T00:00:00.000000Z";
const T3: &str = "2024-01-03T00:00:00.000000Z";
const T4: &str = "2024-01-04T00:00:00.000000Z";
const LOCK_KEY: i64 = 7412030007;

/// A clock the test sets.
#[derive(Clone)]
struct Clock(Arc<Mutex<String>>);

impl Clock {
    fn at(t: &str) -> Clock {
        Clock(Arc::new(Mutex::new(t.to_string())))
    }
    fn set(&self, t: &str) {
        *self.0.lock().unwrap() = t.to_string();
    }
    fn f(&self) -> datumesh_store::RecordingClock {
        let c = self.0.clone();
        Arc::new(move || c.lock().unwrap().clone())
    }
}

fn rec_store_with(db: &TestDb, f: datumesh_store::RecordingClock) -> TStore {
    let s = match db.backend {
        Backend::Pg => TStore(Arc::new(db.pg().bare_store_with(PoolOptions::new().max_size(8).recording_clock(f)))),
        Backend::Sqlite => TStore(Arc::new(
            SqliteStore::open_with(&db.lite().path, SqliteOptions::new().max_size(8).recording_clock(f)).unwrap(),
        )),
    };
    s.put_namespace(SLICE_HOST, &slice_keydoc(), VALIDATED_AT).unwrap();
    s
}

fn function(s: &TStore) -> String {
    s.put_descriptor(&stored_descriptor(&[("k", Type::Number)], "f")).unwrap()
}

fn kcell(h: &str, k: u32) -> Cell {
    slice_cell("f", h, &[("k", sc(Type::Number, &k.to_string()))])
}

/// Holds the version lock from a raw session (PostgreSQL) or the write lock (SQLite) until the
/// returned sender is dropped or sent to; returns once it is held.
fn hold_writes(db: &TestDb) -> (std::sync::mpsc::Sender<()>, std::thread::JoinHandle<()>) {
    let (held_tx, held_rx) = std::sync::mpsc::channel::<()>();
    let (release_tx, release_rx) = std::sync::mpsc::channel::<()>();
    let h = match db.backend {
        Backend::Pg => {
            let mut c = db.pg().client();
            std::thread::spawn(move || {
                let mut t = c.transaction().unwrap();
                t.execute("SELECT pg_advisory_xact_lock($1)", &[&LOCK_KEY]).unwrap();
                held_tx.send(()).unwrap();
                let _ = release_rx.recv();
                t.commit().unwrap();
            })
        }
        Backend::Sqlite => {
            let c = db.lite().conn();
            std::thread::spawn(move || {
                c.execute_batch("BEGIN IMMEDIATE").unwrap();
                held_tx.send(()).unwrap();
                let _ = release_rx.recv();
                c.execute_batch("COMMIT").unwrap();
            })
        }
    };
    held_rx.recv().unwrap();
    (release_tx, h)
}

// ---- Item 2: recorded follows visibility ------------------------------------------------------

fn a_writer_that_waited_records_the_time_it_holds_the_lock_at(b: Backend) {
    let db = TestDb::migrated(b);
    let clock = Clock::at(T2);
    let s = rec_store_with(&db, clock.f());
    let h = function(&s);
    let (release, holder) = hold_writes(&db);
    let c = kcell(&h, 1);
    let writer = {
        let s = s.clone();
        let c = c.clone();
        std::thread::spawn(move || put_member(&s, &c, "1", T1, T1))
    };
    std::thread::sleep(Duration::from_millis(400));
    assert!(!writer.is_finished(), "the writer waits for the lock");
    // The clock moves on while the writer waits; the holder commits.
    clock.set(T4);
    release.send(()).unwrap();
    holder.join().unwrap();
    writer.join().unwrap();
    let v = s.latest(&c.id()).unwrap().unwrap();
    assert_eq!(v.recorded, T4, "recorded under the lock, after the holder's commit, not when the call began");
    assert_eq!(v.tx, T1);
}

fn barrier_returns_at_once_with_no_writer_and_waits_for_one_in_flight(b: Backend) {
    let db = TestDb::migrated(b);
    let s = rec_store_with(&db, Clock::at(T2).f());
    let h = function(&s);
    let t0 = Instant::now();
    s.barrier().unwrap();
    assert!(t0.elapsed() < Duration::from_secs(2), "no writer: at once ({:?})", t0.elapsed());
    // A writer of this store in flight (held behind the lock): the barrier waits for it.
    let (release, holder) = hold_writes(&db);
    let writer = {
        let s = s.clone();
        let c = kcell(&h, 1);
        std::thread::spawn(move || put_member(&s, &c, "1", T1, T1))
    };
    std::thread::sleep(Duration::from_millis(200));
    let done = Arc::new(AtomicBool::new(false));
    let barrier = {
        let s = s.clone();
        let done = done.clone();
        std::thread::spawn(move || {
            s.barrier().unwrap();
            done.store(true, Ordering::SeqCst);
        })
    };
    std::thread::sleep(Duration::from_millis(400));
    assert!(!done.load(Ordering::SeqCst), "the barrier waits while a writer is in flight");
    release.send(()).unwrap();
    holder.join().unwrap();
    writer.join().unwrap();
    barrier.join().unwrap();
    assert!(done.load(Ordering::SeqCst));
}

fn concurrent_writers_to_one_cell_get_no_spurious_not_later(b: Backend) {
    // A ticking recording clock: each reading a microsecond later. Under the lock the writers record
    // in the order they commit, so none is refused as not later than the one before.
    let db = TestDb::migrated(b);
    let tick = Arc::new(AtomicI64::new(0));
    let f: datumesh_store::RecordingClock = {
        let tick = tick.clone();
        Arc::new(move || {
            let us = tick.fetch_add(1, Ordering::SeqCst);
            format!("2024-02-01T00:{:02}:{:02}.{:06}Z", us / 60_000_000, (us / 1_000_000) % 60, us % 1_000_000)
        })
    };
    let s = rec_store_with(&db, f);
    let h = function(&s);
    let c = kcell(&h, 1);
    let refused = std::thread::scope(|sc| {
        let hs: Vec<_> = (0..8u32)
            .map(|t| {
                let s = s.clone();
                let c = c.clone();
                sc.spawn(move || {
                    let mut refused = Vec::new();
                    for i in 0..25u32 {
                        // Signed times interleave across threads; each is its own receipt.
                        let tx = format!("2024-01-{:02}T{:02}:{:02}:00.000000Z", 1 + (i % 28), t, i);
                        let r = slice_receipt(&c, &(t * 100 + i).to_string(), &tx, &tx);
                        let hash = s.put_receipt(&r).unwrap();
                        if let Err(e) = s.put_version(&c.id(), &tx, &hash) {
                            refused.push(format!("{e}"));
                        }
                    }
                    refused
                })
            })
            .collect();
        hs.into_iter().flat_map(|h| h.join().unwrap()).collect::<Vec<_>>()
    });
    assert!(refused.is_empty(), "no writer refused: {refused:?}");
    assert_eq!(db.int("SELECT count(*) FROM versions"), 200);
}

// ---- Item 3: re-runs are no-ops ----------------------------------------------------------------

fn an_existing_version_again_is_a_no_op_and_another_receipt_a_clean_refusal(b: Backend) {
    for clocked in [false, true] {
        let db = TestDb::migrated(b);
        let clock = Clock::at(T3);
        let s = if clocked { rec_store_with(&db, clock.f()) } else { slice_store(&db) };
        let h = function(&s);
        let c = kcell(&h, 1);
        let first = put_member(&s, &c, "1", T1, T1);
        let before = (db.count("versions"), s.latest(&c.id()).unwrap().unwrap());
        // The same (cell, tx) and receipt again: nothing, no error; through put_cells too.
        clock.set(T4);
        s.put_version(&c.id(), T1, &first).unwrap_or_else(|e| panic!("clocked {clocked}: the same version again: {e}"));
        let r1 = slice_receipt(&c, "1", T1, T1);
        let id = c.id();
        s.put_cells(&[CellWrite::new(&r1, Some((id.as_str(), T1)))])
            .unwrap_or_else(|e| panic!("clocked {clocked}: put_cells of the same version again: {e}"));
        assert_eq!(
            (db.count("versions"), s.latest(&c.id()).unwrap().unwrap()),
            before,
            "clocked {clocked}: nothing written"
        );
        // Another receipt at the same (cell, tx): a clean refusal, not a database key error.
        let other = s.put_receipt(&slice_receipt(&c, "2", T1, T1)).unwrap();
        match s.put_version(&c.id(), T1, &other) {
            Err(Error::NotLater { .. }) | Err(Error::Conflict { .. }) => {}
            other => panic!("clocked {clocked}: another receipt at the same tx: {other:?}"),
        }
        let r2 = slice_receipt(&c, "2", T1, T1);
        match s.put_cells(&[CellWrite::new(&r2, Some((id.as_str(), T1)))]) {
            Err(Error::NotLater { .. }) | Err(Error::Conflict { .. }) => {}
            other => panic!("clocked {clocked}: put_cells, another receipt at the same tx: {other:?}"),
        }
        assert_eq!(db.count("versions"), before.0);
    }
}

// ---- Item 7: SQLite, a raw insert with no seq -------------------------------------------------

#[test]
fn sqlite_a_raw_insert_with_no_seq_gets_one_and_updates_function_seqs() {
    let db = TestDb::migrated(Backend::Sqlite);
    let s = slice_store(&db);
    let h = function(&s);
    put_member(&s, &kcell(&h, 1), "1", T1, T1);
    let before = s.fn_seq(&h).unwrap();
    assert!(before > 0);
    let c = kcell(&h, 2);
    let hash = s.put_receipt(&slice_receipt(&c, "2", T2, T2)).unwrap();
    db.exec(&format!(
        "INSERT INTO versions (cell_id, tx, receipt_hash, recorded) VALUES ('{}', '{T2}', '{hash}', '{T2}')",
        c.id()
    ))
    .unwrap();
    let seq = db.int(&format!("SELECT seq FROM versions WHERE cell_id = '{}'", c.id()));
    assert!(seq > before, "a seq from the trigger: {seq} after {before}");
    assert_eq!(db.int(&format!("SELECT seq FROM function_seqs WHERE fn_hash = '{h}'")), seq, "function_seqs updated");
    assert_eq!(s.fn_seq(&h).unwrap(), seq);
}

backends!(
    a_writer_that_waited_records_the_time_it_holds_the_lock_at,
    barrier_returns_at_once_with_no_writer_and_waits_for_one_in_flight,
    concurrent_writers_to_one_cell_get_no_spurious_not_later,
    an_existing_version_again_is_a_no_op_and_another_receipt_a_clean_refusal,
);
