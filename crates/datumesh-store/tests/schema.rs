//! The schema: every SQL statement the store uses prepares against the migrated schema
//! (there is no compile-time query checking with the sync driver); migrations, through a
//! `PgStore` on an empty database, apply from empty, re-run as a no-op, apply once when
//! concurrent, and `check` finds no drift on a migrated database but reports drift
//! introduced by hand; instants round-trip through `timestamptz` exactly.

mod common;

use std::sync::Arc;
use std::time::Duration;

use common::*;
use datumesh_store::{migrate, PgStore, PoolOptions, Resolver, Store};

#[test]
fn every_statement_prepares_against_the_migrated_schema() {
    let db = TestDb::migrated();
    let s = db.store();
    let n = s.check_statements().unwrap();
    assert!(n >= 20, "{n}");
}

#[test]
fn statements_do_not_prepare_without_the_schema() {
    let db = TestDb::empty();
    let s = db.bare_store();
    assert!(s.check_statements().is_err());
}

#[test]
fn migrations_apply_from_empty_and_rerun_as_no_op() {
    let db = TestDb::empty();
    let s = db.bare_store();
    let applied = migrate::apply(&s).unwrap();
    let all: Vec<u32> = migrate::MIGRATIONS.iter().map(|m| m.version).collect();
    assert_eq!(applied, all);
    assert!(!all.is_empty());
    let before = migrate::catalog(&s).unwrap();
    assert!(migrate::apply(&s).unwrap().is_empty());
    assert!(migrate::apply(&db.bare_store()).unwrap().is_empty());
    assert_eq!(migrate::catalog(&s).unwrap(), before);
    // The tracking table records each migration once, with its checksum.
    let n: i64 = db.client().query_one("SELECT count(*) FROM datumesh_schema_migrations", &[]).unwrap().get(0);
    assert_eq!(n as usize, all.len());
}

/// A store connects to an empty database without touching it (no statement is prepared,
/// nothing created), so the schema can be made through it, and the same store then works.
#[test]
fn a_store_on_an_empty_database_migrates_it_and_then_works() {
    let db = TestDb::empty();
    let s = PgStore::connect(&db.url()).unwrap();
    assert!(migrate::catalog(&s).unwrap().is_empty());
    let objects: i64 = db
        .client()
        .query_one(
            "SELECT count(*) FROM pg_class WHERE relnamespace = (SELECT oid FROM pg_namespace WHERE nspname = 'public')",
            &[],
        )
        .unwrap()
        .get(0);
    assert_eq!(objects, 0);
    assert!(!migrate::apply(&s).unwrap().is_empty());
    put_vector_keys(&s, &vector("receipt/stored-number"));
    let h = s.put_receipt(&input(&vector("receipt/stored-number"))).unwrap();
    assert!(s.receipt(&h).is_some());
    assert!(s.check_statements().unwrap() >= 20);
}

/// `apply` holds one pooled connection throughout (the advisory lock and the tracking
/// table's transactions are on one session): with a pool of one connection and a short
/// checkout timeout, a second checkout would time out. The session lock is released after.
#[test]
fn apply_holds_one_connection_and_releases_the_lock() {
    let db = TestDb::empty();
    let s = db.bare_store_with(PoolOptions::new().max_size(1).checkout_timeout(Duration::from_millis(500)));
    assert!(!migrate::apply(&s).unwrap().is_empty());
    assert!(migrate::apply(&s).unwrap().is_empty());
    assert!(!migrate::catalog(&s).unwrap().is_empty());
    let free: bool = db
        .client()
        .query_one("SELECT pg_try_advisory_lock(hashtextextended('datumesh-store:migrate', 0))", &[])
        .unwrap()
        .get(0);
    assert!(free);
}

#[test]
fn concurrent_migrations_apply_once() {
    // Threads sharing one store (each apply on its own pooled connection) and threads with
    // stores of their own.
    let db = TestDb::empty();
    let shared = Arc::new(db.bare_store_with(PoolOptions::new().max_size(4)));
    let mut handles = Vec::new();
    for i in 0..6 {
        let shared = shared.clone();
        let url = db.url();
        handles.push(std::thread::spawn(move || {
            if i % 2 == 0 {
                migrate::apply(&shared).unwrap()
            } else {
                migrate::apply(&PgStore::connect(&url).unwrap()).unwrap()
            }
        }));
    }
    let applied: Vec<Vec<u32>> = handles.into_iter().map(|h| h.join().unwrap()).collect();
    assert_eq!(applied.iter().filter(|a| !a.is_empty()).count(), 1, "{applied:?}");
}

#[test]
fn check_is_clean_after_migrating_and_finds_drift() {
    let live = TestDb::migrated();
    let scratch = TestDb::empty();
    let diff = migrate::check(&live.bare_store(), &scratch.bare_store()).unwrap();
    assert!(diff.is_empty(), "{diff:#?}");

    // Drift: a column added, an index dropped, a trigger disabled... each is reported.
    let cases = [
        ("ALTER TABLE receipts ADD COLUMN note text", "note"),
        ("DROP INDEX edges_input_hash", "edges_input_hash"),
        ("ALTER TABLE versions DISABLE TRIGGER versions_append_only", "versions_append_only"),
        ("ALTER TABLE blobs ALTER COLUMN media_type SET DEFAULT 'application/octet-stream'", "media_type"),
        ("CREATE TABLE extra (x int)", "extra"),
    ];
    for (sql, needle) in cases {
        let live = TestDb::migrated();
        live.client().batch_execute(sql).unwrap();
        let scratch = TestDb::empty();
        let diff = migrate::check(&live.bare_store(), &scratch.bare_store()).unwrap();
        assert!(!diff.is_empty(), "{sql}: no drift found");
        assert!(diff.iter().any(|l| l.contains(needle)), "{sql}: {diff:#?}");
    }
}

#[test]
fn check_refuses_a_scratch_database_that_is_not_empty() {
    let live = TestDb::migrated();
    let scratch = TestDb::migrated();
    assert!(migrate::check(&live.bare_store(), &scratch.bare_store()).is_err());
}

#[test]
fn an_edited_migration_is_refused() {
    let db = TestDb::migrated();
    db.client().batch_execute("UPDATE datumesh_schema_migrations SET checksum = 'x' WHERE version = 1").unwrap();
    let e = migrate::apply(&db.bare_store()).unwrap_err();
    assert!(e.to_string().contains("checksum"), "{e}");
}

/// Every instant of the specification's range round-trips through `timestamptz` exactly,
/// whatever the session's TimeZone and DateStyle, and `timestamptz` order equals the text
/// order under `COLLATE "C"`.
#[test]
fn instants_round_trip_through_timestamptz() {
    let db = TestDb::migrated();
    let mut c = db.client();
    // Days from 0001-01-01 to 9999-12-31 inclusive: 3652059. Sample deterministically
    // (splitmix64), plus edges: first and last instants, leap days, century years, the
    // Julian-Gregorian switch, the Unix epoch, and the ends of each day.
    let mut instants = vec![
        "0001-01-01T00:00:00.000000Z".to_string(),
        "9999-12-31T23:59:59.999999Z".to_string(),
        "0004-02-29T12:00:00.000001Z".to_string(),
        "1582-10-04T23:59:59.999999Z".to_string(),
        "1582-10-15T00:00:00.000000Z".to_string(),
        "1600-02-29T00:00:00.000000Z".to_string(),
        "1900-02-28T23:59:59.999999Z".to_string(),
        "1969-12-31T23:59:59.999999Z".to_string(),
        "1970-01-01T00:00:00.000000Z".to_string(),
        "2000-02-29T00:00:00.000000Z".to_string(),
        "2038-01-19T03:14:08.000000Z".to_string(),
        "0999-12-31T23:59:59.999999Z".to_string(),
        "1000-01-01T00:00:00.000000Z".to_string(),
    ];
    let mut state = 0x9e3779b97f4a7c15u64;
    let mut next = || {
        state = state.wrapping_add(0x9e3779b97f4a7c15);
        let mut z = state;
        z = (z ^ (z >> 30)).wrapping_mul(0xbf58476d1ce4e5b9);
        z = (z ^ (z >> 27)).wrapping_mul(0x94d049bb133111eb);
        z ^ (z >> 31)
    };
    for _ in 0..20_000 {
        let (y, m, d) = civil(1 + (next() % 3_652_059) as i64);
        let us = next() % 86_400_000_000;
        let (s, frac) = (us / 1_000_000, us % 1_000_000);
        instants.push(format!("{y:04}-{m:02}-{d:02}T{:02}:{:02}:{:02}.{frac:06}Z", s / 3600, (s / 60) % 60, s % 60));
    }
    for (tz, style) in [
        ("UTC", "ISO, YMD"),
        ("Asia/Kathmandu", "SQL, DMY"),
        ("America/St_Johns", "German, DMY"),
        ("Pacific/Kiritimati", "Postgres, MDY"),
        ("Europe/Amsterdam", "ISO, MDY"),
    ] {
        c.batch_execute(&format!("SET TIME ZONE '{tz}'; SET DateStyle = '{style}'")).unwrap();
        let rows = c
            .query(
                "SELECT t, to_char(t::timestamptz AT TIME ZONE 'UTC', 'YYYY-MM-DD\"T\"HH24:MI:SS.US\"Z\"')
                 FROM unnest($1::text[]) AS t",
                &[&instants],
            )
            .unwrap();
        for row in &rows {
            let (a, b): (String, String) = (row.get(0), row.get(1));
            assert_eq!(a, b, "{tz} {style}");
        }
        let bad: i64 = c
            .query_one(
                "SELECT count(*) FROM (SELECT row_number() OVER (ORDER BY t COLLATE \"C\") a,
                        row_number() OVER (ORDER BY t::timestamptz) b FROM unnest($1::text[]) AS t) x WHERE a <> b",
                &[&instants],
            )
            .unwrap()
            .get(0);
        assert_eq!(bad, 0, "{tz} {style}");
    }
}

/// The proleptic Gregorian date of day `n` (1 = 0001-01-01).
fn civil(n: i64) -> (i64, i64, i64) {
    // Howard Hinnant's days_from_civil inverse, with day 1 = 0001-01-01 = -719162 from 1970.
    let z = n - 1 - 719_162 + 719_468;
    let era = z.div_euclid(146_097);
    let doe = z - era * 146_097;
    let yoe = (doe - doe / 1460 + doe / 36_524 - doe / 146_096) / 365;
    let y = yoe + era * 400;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    let d = doy - (153 * mp + 2) / 5 + 1;
    let m = if mp < 10 { mp + 3 } else { mp - 9 };
    (if m <= 2 { y + 1 } else { y }, m, d)
}

#[test]
fn civil_dates_are_right() {
    assert_eq!(civil(1), (1, 1, 1));
    assert_eq!(civil(719_163), (1970, 1, 1));
    assert_eq!(civil(3_652_059), (9999, 12, 31));
}
