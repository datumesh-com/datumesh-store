//! Store 0.6.5, the fixes from the second review of Part G (CONTRACT-guard-patch.md, "Decided 27 Nov:
//! fixes from the second review"): A1, B2/B3 and B6. Failing tests: not written yet.
//!
//! API assumed (named by the decision):
//!
//! ```text
//! fn later_bound(&self, cell_id: &str, recorded: &str) -> Result<Option<String>>;
//!     // the minimum of max(asof, recorded) over the cell's versions recorded after `recorded`
//! impl PoolOptions { pub fn barrier_timeout(self, d: Duration) -> Self; }   // default 5 s
//! fn slice_member_hashes_within(&self, slice: &Slice, asof: &str, limit: usize, timeout: Duration)
//!     -> Result<Vec<String>>;   // the rebuild listing under a statement timeout
//! ```

#[macro_use]
mod common;

use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use common::*;
use datumesh_core::{Cell, Type};
use datumesh_store::{CellWrite, PoolOptions, SqliteOptions, SqliteStore};

const T1: &str = "2024-01-01T00:00:00.000000Z";
const T2: &str = "2024-01-02T00:00:00.000000Z";
const T3: &str = "2024-01-03T00:00:00.000000Z";
const T4: &str = "2024-01-04T00:00:00.000000Z";
const T5: &str = "2024-01-05T00:00:00.000000Z";
const LOCK_KEY: i64 = 7412030007;

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

fn rec_store(db: &TestDb, clock: &Clock) -> TStore {
    let s = match db.backend {
        Backend::Pg => {
            TStore(Arc::new(db.pg().bare_store_with(PoolOptions::new().max_size(4).recording_clock(clock.f()))))
        }
        Backend::Sqlite => TStore(Arc::new(
            SqliteStore::open_with(&db.lite().path, SqliteOptions::new().max_size(4).recording_clock(clock.f()))
                .unwrap(),
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

// ---- A1: later_bound ----------------------------------------------------------------------------

fn later_bound_is_the_least_start_of_the_versions_recorded_later(b: Backend) {
    // v1 signed and recorded T1; v2 signed T2 with asof T5 (late-signed); v3 signed and recorded T3.
    let db = TestDb::migrated(b);
    let s = slice_store(&db);
    let h = function(&s);
    let c = kcell(&h, 1);
    put_member(&s, &c, "1", T1, T1);
    put_member(&s, &c, "2", T2, T5);
    put_member(&s, &c, "3", T3, T3);
    // After v1: v2 starts at max(T5, T2) = T5, v3 at T3: the least is T3 (not the next version's T5).
    assert_eq!(s.later_bound(&c.id(), T1).unwrap().as_deref(), Some(T3));
    assert_eq!(s.later_bound(&c.id(), T2).unwrap().as_deref(), Some(T3));
    assert_eq!(s.later_bound(&c.id(), T3).unwrap(), None, "nothing recorded after the last");
    // With a recording clock: a version recorded later than it was signed starts at its recorded time.
    let db = TestDb::migrated(b);
    let clock = Clock::at(T1);
    let s = rec_store(&db, &clock);
    let h = function(&s);
    let c = kcell(&h, 1);
    put_member(&s, &c, "1", T1, T1);
    clock.set(T4);
    put_member(&s, &c, "2", T2, T2);
    assert_eq!(s.later_bound(&c.id(), T1).unwrap().as_deref(), Some(T4), "max(asof T2, recorded T4)");
    assert!(matches!(s.later_bound("nope", T1), Err(datumesh_store::Error::Invalid(_))));
}

// ---- B2/B3: the barrier, one statement with a timeout (PostgreSQL) -------------------------------

/// Holds the version lock from a raw session until the returned sender is used or dropped.
fn hold(db: &TestDb) -> (std::sync::mpsc::Sender<()>, std::thread::JoinHandle<()>) {
    let (held_tx, held_rx) = std::sync::mpsc::channel::<()>();
    let (release_tx, release_rx) = std::sync::mpsc::channel::<()>();
    let mut c = db.pg().client();
    let h = std::thread::spawn(move || {
        let mut t = c.transaction().unwrap();
        t.execute("SELECT pg_advisory_xact_lock($1)", &[&LOCK_KEY]).unwrap();
        held_tx.send(()).unwrap();
        let _ = release_rx.recv();
        t.commit().unwrap();
    });
    held_rx.recv().unwrap();
    (release_tx, h)
}

#[test]
fn pg_the_barrier_leaks_no_session_lock() {
    let db = TestDb::migrated(Backend::Pg);
    let s = slice_store(&db);
    for _ in 0..3 {
        s.barrier().unwrap();
    }
    // No advisory lock on the key is left behind by the store's sessions.
    let held =
        db.int(&format!("SELECT count(*) FROM pg_locks WHERE locktype = 'advisory' AND objid = {}", LOCK_KEY as u32));
    assert_eq!(held, 0, "the barrier is released at the end of its statement");
    // A writer on another connection takes the lock at once.
    let mut c = db.pg().client();
    let mut t = c.transaction().unwrap();
    t.execute("SET LOCAL lock_timeout = '1s'", &[]).unwrap();
    t.execute("SELECT pg_advisory_xact_lock($1)", &[&LOCK_KEY]).expect("not blocked by a leaked barrier");
    t.commit().unwrap();
}

#[test]
fn pg_the_barrier_honours_its_timeout() {
    let db = TestDb::migrated(Backend::Pg);
    let s = TStore(Arc::new(
        db.pg().bare_store_with(PoolOptions::new().max_size(4).barrier_timeout(Duration::from_millis(200))),
    ));
    let (release, holder) = hold(&db);
    let t0 = Instant::now();
    let r = s.barrier();
    let took = t0.elapsed();
    release.send(()).unwrap();
    holder.join().unwrap();
    assert!(r.is_err(), "a writer held past the timeout: the barrier fails, {r:?}");
    assert!(took < Duration::from_secs(3), "it gave up near its timeout: {took:?}");
    // With the writer gone the barrier passes.
    s.barrier().unwrap();
}

// ---- B6: the rebuild listing under a statement timeout -----------------------------------------

#[test]
fn pg_slice_member_hashes_within_errors_when_its_timeout_passes() {
    let db = TestDb::migrated(Backend::Pg);
    let s = slice_store(&db);
    let h = function(&s);
    for chunk in (0..20_000u32).collect::<Vec<_>>().chunks(500) {
        let built: Vec<(String, Vec<u8>)> =
            chunk.iter().map(|&k| (kcell(&h, k).id(), slice_receipt(&kcell(&h, k), "1", T1, T1))).collect();
        let items: Vec<CellWrite> = built.iter().map(|(id, r)| CellWrite::new(r, Some((id.as_str(), T1)))).collect();
        s.put_cells(&items).unwrap();
    }
    let sl = slice(&format!(r#"{{"args":{{"k":"*"}},"fn":"f","fn_hash":"{h}","ns":"{SLICE_HOST}"}}"#));
    let ok = s.slice_member_hashes_within(&sl, T2, 30_000, Duration::from_secs(60)).unwrap();
    assert_eq!(ok.len(), 20_000);
    let r = s.slice_member_hashes_within(&sl, T2, 30_000, Duration::from_millis(1));
    assert!(
        r.is_err(),
        "a 1 ms statement timeout over 20,000 members: refused, {} hashes",
        r.map(|v| v.len()).unwrap_or(0)
    );
}

backends!(later_bound_is_the_least_start_of_the_versions_recorded_later);
