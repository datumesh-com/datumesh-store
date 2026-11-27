//! Store 0.6.5, the guard patch G9 and G10 (CONTRACT-guard-patch.md, "Decided 26 Nov (user)"):
//! the recorded time of every version and withdrawal, set by the store at insert, and the slice
//! definitions kept once per distinct slice. Both backends. Failing tests: nothing of it is written.
//!
//! API assumed (the contract names the column, the migrations and the rule; the Rust shapes are the
//! test writer's choice):
//!
//! ```text
//! /// The clock a store records versions at (G9). The store sets
//! /// recorded = max(the receipt's signed tx, clock()) at insert; no caller of put_version or
//! /// put_cells supplies it. Without one (the default) recorded = the signed tx, as every row
//! /// before migration 17 has it: the tests' worlds, which simulate time with signed times, keep
//! /// their meaning, and the mesh binary sets the system clock.
//! pub type RecordingClock = std::sync::Arc<dyn Fn() -> String + Send + Sync>;
//! impl PoolOptions   { pub fn recording_clock(self, clock: RecordingClock) -> Self; }
//! impl SqliteOptions { pub fn recording_clock(self, clock: RecordingClock) -> Self; }
//! fn recording_time(&self, signed_tx: &str) -> String;  // what the next put records for that tx
//!                                                       // (decided 26 Nov, for publish-stored's check)
//! pub struct Version { ..., pub recorded: String }   // the version's recorded time
//!
//! // G10: a slice's definition, kept once per distinct slice (not per date).
//! fn put_slice_definition(&self, slice: &Slice) -> Result<bool>;          // Ok(true): inserted
//! fn slice_definition(&self, slice_id: &str) -> Result<Option<Slice>>;
//! ```
//!
//! Selection (G9): every as-of selection uses `recorded` (a version answers as of A when its
//! recorded time and its receipt's asof are at or before A): `stored_asof`, `version_asof`,
//! `slice_members`, `slice_values`, `versions_between` (its `[from, to]` range stays on the signed
//! tx, which a verifier checks, SPEC 15.13; only what answers as of A is by `recorded`),
//! `slice_changed_after`. The backdating rule of `put_version` stays on the signed tx (decided 27 Nov,
//! user: a version signed not later than the cell's latest signed tx is NotLater); `recorded` only
//! orders and strictly increases per cell.
//!
//! Migrations: PostgreSQL 17 (`0017_recorded.sql`), SQLite 13 (`0013_recorded.sql`), additive;
//! existing rows get recorded = tx. The slice definitions' table is `slice_definitions` (`slice_id`,
//! `fn_hash`, `slice`), append-only; in which of the guard's migrations it lands is the
//! implementer's choice.

#[macro_use]
mod common;

use std::sync::{Arc, Mutex};

use common::*;
use datumesh_core::{Slice, Type};
use datumesh_store::{migrate, CellWrite, PoolOptions, SqliteOptions, SqliteStore};

const T1: &str = "2024-01-01T00:00:00.000000Z";
const T2: &str = "2024-01-02T00:00:00.000000Z";
const T3: &str = "2024-01-03T00:00:00.000000Z";
const T4: &str = "2024-01-04T00:00:00.000000Z";
const T5: &str = "2024-01-05T00:00:00.000000Z";
const END: &str = "9999-12-31T23:59:59.999999Z";

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

/// A store of `db` recording at `clock`, holding SLICE_HOST's key document.
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

fn kcell(h: &str, k: u32) -> datumesh_core::Cell {
    slice_cell("f", h, &[("k", sc(Type::Number, &k.to_string()))])
}

fn slice_f(h: &str) -> Slice {
    slice(&format!(r#"{{"args":{{"k":"*"}},"fn":"f","fn_hash":"{h}","ns":"{SLICE_HOST}"}}"#))
}

fn ids(vs: &[datumesh_store::SliceValue]) -> Vec<String> {
    vs.iter().map(|v| v.version.cell_id.clone()).collect()
}

// ---- G9: the column and the migration --------------------------------------------------------

/// Migration 17 (SQLite 13) is the recorded-time migration, at its place; later releases add more after it (0.7.0 adds
/// the listings, `store_070_listings.rs`).
fn migration_17_and_13_are_the_last_and_named(b: Backend) {
    let ms = match b {
        Backend::Pg => migrate::MIGRATIONS,
        Backend::Sqlite => migrate::sqlite::MIGRATIONS,
    };
    let want = if b == Backend::Pg { (17, "0017_recorded.sql") } else { (13, "0013_recorded.sql") };
    let m = &ms[want.0 as usize - 1];
    assert_eq!((m.version, m.name), want);
    assert!(ms.len() as u32 >= want.0);
}

fn without_a_clock_recorded_is_the_signed_tx(b: Backend) {
    let db = TestDb::migrated(b);
    let s = slice_store(&db);
    let h = function(&s);
    put_member(&s, &kcell(&h, 1), "1", T1, T1);
    let v = s.latest(&kcell(&h, 1).id()).unwrap().unwrap();
    assert_eq!((v.tx.as_str(), v.recorded.as_str()), (T1, T1));
}

fn the_store_records_at_its_clock_and_keeps_the_signed_tx(b: Backend) {
    let db = TestDb::migrated(b);
    let clock = Clock::at(T4);
    let s = rec_store(&db, &clock);
    let h = function(&s);
    // A receipt signed at T1 (a backfill), recorded at T4.
    let hash = put_member(&s, &kcell(&h, 1), "1", T1, T1);
    let v = s.latest(&kcell(&h, 1).id()).unwrap().unwrap();
    assert_eq!(v.tx, T1, "the signed tx stays as signed");
    assert_eq!(v.recorded, T4, "recorded at the store's clock");
    assert_eq!(v.receipt_hash, hash);
    assert_eq!(datumesh_core::Receipt::from_json(&s.receipt(&hash).unwrap()).unwrap().tx(), T1, "no receipt changes");
    // put_cells records the same way.
    clock.set(T5);
    let c2 = kcell(&h, 2);
    let r2 = slice_receipt(&c2, "2", T2, T2);
    let id2 = c2.id();
    s.put_cells(&[CellWrite::new(&r2, Some((id2.as_str(), T2)))]).unwrap();
    let v2 = s.latest(&id2).unwrap().unwrap();
    assert_eq!((v2.tx.as_str(), v2.recorded.as_str()), (T2, T5));
    // A clock behind the signed tx: recorded is never before it (recorded = max(tx, clock)).
    clock.set(T1);
    put_member(&s, &kcell(&h, 3), "3", T3, T3);
    assert_eq!(s.latest(&kcell(&h, 3).id()).unwrap().unwrap().recorded, T3);
    // The column is the database's: every row has one, and it cannot be updated.
    assert_eq!(db.int("SELECT count(*) FROM versions WHERE recorded IS NULL"), 0);
    db.refused(&format!("UPDATE versions SET recorded = '{T5}'"));
}

fn recording_time_is_what_the_next_put_records(b: Backend) {
    // Store::recording_time(signed_tx) (decided 26 Nov, from the http implementation): the time the
    // store will record a version signed at `signed_tx` at, for publish-stored's own check.
    let db = TestDb::migrated(b);
    let plain = slice_store(&db);
    assert_eq!(plain.recording_time(T2), T2, "no clock: the signed tx");
    let clock = Clock::at(T3);
    let s = rec_store(&db, &clock);
    assert_eq!(s.recording_time(T1), T3, "max(signed tx, clock)");
    assert_eq!(s.recording_time(T4), T4, "a clock behind the signed tx: the signed tx");
    assert_eq!(s.recording_time(T3), T3);
    // It equals what the next put records for that tx.
    let h = function(&s);
    for (k, tx, at) in [(1, T1, T3), (2, T4, T3), (3, T2, T5)] {
        clock.set(at);
        let want = s.recording_time(tx);
        put_member(&s, &kcell(&h, k), "1", tx, tx);
        assert_eq!(s.latest(&kcell(&h, k).id()).unwrap().unwrap().recorded, want, "k {k}");
    }
    let c = kcell(&h, 9);
    let want = plain.recording_time(T1);
    put_member(&plain, &c, "1", T1, T1);
    assert_eq!(plain.latest(&c.id()).unwrap().unwrap().recorded, want);
}

fn as_of_reads_select_by_recorded(b: Backend) {
    let db = TestDb::migrated(b);
    let clock = Clock::at(T1);
    let s = rec_store(&db, &clock);
    let h = function(&s);
    let (c1, c2) = (kcell(&h, 1), kcell(&h, 2));
    put_member(&s, &c1, "1", T1, T1);
    // c2 signed at T1 (a backfill from elsewhere), recorded at T3.
    clock.set(T3);
    put_member(&s, &c2, "2", T1, T1);
    // stored_asof and version_asof: not before it was recorded.
    assert!(s.stored_asof(&c2.id(), T2).unwrap().is_none(), "not on this mesh as of T2");
    assert!(s.version_asof(&c2.id(), T2).unwrap().is_none());
    assert_eq!(s.stored_asof(&c2.id(), T3).unwrap().unwrap().recorded, T3);
    // Slices: a member from its recorded time on.
    let sl = slice_f(&h);
    assert_eq!(ids(&s.slice_values(&sl, T2).unwrap()), [c1.id()], "as of T2: c1 only");
    let mut both = vec![c1.id(), c2.id()];
    both.sort();
    let mut at3 = ids(&s.slice_values(&sl, T3).unwrap());
    at3.sort();
    assert_eq!(at3, both);
    assert_eq!(s.slice_members(&sl, T2).unwrap().len(), 1);
    assert_eq!(s.slice_members(&sl, T3).unwrap().len(), 2);
    // Whether the members may answer differently after T2: c2 was recorded after it.
    assert!(s.slice_changed_after(&sl, T2).unwrap());
    assert!(!s.slice_changed_after(&sl, T3).unwrap());
    // A version slice: the range on the signed tx, the answer as of A by recorded.
    assert!(s.versions_between(&c2.id(), T1, T1, T2).unwrap().is_empty(), "recorded after T2");
    assert_eq!(s.versions_between(&c2.id(), T1, T1, T3).unwrap().len(), 1, "signed at T1: in [T1, T1]");
    assert!(
        s.versions_between(&c2.id(), T3, T5, T5).unwrap().is_empty(),
        "the range is on the signed tx, not recorded"
    );
}

fn the_backdating_rule_is_enforced_on_recorded(b: Backend) {
    // Decided 27 Nov (user), "backdating on the same cell is refused": a version of an existing cell
    // signed not later than that cell's latest signed tx is NotLater, as before the patch, whatever its
    // recorded time (a replayed old receipt cannot roll the cell back). A version signed later is
    // accepted, and on a frozen clock recorded 1 µs after the latest (recorded strictly increases).
    let db = TestDb::migrated(b);
    let clock = Clock::at(T2);
    let s = rec_store(&db, &clock);
    let h = function(&s);
    let c = kcell(&h, 1);
    put_member(&s, &c, "10", T2, T2);
    // A correction signed earlier (T1) than the latest (T2), recorded later (T4): refused, nothing changes.
    clock.set(T4);
    let corr = s.put_receipt(&slice_receipt(&c, "11", T1, T1)).unwrap();
    match s.put_version(&c.id(), T1, &corr) {
        Err(datumesh_store::Error::NotLater { given, latest, .. }) => {
            assert_eq!((given.as_str(), latest.as_str()), (T1, T2))
        }
        other => panic!("signed before the latest: {other:?}"),
    }
    let latest = s.latest(&c.id()).unwrap().unwrap();
    assert_eq!((latest.tx.as_str(), latest.recorded.as_str()), (T2, T2), "the cell is unchanged");
    // Signed later (T3) on a clock frozen at T2's recording: accepted, recorded 1 µs after the latest.
    clock.set(T2);
    let later = s.put_receipt(&slice_receipt(&c, "12", T3, T3)).unwrap();
    s.put_version(&c.id(), T3, &later).expect("signed later: accepted");
    let latest = s.latest(&c.id()).unwrap().unwrap();
    assert_eq!((latest.receipt_hash.as_str(), latest.tx.as_str()), (later.as_str(), T3));
    assert_eq!(latest.recorded, T3, "max(signed tx T3, clock T2, latest recorded T2 + 1 µs)");
    // Frozen at T5 now: two later-signed versions are recorded T5 and T5 + 1 µs.
    clock.set(T5);
    for (v, tx) in [("13", "2024-01-03T12:00:00.000000Z"), ("14", T4)] {
        let r = s.put_receipt(&slice_receipt(&c, v, tx, tx)).unwrap();
        s.put_version(&c.id(), tx, &r).unwrap();
    }
    let recorded: Vec<String> = s.versions(&c.id(), T1, END).unwrap().iter().map(|v| v.recorded.clone()).collect();
    assert_eq!(recorded, [T2.to_string(), T3.to_string(), T5.to_string(), "2024-01-05T00:00:00.000001Z".to_string()]);
}

fn two_versions_in_one_batch_on_a_frozen_clock_are_recorded_one_microsecond_apart(b: Backend) {
    // B5 follow-up (decided 27 Nov): recorded = max(signed tx, clock, the cell's latest recorded +
    // 1 µs). Two versions of one cell in one put_cells batch, the clock frozen at T3: both stored, at
    // T3 and 1 µs after.
    let db = TestDb::migrated(b);
    let clock = Clock::at(T3);
    let s = rec_store(&db, &clock);
    let h = function(&s);
    let c = kcell(&h, 1);
    let (r1, r2) = (slice_receipt(&c, "1", T1, T1), slice_receipt(&c, "2", T2, T2));
    let id = c.id();
    s.put_cells(&[CellWrite::new(&r1, Some((id.as_str(), T1))), CellWrite::new(&r2, Some((id.as_str(), T2)))])
        .expect("both versions stored");
    let vs = s.versions(&id, T1, T5).unwrap();
    assert_eq!(vs.len(), 2);
    let mut recorded: Vec<String> = vs.iter().map(|v| v.recorded.clone()).collect();
    recorded.sort();
    assert_eq!(
        recorded,
        [T3.to_string(), "2024-01-03T00:00:00.000001Z".to_string()],
        "strictly increasing, 1 µs apart"
    );
    assert_eq!(s.latest(&id).unwrap().unwrap().tx, T2, "the second of the batch is the latest");
}

fn migration_17_gives_existing_rows_recorded_equal_to_tx(b: Backend) {
    let (old, new) = if b == Backend::Pg { (16, 17) } else { (12, 13) };
    let db = TestDb::empty(b);
    migrate_to(&db, b, old);
    let s = slice_store(&db);
    let h = function(&s);
    for k in 1..=3 {
        put_member(&s, &kcell(&h, k), &k.to_string(), [T1, T2, T3][k as usize - 1], T3);
    }
    let receipts = db.texts(db.pick(
        "SELECT hash, encode(bytes, 'hex') FROM receipts ORDER BY hash",
        "SELECT hash, hex(bytes) FROM receipts ORDER BY hash",
    ));
    drop(s);
    assert_eq!(apply_rest(&db, b), [new], "only the recorded-time migration is applied");
    assert_eq!(db.int("SELECT count(*) FROM versions WHERE recorded = tx"), 3, "existing rows: recorded = tx");
    assert_eq!(
        db.texts(db.pick(
            "SELECT hash, encode(bytes, 'hex') FROM receipts ORDER BY hash",
            "SELECT hash, hex(bytes) FROM receipts ORDER BY hash",
        )),
        receipts,
        "no receipt changes"
    );
    let s = db.store();
    assert_eq!(s.stored_asof(&kcell(&h, 2).id(), T3).unwrap().unwrap().recorded, T2);
}

// ---- G10: the slice definitions --------------------------------------------------------------

fn a_slice_definition_is_kept_once_per_slice(b: Backend) {
    let db = TestDb::migrated(b);
    let s = slice_store(&db);
    let h = function(&s);
    let sl = slice_f(&h);
    assert_eq!(s.slice_definition(sl.id()).unwrap(), None);
    assert!(s.put_slice_definition(&sl).unwrap(), "inserted");
    assert!(!s.put_slice_definition(&sl).unwrap(), "once: held already");
    assert_eq!(s.slice_definition(sl.id()).unwrap(), Some(sl.clone()));
    assert_eq!(db.count("slice_definitions"), 1);
    let row = &db.texts(db.pick(
        "SELECT slice_id, fn_hash, encode(slice, 'escape') FROM slice_definitions",
        "SELECT slice_id, fn_hash, CAST(slice AS TEXT) FROM slice_definitions",
    ))[0];
    assert_eq!((row[0].as_str(), row[1].as_str()), (sl.id(), h.as_str()));
    assert_eq!(row[2].as_bytes(), sl.canonical().as_slice(), "the canonical bytes");
    // Another slice of the same function (a range of k, not every k): a row of its own.
    let other = slice(&format!(
        r#"{{"args":{{"k":{{"hi":"5","lo":"1","type":"number"}}}},"fn":"f","fn_hash":"{h}","ns":"{SLICE_HOST}"}}"#
    ));
    assert_ne!(other.id(), sl.id());
    assert!(s.put_slice_definition(&other).unwrap());
    assert_eq!(db.count("slice_definitions"), 2);
    // Append-only.
    db.refused("DELETE FROM slice_definitions");
    db.refused(&format!("UPDATE slice_definitions SET fn_hash = '{}'", "0".repeat(64)));
    assert!(matches!(s.slice_definition("not-a-slice-id"), Err(datumesh_store::Error::Invalid(_))));
}

// ---- Migration helpers (as store_064_sort_key's) ---------------------------------------------

fn migrate_to(db: &TestDb, b: Backend, up_to: usize) {
    let old: Vec<&migrate::Migration> = match b {
        Backend::Pg => migrate::MIGRATIONS[..up_to].iter().collect(),
        Backend::Sqlite => migrate::sqlite::MIGRATIONS[..up_to].iter().collect(),
    };
    db.exec(db.pick(
        "CREATE TABLE IF NOT EXISTS datumesh_schema_migrations (version integer PRIMARY KEY, name text NOT NULL, checksum text NOT NULL, applied_at timestamptz NOT NULL DEFAULT now())",
        "CREATE TABLE IF NOT EXISTS datumesh_schema_migrations (version INTEGER PRIMARY KEY, name TEXT NOT NULL, checksum TEXT NOT NULL, applied_at TEXT NOT NULL DEFAULT (strftime('%Y-%m-%dT%H:%M:%fZ', 'now'))) STRICT",
    ))
    .unwrap();
    for m in &old {
        db.exec(m.sql).unwrap();
        db.exec(&format!(
            "INSERT INTO datumesh_schema_migrations (version, name, checksum) VALUES ({}, '{}', '{}')",
            m.version,
            m.name,
            m.checksum()
        ))
        .unwrap();
    }
}

fn apply_rest(db: &TestDb, b: Backend) -> Vec<u32> {
    match b {
        Backend::Pg => migrate::apply(&db.pg().bare_store()).unwrap(),
        Backend::Sqlite => migrate::sqlite::apply(&db.lite().store_with(SqliteOptions::new())).unwrap(),
    }
}

backends!(
    migration_17_and_13_are_the_last_and_named,
    without_a_clock_recorded_is_the_signed_tx,
    the_store_records_at_its_clock_and_keeps_the_signed_tx,
    recording_time_is_what_the_next_put_records,
    as_of_reads_select_by_recorded,
    the_backdating_rule_is_enforced_on_recorded,
    two_versions_in_one_batch_on_a_frozen_clock_are_recorded_one_microsecond_apart,
    migration_17_gives_existing_rows_recorded_equal_to_tx,
    a_slice_definition_is_kept_once_per_slice,
);
